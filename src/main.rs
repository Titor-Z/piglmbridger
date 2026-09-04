//! GLM-5.3-Flash SSE 分片修复中转代理
//!
//! 用法：
//!   piglmbridger serve [--port 8123] [--upstream URL] [--timeout SECS] [--config PATH]
//!   piglmbridger logs [--lines N] [--follow]
//!
//! 配置文件：~/.piglmbridger/config.toml（优先级：CLI 参数 > 配置文件 > 默认值）
//! 日志文件：~/.piglmbridger/logs/proxy.log

mod logger;

use clap::{Parser, Subcommand};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;

use logger::Logger;

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

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    upstream: String,
    logger: Logger,
    /// 在途请求数（优雅退出时等待收尾）
    inflight: Arc<std::sync::atomic::AtomicU64>,
    /// 进程级累计统计
    total_requests: Arc<std::sync::atomic::AtomicU64>,
    total_dropped: Arc<std::sync::atomic::AtomicU64>,
    /// stats.jsonl 路径（每请求一行，供 stats 子命令汇总）
    stats_path: PathBuf,
    /// 读空闲超时（秒），0=禁用
    idle_secs: u64,
    /// 远端令牌（空=不鉴权）
    auth_token: String,
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

/// 包裹下发流：Drop 时能感知"下游提前断开"（K05 盲区的观测面）。
/// 正常完成时 finished 已置位，不误报。
struct NotifyDrop<S> {
    inner: std::pin::Pin<Box<S>>,
    logger: Logger,
    req_id: String,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl<S: futures_util::Stream> futures_util::Stream for NotifyDrop<S> {
    type Item = S::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }
}

impl<S> Drop for NotifyDrop<S> {
    fn drop(&mut self) {
        if !self.finished.load(std::sync::atomic::Ordering::Relaxed) {
            self.logger.debug(&format!(
                "[{}] 客户端提前断开：body 被 cancel，上游请求已随之中止（取消透传生效）",
                self.req_id
            ));
        }
    }
}

impl<S> NotifyDrop<S> {
    fn new(inner: S, logger: Logger, req_id: String, finished: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { inner: Box::pin(inner), logger, req_id, finished }
    }
}

/// 逐字节喂入原始数据，产出规整后的 SSE 行。
struct SseNormalizer {
    buffer: Vec<u8>,
    done_seen: bool,
    /// 统计：丢弃的空帧数
    dropped: u64,
    /// 统计：成功下发的行数
    emitted: u64,
    /// 统计：跨块拼接后才发出的行数（分片缓冲命中）
    rejoined: u64,
    /// 上一轮是否留下了残尾（用于 rejoined 计数）
    had_partial: bool,
}

impl SseNormalizer {
    fn new() -> Self {
        Self { buffer: Vec::new(), done_seen: false, dropped: 0, emitted: 0, rejoined: 0, had_partial: false }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let is_continuation = self.had_partial;

        // 逐字节窗口处理；每轮回先把能从合法 UTF-8 边界切开的部分取走。
        while let Some(valid_len) = valid_utf8_prefix_len(&self.buffer) {
            let s = String::from_utf8_lossy(&self.buffer[..valid_len]).into_owned();
            self.buffer.drain(..valid_len);

            // s 是否以换行收尾，决定最后一个 split 片段是不是“完整行”。
            // GLM 上游经常在同一个网络块里塞多条完整 data 行 + 一个只写到一半的下一条
            // （形如 data:{…}\n\ndata:{…\n\ndata:{"id":…"} —— 末尾无换行）。
            // 只有以换行收尾的片段才算完整，残缺尾段必须放回 buffer 等下块拼接。
            let ends_nl = s.ends_with('\n');
            let mut parts = s.split('\n').peekable();
            while let Some(line) = parts.next() {
                let is_last = parts.peek().is_none();
                if is_last && !ends_nl && !line.is_empty() {
                    // 残缺尾行（不带换行收尾）：放回缓冲，绝不当下发。
                    let back = line.as_bytes().to_vec();
                    self.buffer.splice(0..0, back);
                    self.had_partial = true;
                    break;
                }
                let trimmed = line.trim_end_matches('\r');
                if !trimmed.is_empty() {
                    if is_continuation || self.had_partial {
                        self.rejoined += 1;
                    }
                    if let Some(norm) = self.normalize_line(trimmed) {
                        lines.push(norm);
                    }
                }
            }
            if self.buffer.is_empty() {
                self.had_partial = false;
            }
            if !ends_nl {
                break;
            }
        }
        lines
    }

