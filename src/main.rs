//! GLM-5.3-Flash SSE 分片修复中转代理
//!
//! 用法：
//!   piglmbridger serve [--port 8123] [--upstream URL] [--timeout SECS] [--config PATH]
//!   piglmbridger logs [--lines N] [--follow]
//!
//! 配置文件：~/.piglmbridger/config.toml（优先级：CLI 参数 > 配置文件 > 默认值）
//! 日志文件：~/.piglmbridger/logs/proxy.log

use piglmbridger::{logger, proxy, state};

use clap::{Parser, Subcommand};
use axum::{
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::IsTerminal;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;
use std::path::PathBuf;
use std::time::Duration;

use piglmbridger::logger::Logger;
use state::AppState;
use proxy::{health, passthrough};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    /// 监听端口
    port: u16,
    /// 监听地址（IP），默认仅本机
    #[serde(default = "default_addr")]
    addr: String,
    /// 上游 API 地址
    upstream: String,
    /// 上游请求超时（秒）
    timeout_secs: u64,
    /// 读空闲超时（秒）：GLM 长思考期间无 token 输出的保护；0=禁用
    #[serde(default = "default_idle")]
    idle_timeout_secs: u64,
    /// 远端部署令牌（空=不鉴权）；非空时要求 Authorization: Bearer <token>
    #[serde(default)]
    auth_token: String,
    /// 日志目录
    log_dir: PathBuf,
}

fn default_addr() -> String {
    "127.0.0.1".into()
}

fn default_idle() -> u64 {
    120
}

fn pid_file() -> PathBuf {
    dirs_home().join(".piglmbridger").join("piglmbridged.pid")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8123,
            addr: "127.0.0.1".into(),
            upstream: "https://open.bigmodel.cn/api/paas/v4".into(),
            timeout_secs: 300,
            idle_timeout_secs: 120,
            auth_token: String::new(),
            log_dir: dirs_home().join(".piglmbridger").join("logs"),
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

impl Config {
    fn config_path() -> PathBuf {
        dirs_home().join(".piglmbridger").join("config.toml")
    }

    /// 读取配置文件；不存在则创建默认配置
    fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            match toml::from_str::<Config>(&raw) {
                Ok(cfg) => return cfg,
                Err(e) => eprintln!("{}", status_line(std::io::stdout().is_terminal(), Some(false), &format!("配置文件解析失败 {}: {e}，使用默认配置", path.display()))),
            }
        }
        let cfg = Config::default();
        // 尝试写出默认配置文件，方便用户后续修改
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match toml::to_string_pretty(&cfg) {
            Ok(s) => {
                let _ = std::fs::write(&path, format!(
                    "# piglmbridger 配置文件（优先级：CLI 参数 > 此文件 > 内置默认值）\n{s}"
                ));
                eprintln!("{}", status_line(std::io::stdout().is_terminal(), None, &format!("已生成默认配置文件: {}", path.display())));
            }
            Err(_) => {}
        }
        cfg
    }
}

#[derive(Parser)]
#[command(name = "piglmbridger", version, about = "GLM-5.3-Flash SSE 分片修复中转代理")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// 启动服务
    Start {
        /// 后台守护进程方式启动
        #[arg(short = 'd', long)]
        daemon: bool,
        /// 覆盖监听地址（IP，如 0.0.0.0）
        #[arg(long)]
        addr: Option<String>,
        /// 覆盖监听端口
        #[arg(long)]
        port: Option<u16>,
        /// 覆盖上游地址
        #[arg(long)]
        upstream: Option<String>,
        /// 覆盖上游超时（秒）
        #[arg(long)]
        timeout: Option<u64>,
        /// 日志着色：auto | always | never
        #[arg(long, default_value = "auto")]
        color: String,
        /// 日志等级：info | debug
        #[arg(long, default_value = "info")]
        log_level: String,
    },
    /// 停止后台服务（优雅退出，最长等 30s，超时 SIGKILL）
    Stop,
    /// 重启后台服务
    Restart {
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// 查看服务状态
    Status,
}

