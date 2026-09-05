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
use axum::{routing::post, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;
use std::path::PathBuf;
use std::time::Duration;

use piglmbridger::logger::Logger;
use state::AppState;
use proxy::passthrough;

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
                Err(e) => eprintln!("⚠️  配置文件解析失败 {}: {e}，使用默认配置", path.display()),
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
                eprintln!("ℹ️  已生成默认配置文件: {}", path.display());
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
enum Command {
    /// 启动代理（前台；默认子命令）
    Serve {
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
        /// 内部用：由 start 以守护进程方式拉起（隐藏）
        #[arg(long, hide = true)]
        daemon: bool,
    },
    /// 后台守护进程方式启动（进程名 piglmbridged）
    Start {
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// 停止守护进程（优雅退出，最长等 30s，超时 SIGKILL）
    Stop,
    /// 重启守护进程
    Restart {
        #[arg(long)]
        addr: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// 查看守护进程状态
    Status,
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
    /// 查看代理日志
    Logs {
        /// 只显示最后 N 行
        #[arg(long, default_value_t = 30)]
        lines: usize,
        /// 持续跟踪新日志（类似 tail -f）
        #[arg(short, long)]
        follow: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Config::load();

    match cli.command.unwrap_or(Command::Serve {
        addr: None,
        port: None,
        upstream: None,
        timeout: None,
        color: "auto".into(),
        log_level: "info".into(),
        daemon: false,
    }) {
        Command::Stats { days } => {
            print_stats(&config.log_dir.join("stats.jsonl"), days);
        }
        Command::Logs { lines, follow } => {
            logger::view_logs(&config.log_dir.join("proxy.log"), lines, follow);
        }
        Command::Start { addr, port } => daemon::start(addr, port),
        Command::Stop => daemon::stop(),
        Command::Restart { addr, port } => {
            daemon::stop();
            std::thread::sleep(Duration::from_millis(500));
            daemon::start(addr, port);
        }
        Command::Status => daemon::status(),
        Command::Doctor { api_key } => {
            doctor::run(&config, api_key).await;
        }
        Command::Serve { addr, port, upstream, timeout, color, log_level, daemon } => {
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
                    eprintln!("⚠️  日志初始化失败: {e}，仅输出到终端");
                    Logger::disabled()
                }
            };

            let bind = if addr.contains(':') {
                addr.clone()
            } else {
                format!("{addr}:{port}")
            };
            logger.info(&format!("piglmbridger{} 启动 http://{bind}", if daemon { "d" } else { "" }));
            if upstream.contains("bigmodel.cn") {
                logger.info("上游是国内站 open.bigmodel.cn —— 请确认你的 key 来自智谱国内平台（与 api.z.ai 国际站不互通；401 时先怀疑这一点）");
            }
            if !daemon {
                eprintln!("piglmbridger listening on http://{bind}");
                eprintln!("upstream: {upstream}");
                eprintln!("log file: {}", cfg.log_dir.join("proxy.log").display());
                eprintln!("另开终端可运行: piglmbridger logs --follow 实时查看日志");
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
                .fallback(passthrough)
                .with_state(state.clone());

            let listener = match tokio::net::TcpListener::bind(&bind).await {
                Ok(l) => l,
                Err(e) => {
                    let msg = format!(
                        "监听 {bind} 失败: {e}（端口被占用？试试 --port 换端口，或 piglmbridger status 查看已有实例）"
                    );
                    eprintln!("{msg}");
                    logger.error(&msg);
                    std::process::exit(1);
                }
            };

            let start_time = std::time::Instant::now();
            let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(state.clone()));
            if let Err(e) = server.await {
                logger.error(&format!("server error: {e}"));
            }
            // 收尾统计
            logger.info(&format!(
                "piglmbridger 退出：本次运行 {:.0}s，共 {} 个请求（其中上游残断 {} 次）",
                start_time.elapsed().as_secs_f32(),
                state.total_requests.load(std::sync::atomic::Ordering::Relaxed),
                state.total_dropped.load(std::sync::atomic::Ordering::Relaxed),
            ));
            if daemon {
                let _ = std::fs::remove_file(pid_file());
            }
        }
    }
}

/// 优雅退出信号：Ctrl+C 或 SIGTERM；日志提示在途流等待
async fn shutdown_signal(state: AppState) {
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
        state.logger.info(&format!(
            "收到退出信号，等待 {n} 个在途流收尾（最长 30s）…"
        ));
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
        state.logger.info("收到退出信号，无在途流，直接退出".into());
    }
}

fn print_stats(path: &PathBuf, days: u32) {
    use std::io::BufRead;
    if !path.exists() {
        eprintln!("暂无统计数据（{path:?} 不存在，先跑一些请求）");
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
        println!("所选时间范围内没有请求");
        return;
    }
    println!("piglmbridger 请求统计{}\n==================", if days > 0 { format!("（最近 {days} 天）") } else { Default::default() });
    println!("总请求:   {count}");
    println!("成功:     {ok} ({:.1}%)", ok as f64 * 100.0 / count as f64);
    println!("非2xx:    {err}");
    println!("平均耗时: {:.0}ms", total_ms as f64 / count as f64);
    println!("最大耗时: {max_ms}ms");
    println!("数据源:   {path:?}");
}


mod daemon {
    use super::*;

    fn is_alive(pid: u32) -> bool {
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

    fn read_pid() -> Option<u32> {
        let raw = std::fs::read_to_string(pid_file()).ok()?;
        raw.trim().parse().ok()
    }

    pub fn start(addr: Option<String>, port: Option<u16>) {
        // 已有实例？
        if let Some(pid) = read_pid() {
            if is_alive(pid) {
                eprintln!("❌ 已有实例在运行（pid {pid}）。如需重启：piglmbridger restart");
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
        if let Some(a) = addr {
            cmd.args(["--addr", &a]);
        }
        if let Some(p) = port {
            cmd.args(["--port", &p.to_string()]);
        }
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
                eprintln!("✅ piglmbridged 已后台启动 (pid {})，日志: {}", child.id(), Config::load().log_dir.join("proxy.log").display());
                eprintln!("   查看状态: piglmbridger status | 实时日志: piglmbridger logs -f");
            }
            Err(e) => {
                eprintln!("❌ 启动失败: {e}");
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
        let Some(pid) = read_pid() else {
            eprintln!("ℹ️  没有正在运行的 piglmbridged（无 pid 文件）");
            return;
        };
        if !is_alive(pid) {
            eprintln!("ℹ️  pid {pid} 已不存在，清理 stale pid 文件");
            let _ = std::fs::remove_file(pid_file());
            return;
        }
        eprint!("⏳ 向 piglmbridged (pid {pid}) 发送 SIGTERM，等待优雅退出…");
        #[cfg(unix)]
        let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        #[cfg(not(unix))]
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string()]).status();

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while is_alive(pid) {
            if std::time::Instant::now() > deadline {
                eprintln!("\n⚠️  30s 未退出，强制 SIGKILL");
                #[cfg(unix)]
                let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
            eprint!(".");
        }
        let _ = std::fs::remove_file(pid_file());
        eprintln!("\n✅ 已停止");
    }

    pub fn status() {
        match read_pid() {
            Some(pid) if is_alive(pid) => {
                eprintln!("✅ piglmbridged 运行中 (pid {pid})");
            }
            Some(pid) => {
                eprintln!("⚠️  pid 文件存在但进程 {pid} 已死（stale），运行 piglmbridger start 重新启动");
            }
            None => eprintln!("⛔ piglmbridged 未运行"),
        }
        let cfg = Config::load();
        let bind = format!("{}:{}", cfg.addr, cfg.port);
        match std::net::TcpStream::connect(&bind) {
            Ok(_) => eprintln!("✅ 端口 {bind} 可达"),
            Err(_) => eprintln!("⛔ 端口 {bind} 不可达（未监听）"),
        }
    }
}

mod doctor {
    use super::*;

    pub async fn run(cfg: &Config, api_key: Option<String>) {
        let mut ok = true;
        println!("piglmbridger doctor\n==================");

        // 1) 配置文件
        println!("\n[1/3] 配置文件: {:?}", Config::config_path());
        println!("      port={} addr={} timeout={}s", cfg.port, cfg.addr, cfg.timeout_secs);
        let log_ok = cfg.log_dir.is_dir() || std::fs::create_dir_all(&cfg.log_dir).is_ok();
        println!("      日志目录可写: {}", mark(log_ok));
        ok &= log_ok;

        // 2) 端口占用
        let bind = format!("{}:{}", cfg.addr, cfg.port);
        println!("\n[2/3] 监听端口 {bind}:");
        match std::net::TcpStream::connect(&bind) {
            Ok(_) => {
                println!("      已有服务在监听: {}（若非本代理请换端口）", mark(true));
                println!("      提示: piglmbridger status 可确认是否为本代理实例");
            }
            Err(_) => println!("      空闲: {}", mark(true)),
        }

        // 3) 上游连通性
        println!("\n[3/3] 上游 {}: ", cfg.upstream);
        let url = format!("{}/chat/completions", cfg.upstream.trim_end_matches('/'));
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                println!("      客户端构建失败: {e} {}", mark(false));
                return;
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
                    (200, _) => println!("      {st} ✓ 探活成功（key 有效，模型可用） {}", mark(true)),
                    (401, false) => println!("      {st} ✓ 网络可达（未带 key，被要求鉴权属预期） {}", mark(true)),
                    (401, true) => {
                        println!("      {st} ✗ key 被拒绝 {}", mark(false));
                        ok = false;
                    }
                    (s, _) => {
                        println!("      {s} ✗ 异常响应 {}", mark(false));
                        ok = false;
                    }
                }
            }
            Err(e) => {
                println!("      连接失败: {e} {}", mark(false));
                ok = false;
            }
        }

        println!("\n结论: {}", if ok { "✅ 全部通过" } else { "⚠️  有问题，见上" });
    }

    fn mark(b: bool) -> &'static str {
        if b { "✅" } else { "❌" }
    }
}