    /// 带内错误帧：流异常时给下游一条标准 SSE error 事件 + [DONE]，
    /// 让 pi 立即走失败/重试路径，而不是干等到自身超时。
    /// 注意：这不是伪造数据帧（数据帧绝不伪造），是显式的失败信号。
    fn error_frame(&self, message: &str) -> Vec<u8> {
        let payload = json!({
            "error": {
                "message": message,
                "type": "proxy_upstream_error",
                "proxy": "piglmbridger",
            }
        });
        let out = format!("data: {payload}\n\ndata: [DONE]\n\n").into_bytes();
        out
    }

    fn normalize_line(&mut self, line: &str) -> Option<String> {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            if data == "[DONE]" {
                if self.done_seen {
                    self.dropped += 1;
                    return None;
                }
                self.done_seen = true;
                self.emitted += 1;
                return Some("data: [DONE]".to_string());
            }
            if data.is_empty() {
                self.dropped += 1;
                return None;
            }
            self.emitted += 1;
            return Some(format!("data: {data}"));
        }
        Some(line.to_string())
    }

    /// 异常中断时调用：缓冲区里若还残留一行，则说明该 SSE 帧没有以换行/空行收尾，
    /// 是上游把帧切半后丢弃了（GLM 的已知坑）。绝不能补 \n 伪造完整帧发给下游，
    /// 否则会产出 "Unterminated string in JSON"。这里直接丢弃并返回描述供记日志。
    fn drain_abrupt(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let leftover = String::from_utf8_lossy(&self.buffer).to_string();
        self.buffer.clear();

        // 判断是否是被截断的 data 残帧
        let looks_like_data = leftover.contains("data:") && !leftover.ends_with("\n");
        let msg = if looks_like_data {
            self.dropped += 1;
            format!(
                "流被上游切断：残留未终结的 data 残帧({}B)已丢弃: {}…",
                leftover.len(),
                truncate_mid(leftover.trim(), 100)
            )
        } else {
            format!(
                "流结束后缓冲区残留 {}B（非 data 残帧，已丢弃）",
                leftover.len()
            )
        };
        Some(msg)
    }
}

fn truncate_mid(s: &str, n: usize) -> String {
    if s.chars().count() <= n || n == 0 {
        return s.to_string();
    }
    let half = n / 2;
    let start: String = s.chars().take(half).collect();
    let end: String = s.chars().skip(s.chars().count() - half).collect();
    format!("{start}…{end}")
}

fn valid_utf8_prefix_len(buf: &[u8]) -> Option<usize> {
    match std::str::from_utf8(buf) {
        Ok(_) => Some(buf.len()),
        Err(e) => {
            let valid = e.valid_up_to();
            let tail: &[u8] = &buf[valid..];
            if tail.len() < 4 && tail.first().is_some_and(|b| b & 0b1100_0000 == 0b1100_0000) {
                Some(valid)
            } else {
                Some(valid + 1)
            }
        }
    }
}

async fn passthrough(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let start = std::time::Instant::now();
    let req_id: String = {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = write!(s, "{:06x}", rand_u24());
        s
    };

    state.inflight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    state.total_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let result = passthrough_inner(state.clone(), uri, headers, body, req_id.clone(), start).await;
    state.inflight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let line = json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "id": req_id,
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "status": result.status().as_u16(),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&state.stats_path) {
        use std::io::Write as _;
        let _ = writeln!(f, "{line}");
    }
    result
}