#[derive(Subcommand)]
enum Command {
    /// 服务生命周期：start / stop / restart / status
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// 内部：由 service start -d 以守护进程方式拉起（隐藏）
    #[command(hide = true)]
    Serve {
        #[arg(long, hide = true)]
        daemon: bool,
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value = "auto")]
        color: String,
        #[arg(long, default_value = "info")]
        log_level: String,
    },
    /// 体检：配置校验 / 端口占用 / 上游连通性
    Doctor {
        /// 可选：用于真实探活的智谱 API Key（不传则仅连通性检查）
        #[arg(long)]
        api_key: Option<String>,
    },
    /// 汇总请求统计（读取 stats.jsonl）
    Stats {
        /// 只统计最近 N 天（0 = 全部）
        #[arg(long, default_value_t = 0)]
        days: u32,
    },
    /// 查看代理日志（最后 N 条；-f 从本次启动处开始实时跟踪）
    #[command(alias = "log")]
    Logs {
        /// 只显示最后 N 行
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// 先回放本次会话日志，再持续跟踪新内容（类似 tail -f）
        #[arg(short, long)]
        follow: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Config::load();

    match cli.command {
        // 默认（无子命令）：只打印 help，不隐式启动任何东西
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
        }
        Some(Command::Service { action }) => match action {
            ServiceAction::Start { daemon, addr, port, upstream, timeout, color, log_level } => {
                if daemon {
                    daemon::start(addr, port);
                } else {
                    run_serve(config, addr, port, upstream, timeout, color, log_level, false).await;
                }
            }
            ServiceAction::Stop => daemon::stop(),
            ServiceAction::Restart { addr, port } => {
                daemon::stop();
                std::thread::sleep(Duration::from_millis(500));
                daemon::start(addr, port);
            }
            ServiceAction::Status => daemon::status(),
        },
        Some(Command::Serve { daemon, addr, port, color, log_level }) => {
            run_serve(config, addr, port, None, None, color, log_level, daemon).await;
        }
        Some(Command::Doctor { api_key }) => {
            doctor::run(&config, api_key).await;
        }
        Some(Command::Stats { days }) => {
            print_stats(&config.log_dir.join("stats.jsonl"), days);
        }
        Some(Command::Logs { lines, follow }) => {
            logger::view_logs(&config.log_dir.join("proxy.log"), lines, follow);
        }
    }
}

/// serve 主流程（前台由 service start 进入，后台由 -d 拉起的隐藏 serve --daemon 进入）
async fn run_serve(
    config: Config,
    addr: Option<String>,
    port: Option<u16>,
    upstream: Option<String>,
    timeout: Option<u64>,
    color: String,
    log_level: String,
    daemon: bool,
) {
    {
        let cfg = config;
            let addr = addr.unwrap_or(cfg.addr.clone());
            let port = port.unwrap_or(cfg.port);
            let upstream = upstream.unwrap_or(cfg.upstream);
            let timeout_secs = timeout.unwrap_or(cfg.timeout_secs);
            let color_mode = match color.as_str() {
                "always" => logger::ColorMode::Always,
                "never" => logger::ColorMode::Never,
                _ => logger::ColorMode::Auto,
            };

            let debug_on = log_level == "debug";
            let logger = match Logger::new_with_mode(&cfg.log_dir, color_mode, daemon, debug_on) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("{}", status_line(std::io::stdout().is_terminal(), Some(false), &format!("日志初始化失败: {e}，仅输出到终端")));
                    Logger::disabled()
                }
            };

            let bind = if addr.contains(':') {
                addr.clone()
            } else {
                format!("{addr}:{port}")
            };
            // TTY 彩色判定：横幅与退出摘要共用（Auto=stdout 是 TTY；Always 强制；Never 纯文本）
            let tty = matches!(color_mode, logger::ColorMode::Always)
                || (matches!(color_mode, logger::ColorMode::Auto) && std::io::stdout().is_terminal());
            if !daemon {
                // 启动横幅：仅前台 + TTY/强制彩带 ANSI，管道纯文本（K09 纪律：零新增依赖，手工 ANSI）
                let (dim, cyan, blue, reset) = if tty {
                    ("\x1b[2m", "\x1b[36m", "\x1b[34m", "\x1b[0m")
                } else {
                    ("", "", "", "")
                };
                let label = |s: &str| format!("{dim}{s:<14}{reset}");
                let bullet = |c: &str| if tty { format!("{c}●{reset}") } else { "●".to_string() };
                eprintln!();
                eprintln!("{} {}{cyan}{bind}{reset}", bullet(cyan), label("Listening on"));
                let note = if upstream.contains("bigmodel.cn") { " (国内站，确认 key 匹配)" } else { "" };
                eprintln!("{} {}{blue}{upstream}{reset}{dim}{note}{reset}", bullet(blue), label("Upstream"));
                eprintln!("{} {}{}", bullet(dim), label("Log file"), cfg.log_dir.join("proxy.log").display());
                eprintln!("{} {}piglmbridger log --follow", bullet(dim), label("Follow logs"));
                eprintln!(); // 与后续日志隔开
            }

            let state = AppState {
                active_streams: Arc::new(Mutex::new(HashMap::new())),
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(timeout_secs))
                    .connect_timeout(Duration::from_secs(30))
                    .build()
                    .expect("failed to build http client"),
                upstream,
                logger: logger.clone(),
                inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                total_requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                total_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                stats_path: cfg.log_dir.join("stats.jsonl"),
                idle_secs: cfg.idle_timeout_secs,
                auth_token: cfg.auth_token.clone(),
            };

            if daemon {
                // 守护进程模式：写 pid 文件
                let _ = std::fs::write(pid_file(), std::process::id().to_string());
            }

            let app = Router::new()
                .route("/chat/completions", post(passthrough))
                .route("/health", get(health))
                .fallback(passthrough)
                .with_state(state.clone());

            let listener = match tokio::net::TcpListener::bind(&bind).await {
                Ok(l) => l,
                Err(e) => {
                    let msg = format!(
                        "监听 {bind} 失败: {e}（端口被占用？试试 --port 换端口，或 piglmbridger service status 查看已有实例）"
                    );
                    eprintln!("{msg}");
                    logger.error(&msg);
                    std::process::exit(1);
                }
            };
            // 会话标记（仅落盘，终端不可见）：log -f 据此从本次启动处开始回放
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            logger.marker(&format!("###### {ts} 启动 http://{bind} ######"));

            let start_time = std::time::Instant::now();
            let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(state.clone(), !daemon, tty));
            if let Err(e) = server.await {
                logger.error(&format!("server error: {e}"));
            }
            // 收尾统计：前台自绘样式行（与启动横幅同语言），文件保留纯文本供 grep；daemon 只落盘
            let summary = format!(
                "退出 · 运行 {} · {} 个请求 · 上游残断 {}",
                logger::fmt_duration(start_time.elapsed()),
                state.total_requests.load(std::sync::atomic::Ordering::Relaxed),
                state.total_dropped.load(std::sync::atomic::Ordering::Relaxed),
            );
            if !daemon {
                eprintln!("\n{}", bullet_line(tty, "\x1b[36m", &summary));
            }
            logger.info_file(&format!("piglmbridger {}", summary));
            if daemon {
                let _ = std::fs::remove_file(pid_file());
            }
    }
}