async fn passthrough_inner(
    state: AppState,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
    req_id: String,
    start: std::time::Instant,
) -> Response {
    // 远端令牌鉴权（auth_token 配置非空时启用）
    if !state.auth_token.is_empty() {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if provided != format!("Bearer {}", state.auth_token) {
            state.logger.error(&format!("[{req_id}] 令牌校验失败，拒绝转发"));
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": {"message": "invalid or missing proxy token", "type": "proxy_auth"}})),
            ).into_response();
        }
    }

    // 模型感知：仅 glm-5.3* 走 SSE 归一化，其余模型字节级直通
    let model = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()));
    let normalize = match &model {
        Some(m) => m.starts_with("glm-5.3"),
        None => false, // 解析失败按不透明字节处理
    };
    state.logger.debug(&format!("[{req_id}] model={:?} normalize={}", model, normalize));

    let url = format!("{}{}", state.upstream.trim_end_matches('/'), uri.path());
    let mut req = state.client.post(&url).body(body);
    // Header 白名单：只放行鉴权与内容协商相关头部，其余一律丢弃
    const ALLOWED: [&str; 4] = ["authorization", "content-type", "accept", "user-agent"];
    for (k, v) in headers.iter() {
        let name = k.as_str();
        if ALLOWED.contains(&name) {
            req = req.header(name, v);
        } else if !matches!(name, "host" | "content-length" | "connection" | "accept-encoding") {
            state.logger.debug(&format!("[{req_id}] 滤除头: {name}"));
        }
    }

    state.logger.info(&format!("[{req_id}] -> POST {} 转发至 {url}", uri.path()));

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let mut out_headers = HeaderMap::new();
            for (k, v) in resp.headers().iter() {
                let name = k.as_str();
                if matches!(name, "content-length" | "transfer-encoding" | "connection") {
                    continue;
                }
                out_headers.insert(k.clone(), v.clone());
            }

            let content_type = out_headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            if content_type.contains("text/event-stream") && normalize {
                state.logger.info(&format!(
                    "[{req_id}] <- {status} SSE 流开始"
                ));
                let logger = state.logger.clone();
                let req_id2 = req_id.clone();
                let idle = state.idle_secs;
                let upstream_stream = resp.bytes_stream();
                let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let finished_for_unfold = finished.clone();
                let finished_for_drop = finished.clone();
                let logger_for_drop = logger.clone();
                let req_id_for_drop = req_id.clone();
                let body_stream = NotifyDrop::new(
                    futures_util::stream::unfold(
                    (upstream_stream, SseNormalizer::new(), false, logger, req_id2, state.total_dropped.clone(), idle, finished_for_unfold),
                    move |(mut stream, mut norm, mut done, logger, req_id, total_dropped, idle, finished2)| async move {
                        if done {
                            return None;
                        }
                        loop {
                            // 读空闲看门狗：GLM 长思考可能几十秒无输出；0=禁用
                            let next = if idle > 0 {
                                tokio::time::timeout(Duration::from_secs(idle), stream.next()).await
                            } else {
                                Ok(stream.next().await)
                            };
                            match next {
                                Err(_elapsed) => {
                                    let m = format!("读空闲 {idle}s 无数据，主动中止上游（疑似链路静默断开）");
                                    logger.error(&format!("[{req_id}] {m}"));
                                    let _ = norm.drain_abrupt();
                                    total_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    // 带内错误帧：让 pi 立即判定失败，而不是干等到自身超时
                                    let out = norm.error_frame(&m);
                                    return Some((
                                        Ok::<_, std::io::Error>(bytes::Bytes::from(out)),
                                        (stream, norm, true, logger, req_id, total_dropped, idle, finished2),
                                    ));
                                }
                                Ok(Some(Ok(chunk))) => {
                                    let lines = norm.push(&chunk);
                                    if norm.done_seen {
                                        done = true;
                                    }
                                    if !lines.is_empty() {
                                        let mut out = Vec::new();
                                        for l in lines {
                                            out.extend_from_slice(l.as_bytes());
                                            out.push(b'\n');
                                            out.push(b'\n');
                                        }
                                        return Some((
                                            Ok::<_, std::io::Error>(bytes::Bytes::from(out)),
                                            (stream, norm, done, logger, req_id, total_dropped, idle, finished2),
                                        ));
                                    }
                                }
                                Ok(Some(Err(e))) => {
                                    logger.error(&format!("[{req_id}] 上游流错误: {e}"));
                                    return Some((
                                        Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                                        (stream, norm, true, logger, req_id, total_dropped, idle, finished2),
                                    ));
                                }
                                Ok(None) => {
                                    // 流到此结束。正常情况下内容行在各自换行时已即时下发，
                                    // done_seen=true（收到过 [DONE]）。若 buffer 里仍有残留，
                                    // 说明是上游异常切断的半截帧 → 丢弃，绝不伪造完整 frame。
                                    let info = norm.drain_abrupt();
                                    let mut abnormal = false;
                                    if let Some(m) = info {
                                        logger.error(&format!("[{req_id}] {m}"));
                                        total_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        abnormal = true;
                                    }
                                    // 带内错误帧：HTTP 已 200 无法改状态码，用标准 error 事件告知下游
                                    let mut out: Vec<u8> = Vec::new();
                                    if abnormal && !norm.done_seen {
                                        out.extend_from_slice(&norm.error_frame("upstream stream terminated prematurely"));
                                    }
                                    logger.info(&format!(
                                        "[{req_id}] SSE 流结束(done={})，规整下发 {} 行（跨块拼接 {} 行），丢弃 {} 个无效帧，耗时 {:.2}s",
                                        norm.done_seen,
                                        norm.emitted,
                                        norm.rejoined,
                                        norm.dropped,
                                        start.elapsed().as_secs_f32()
                                    ));
                                    finished2.store(true, std::sync::atomic::Ordering::Relaxed);
                                    if out.is_empty() {
                                        return None;
                                    }
                                    return Some((
                                        Ok::<_, std::io::Error>(bytes::Bytes::from(out)),
                                        (stream, norm, true, logger, req_id, total_dropped, idle, finished2),
                                    ));
                                }
                            }
                        }
                    },
                    ),
                    logger_for_drop,
                    req_id_for_drop,
                    finished_for_drop,
                );
                // 取消透传说明：客户端断开 → hyper drop 本 body → unfold 状态
                // （含 reqwest bytes_stream）一并 drop → 上游连接立即关闭。
                // 依赖 Rust drop 传播，无需显式 CancellationToken。

                let mut resp = Response::new(Body::from_stream(body_stream));
                *resp.status_mut() = status;
                if let Some(ct) = out_headers.remove("content-type") {
                    resp.headers_mut().insert("content-type", ct);
                } else {
                    resp.headers_mut().insert(
                        "content-type",
                        "text/event-stream".parse().unwrap(),
                    );
                }
                resp
            } else if content_type.contains("text/event-stream") {
                // 其他模型 / 未识别 body：SSE 字节级直通，零缓冲零过滤
                state.logger.info(&format!("[{req_id}] <- {status} SSE 直通（非 glm-5.3，不归一化）"));
                let mut resp = Response::new(Body::from_stream(resp.bytes_stream()));
                *resp.status_mut() = status;
                if let Some(ct) = out_headers.remove("content-type") {
                    resp.headers_mut().insert("content-type", ct);
                }
                resp
            } else {
                match resp.bytes().await {
                    Ok(bytes) => {
                        state.logger.info(&format!(
                            "[{req_id}] <- {status} 非SSE响应 {}B 耗时 {:.2}s",
                            bytes.len(),
                            start.elapsed().as_secs_f32()
                        ));
                        let mut resp = (status, bytes).into_response();
                        if let Some(ct) = out_headers.get("content-type") {
                            resp.headers_mut().insert("content-type", ct.clone());
                        }
                        resp
                    }
                    Err(e) => {
                        state.logger.error(&format!(
                            "[{req_id}] 上游读取错误: {e} 耗时 {:.2}s",
                            start.elapsed().as_secs_f32()
                        ));
                        (StatusCode::BAD_GATEWAY, format!("upstream read error: {e}"))
                            .into_response()
                    }
                }
            }
        }
        Err(e) => {
            state.logger.error(&format!(
                "[{req_id}] 上游连接失败: {e} 耗时 {:.2}s",
                start.elapsed().as_secs_f32()
            ));
            (StatusCode::BAD_GATEWAY, format!("upstream connect error: {e}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_complete_lines_and_joins_split_frames() {
        let mut n = SseNormalizer::new();
        let out1 = n.push(b"data: {\"role\": \"assistant\", \"content\": \"nihao shijie\"");
        assert!(out1.is_empty(), "no newline, should not emit yet");
        let out2 = n.push(b"}\n\ndata: [DONE]\n\n");
        assert_eq!(out2.len(), 2);
        assert_eq!(out2[0], "data: {\"role\": \"assistant\", \"content\": \"nihao shijie\"}");
        assert_eq!(out2[1], "data: [DONE]");
    }

    #[test]
    fn drops_abruptly_truncated_data_frame() {
        let mut n = SseNormalizer::new();
        let out1 = n.push(b"data: {\"role\": \"assistant\", \"content\": \"changg\n");
        assert_eq!(out1, vec!["data: {\"role\": \"assistant\", \"content\": \"changg"]);
        // simulate GLM chopping the next frame then dropping connection
        let out2 = n.push(b"data: {\"role\": \"tool\", \"arguments\": \"{\"q...");
        assert!(out2.is_empty());
        let dropped = n.drain_abrupt();
        assert!(dropped.is_some(), "should log the discarded frame");
        assert!(n.buffer.is_empty());
        assert_eq!(n.dropped, 1, "dropped count incremented");
        assert_eq!(n.emitted, 1, "already-complete line must be kept");
    }

    #[test]
    fn clean_end_after_done_has_no_leftover() {
        let mut n = SseNormalizer::new();
        let out1 = n.push(b"data: [DONE]\n\n");
        assert_eq!(out1, vec!["data: [DONE]"]);
        assert!(n.drain_abrupt().is_none());
        assert_eq!(n.dropped, 0);
    }

    #[test]
    fn glm_mixed_block_tail_completes_on_next_block() {
        let mut n = SseNormalizer::new();
        // 块1：完整行 + 残尾(无换行)
        let out1 = n.push("data: {\"role\":\"assistant\",\"content\":\"你好\"}\n\ndata: {\"tool_calls\":[".as_bytes());
        assert_eq!(out1, vec!["data: {\"role\":\"assistant\",\"content\":\"你好\"}"]);
        // 块2：把残尾补齐并带换行
        let out2 = n.push("{\"index\":0}]}\n\n".as_bytes());
        assert_eq!(out2, vec!["data: {\"tool_calls\":[{\"index\":0}]}"]);
        assert!(n.buffer.is_empty(), "拼完应无残留");
    }

    #[test]
    fn empty_tail_after_trailing_newline_drops_nothing() {
        // 整块就是一个完整 data + 空行收尾，不产生残尾，也不报错
        let mut n = SseNormalizer::new();
        let out = n.push("data: {\"x\":1}\n\n".as_bytes());
        assert_eq!(out, vec!["data: {\"x\":1}"]);
        assert!(n.buffer.is_empty());
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

fn rand_u24() -> u32 {
    // 轻量随机请求 id（无需额外依赖）
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x & 0xff_ffff
}