/// 优雅退出信号：Ctrl+C 或 SIGTERM；前台终端自绘样式，文件/daemon 走 logger
async fn shutdown_signal(state: AppState, foreground: bool, tty: bool) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    let n = state.inflight.load(std::sync::atomic::Ordering::Relaxed);
    if n > 0 {
        let msg = format!("收到退出信号，等待 {n} 个在途流收尾（最长 30s）…");
        if foreground {
            eprintln!("\n{}", bullet_line(tty, "\x1b[1;33m", &msg));
            state.logger.info_file(&msg);
        } else {
            state.logger.info(&msg);
        }
        // 等在途流归零，最长 30s
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while state.inflight.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            if std::time::Instant::now() > deadline {
                state.logger.warn("等待超时，强制退出（在途流可能被中断）".to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    } else {
        // 前台不打垚话：摘要行马上就到；文件里留一笔
        if foreground {
            state.logger.info_file("收到退出信号，无在途流，直接退出");
        } else {
            state.logger.info("收到退出信号，无在途流，直接退出");
        }
    }
}

/// 横幅/退出共用的样式行：等宽 ● + 颜色（tty=false 时纯文本，无控制符）
fn bullet_line(tty: bool, color: &str, text: &str) -> String {
    if tty {
        format!("{color}●\x1b[0m {text}")
    } else {
        format!("● {text}")
    }
}

/// 状态行统一符号家族：Some(绿✓)=成功/健康 Some(红✗)=失败/异常 None(黄!)=中性状态/提示
/// （D18：全库 emoji 清零后唯一的状态行体系；非 TTY 纯文本）
fn status_line(tty: bool, good: Option<bool>, text: &str) -> String {
    let (sym, color) = match good {
        Some(true) => ("✓", "\x1b[1;32m"),
        Some(false) => ("✗", "\x1b[1;31m"),
        None => ("!", "\x1b[1;33m"),
    };
    if tty {
        format!("{color}{sym}\x1b[0m {text}")
    } else {
        format!("{sym} {text}")
    }
}

fn print_stats(path: &PathBuf, days: u32) {
    use std::io::BufRead;
    if !path.exists() {
        eprintln!("● Stats · 暂无数据（{} 不存在，先跑一些请求）", path.display());
        return;
    }
    let file = std::fs::File::open(path).expect("open stats");
    let cutoff = if days > 0 {
        Some(chrono::Utc::now() - chrono::Duration::days(days as i64))
    } else {
        None
    };
    let mut count = 0u64;
    let mut ok = 0u64;
    let mut err = 0u64;
    let mut total_ms = 0u128;
    let mut max_ms: u128 = 0;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(_c) = cutoff {
            let ts = v["ts"].as_str().unwrap_or("");
            match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(t) if t < cutoff.unwrap() => continue,
                _ => {}
            }
        }
        count += 1;
        let st = v["status"].as_u64().unwrap_or(0);
        if (200..300).contains(&st) { ok += 1 } else { err += 1 }
        let ms = v["elapsed_ms"].as_u64().unwrap_or(0) as u128;
        total_ms += ms;
        max_ms = max_ms.max(ms);
    }
    if count == 0 {
        eprintln!("● Stats{} · 所选时间范围内没有请求", if days > 0 { format!(" · 最近 {days} 天") } else { Default::default() });
        return;
    }
    let tty = std::io::stdout().is_terminal();
    let (dim, cyan, green, red, reset) = if tty {
        ("\x1b[2m", "\x1b[36m", "\x1b[1;32m", "\x1b[1;31m", "\x1b[0m")
    } else {
        ("", "", "", "", "")
    };
    println!();
    println!("{}{}", bullet_line(tty, "\x1b[36m", "Stats"), if days > 0 { format!("{dim} · 最近 {days} 天{reset}") } else { Default::default() });
    let avg = logger::fmt_duration(Duration::from_millis((total_ms / count as u128) as u64));
    let max = logger::fmt_duration(Duration::from_millis(max_ms as u64));
    // 面板里 + 前缀是噪音（那是日志耗时偏移的语义）
    let (avg, max) = (avg.trim_start_matches('+'), max.trim_start_matches('+'));
    // 终端显示宽度：CJK 记 2、ASCII 记 1（{:.<N$} 按字符数填，中文会错位）
    let disp_w = |s: &str| s.chars().map(|c| if c as u32 > 0x2E80 { 2 } else { 1 }).sum::<usize>();
    let row = |label: &str, val: &str| {
        let pad = " ".repeat(11 - disp_w(label));
        println!("{dim}{label}{reset}{pad}{val}");
    };
    row("总请求", &format!("{cyan}{count}{reset}"));
    row("成功", &format!("{green}{ok} ({:.1}%){reset}", ok as f64 * 100.0 / count as f64));
    row("非2xx", &format!("{red}{err}{reset}"));
    row("平均耗时", &format!("{cyan}{avg}{reset}"));
    row("最大耗时", &format!("{cyan}{max}{reset}"));
    row("数据源", &format!("{dim}{}{reset}", path.display()));
    println!();
}


mod daemon {
    use super::*;

    pub(super) fn is_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // kill -0 探活
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }
    }

    pub(super) fn read_pid() -> Option<u32> {
        let raw = std::fs::read_to_string(pid_file()).ok()?;
        raw.trim().parse().ok()
    }

    pub fn start(addr: Option<String>, port: Option<u16>) {
        // 已有实例？
        if let Some(pid) = read_pid() {
            if is_alive(pid) {
                let tty = std::io::stdout().is_terminal();
                eprintln!("{}", status_line(tty, Some(false), &format!("已有实例在运行 (pid {pid})")));
                eprintln!("{}", status_line(tty, None, "如需重启：piglmbridger service restart"));
                std::process::exit(1);
            }
            let _ = std::fs::remove_file(pid_file()); // stale pid 清理
        }

        let exe = std::env::current_exe().expect("cannot locate current exe");
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["serve", "--daemon"]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.arg0("piglmbridged"); // 进程名呈 piglmbridged
        }
        if let Some(a) = &addr {
            cmd.args(["--addr", a]);
        }
        if let Some(p) = port {
            cmd.args(["--port", &p.to_string()]);
        }

        // 启动横幅（父进程仍连着终端）：与前台同语言，TTY 彩色 / 管道纯文本
        let cfg = Config::load();
        let a = addr.clone().unwrap_or_else(|| cfg.addr.clone());
        let p = port.unwrap_or(cfg.port);
        let bind = if a.contains(':') { a } else { format!("{a}:{p}") };
        let tty = std::io::stdout().is_terminal();
        let (dim, cyan, blue, reset) = if tty {
            ("\x1b[2m", "\x1b[36m", "\x1b[34m", "\x1b[0m")
        } else {
            ("", "", "", "")
        };
        let label = |s: &str| format!("{dim}{s:<14}{reset}");
        let bullet = |c: &str| if tty { format!("{c}●{reset}") } else { "●".to_string() };
        eprintln!();
        eprintln!("{} {}{cyan}{bind}{reset}", bullet(cyan), label("Listening on"));
        let note = if cfg.upstream.contains("bigmodel.cn") { " (国内站，确认 key 匹配)" } else { "" };
        eprintln!("{} {}{blue}{}{reset}{dim}{note}{reset}", bullet(blue), label("Upstream"), cfg.upstream);
        eprintln!("{} {}{}", bullet(dim), label("Log file"), cfg.log_dir.join("proxy.log").display());
        eprintln!("{} {}piglmbridger log --follow", bullet(dim), label("Follow logs"));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 完全脱离终端会话
            unsafe {
                cmd.pre_exec(|| {
                    libc_setsid();
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            cmd.creation_flags(DETACHED_PROCESS);
        }

        match cmd.spawn() {
            Ok(child) => {
                eprintln!("{} 已后台启动 (pid {})", bullet(cyan), child.id());
            }
            Err(e) => {
                eprintln!("{} 启动失败: {e}", bullet("\x1b[1;31m"));
                std::process::exit(1);
            }
        }
    }

    #[cfg(unix)]
    fn libc_setsid() {
        // setsid(2)：脱离控制终端；直接用系统调用，避免引入 libc crate
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        unsafe {
            setsid();
        }
    }

    pub fn stop() {
        let tty = std::io::stdout().is_terminal();
        let Some(pid) = read_pid() else {
            eprintln!("{}", status_line(tty, None, "没有正在运行的服务（无 pid 文件）"));
            return;
        };
        if !is_alive(pid) {
            eprintln!("{}", status_line(tty, None, &format!("pid {pid} 已不存在，清理 pid 文件")));
            let _ = std::fs::remove_file(pid_file());
            return;
        }
        eprint!("{}", if tty { format!("\x1b[2m向 piglmbridged (pid {pid}) 发送 SIGTERM，等待优雅退出…\x1b[0m") } else { format!("向 piglmbridged (pid {pid}) 发送 SIGTERM，等待优雅退出…") });
        #[cfg(unix)]
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        #[cfg(not(unix))]
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string()]).status();

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while is_alive(pid) {
            if std::time::Instant::now() > deadline {
                eprintln!("\n{}", status_line(tty, Some(false), "30s 未退出，强制 SIGKILL"));
                #[cfg(unix)]
                let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
            eprint!(".");
        }
        let _ = std::fs::remove_file(pid_file());
        eprintln!("\n{}", status_line(tty, Some(true), "已停止"));
    }

    pub fn status() {
        let tty = std::io::stdout().is_terminal();
        match read_pid() {
            Some(pid) if is_alive(pid) => {
                eprintln!("{}", status_line(tty, Some(true), &format!("服务运行中 (pid {pid})")));
            }
            Some(pid) => {
                eprintln!("{}", status_line(tty, None, &format!("pid {pid} 已死（stale），运行 piglmbridger service start 重新启动")));
            }
            None => eprintln!("{}", status_line(tty, None, "服务未运行（无 pid 文件）")),
        }
        let cfg = Config::load();
        let bind = format!("{}:{}", cfg.addr, cfg.port);
        match std::net::TcpStream::connect(&bind) {
            Ok(_) => eprintln!("{}", status_line(tty, Some(true), &format!("端口 {bind} 可达"))),
            Err(_) => eprintln!("{}", status_line(tty, Some(false), &format!("端口 {bind} 不可达（未监听）"))),
        }
    }
}

mod doctor {
    use super::*;

    /// doctor 体检：扁平清单，一行一结论；结论由退出码承载（人看 ✓/✗，脚本看 $?）
    pub async fn run(cfg: &Config, api_key: Option<String>) {
        let tty = std::io::stdout().is_terminal();
        let mut ok = true;

        // 主行：{绿✓/红✗}  text（非 TTY 纯文本，零控制符）
        let ok_line = |good: bool, text: &str| {
            if tty {
                if good {
                    println!("\x1b[1;32m✓\x1b[0m  {text}");
                } else {
                    println!("\x1b[1;31m✗\x1b[0m  {text}");
                }
            } else {
                println!("{}  {text}", if good { "✓" } else { "✗" });
            }
        };
        // 附注行：缩进 + {黄!} + 淡色正文
        let note_line = |text: &str| {
            if tty {
                println!("   \x1b[1;33m!\x1b[0m \x1b[2m{text}\x1b[0m");
            } else {
                println!("   ! {text}");
            }
        };

        // 1) 配置文件
        let log_ok = cfg.log_dir.is_dir() || std::fs::create_dir_all(&cfg.log_dir).is_ok();
        ok &= log_ok;
        if log_ok {
            ok_line(true, &format!("配置文件 {}", Config::config_path().display()));
            note_line(&format!("port={} · addr={} · timeout={}s", cfg.port, cfg.addr, cfg.timeout_secs));
        } else {
            ok_line(false, &format!("配置文件 {}", Config::config_path().display()));
            note_line(&format!("日志目录不可写: {}", cfg.log_dir.display()));
        }

        // 2) 端口：区分“本代理运行中”（健康）与“被其他进程占用”（问题）
        let bind = format!("{}:{}", cfg.addr, cfg.port);
        match std::net::TcpStream::connect(&bind) {
            Err(_) => ok_line(true, &format!("端口 {bind} 空闲")),
            Ok(_) => match daemon::read_pid() {
                Some(pid) if daemon::is_alive(pid) => {
                    ok_line(true, &format!("端口 {bind} 本代理运行中 (pid {pid})"));
                    note_line("如需重新监听：piglmbridger service restart");
                }
                _ => {
                    ok_line(false, &format!("端口 {bind} 已被其他进程占用"));
                    note_line("代理将无法监听；若是残留进程请结束它，或换端口（--port / config.toml）");
                    ok = false;
                }
            },
        }

        // 3) 上游连通性
        let url = format!("{}/chat/completions", cfg.upstream.trim_end_matches('/'));
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                ok_line(false, &format!("上游 {} 客户端构建失败: {e}", cfg.upstream));
                std::process::exit(1);
            }
        };
        let mut req = client.post(&url).json(&json!({
            "model": "glm-5.3-flash",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
        }));
        if let Some(k) = &api_key {
            req = req.bearer_auth(k);
        }
        match req.send().await {
            Ok(resp) => {
                let st = resp.status().as_u16();
                match (st, api_key.is_some()) {
                    (200, _) => {
                        ok_line(true, &format!("{st} {} 上游可达", cfg.upstream));
                        note_line("key 有效，模型可用");
                    }
                    (401, false) => {
                        ok_line(true, &format!("{st} {} 上游可达", cfg.upstream));
                        note_line("未带 key，被要求鉴权属预期");
                    }
                    (401, true) => {
                        ok_line(false, &format!("{st} {} 上游可达", cfg.upstream));
                        note_line("key 被拒绝");
                        ok = false;
                    }
                    (s, _) => {
                        ok_line(false, &format!("{s} {} 上游异常响应", cfg.upstream));
                        ok = false;
                    }
                }
            }
            Err(e) => {
                ok_line(false, &format!("上游 {} 连接失败: {e}", cfg.upstream));
                ok = false;
            }
        }

        // 无结论行：✗ 即结论，! 即原因；脚本看退出码
        if !ok {
            std::process::exit(1);
        }
    }
}
