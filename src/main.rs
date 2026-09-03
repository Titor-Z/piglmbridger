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
use std::path::PathBuf;
use std::time::Duration;

use logger::Logger;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Config {
    /// 监听端口
    port: u16,
    /// 上游 API 地址
    upstream: String,
    /// 上游请求超时（秒）
    timeout_secs: u64,
    /// 日志目录
    log_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8123,
            upstream: "https://open.bigmodel.cn/api/paas/v4".into(),
            timeout_secs: 300,
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
    /// 启动代理（默认子命令）
    Serve {
        /// 覆盖监听端口
        #[arg(long)]
        port: Option<u16>,
        /// 覆盖上游地址
        #[arg(long)]
        upstream: Option<String>,
        /// 覆盖上游超时（秒）
        #[arg(long)]
        timeout: Option<u64>,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = Config::load();

    match cli.command.unwrap_or(Command::Serve {
        port: None,
        upstream: None,
        timeout: None,
    }) {
        Command::Logs { lines, follow } => {
            logger::view_logs(&config.log_dir.join("proxy.log"), lines, follow);
        }
        Command::Serve { port, upstream, timeout } => {
            let cfg = config;
            let port = port.unwrap_or(cfg.port);
            let upstream = upstream.unwrap_or(cfg.upstream);
            let timeout_secs = timeout.unwrap_or(cfg.timeout_secs);

            let logger = match Logger::new(&cfg.log_dir) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("⚠️  日志初始化失败: {e}，仅输出到终端");
                    Logger::disabled()
                }
            };
            logger.info(&format!("piglmbridger 启动 http://127.0.0.1:{port}"));
            eprintln!("piglmbridger listening on http://127.0.0.1:{port}");
            eprintln!("upstream: {upstream}");
            eprintln!("log file: {}", cfg.log_dir.join("proxy.log").display());
            eprintln!("另开终端可运行: piglmbridger logs --follow 实时查看日志");

            let state = AppState {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(timeout_secs))
                    .connect_timeout(Duration::from_secs(30))
                    .build()
                    .expect("failed to build http client"),
                upstream,
                logger,
            };

            let app = Router::new()
                .route("/chat/completions", post(passthrough))
                .fallback(passthrough)
                .with_state(state.clone());

            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
                .await
                .expect("failed to bind");
            axum::serve(listener, app).await.unwrap();
        }
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
}

impl SseNormalizer {
    fn new() -> Self {
        Self { buffer: Vec::new(), done_seen: false, dropped: 0, emitted: 0 }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();

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
                    break;
                }
                let trimmed = line.trim_end_matches('\r');
                if !trimmed.is_empty() {
                    if let Some(norm) = self.normalize_line(trimmed) {
                        lines.push(norm);
                    }
                }
            }
            if !ends_nl {
                break;
            }
        }
        lines
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

    let url = format!("{}{}", state.upstream.trim_end_matches('/'), uri.path());
    let mut req = state.client.post(&url).body(body);
    for (k, v) in headers.iter() {
        let name = k.as_str();
        if matches!(name, "host" | "content-length" | "connection" | "accept-encoding") {
            continue;
        }
        req = req.header(name, v);
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

            if content_type.contains("text/event-stream") {
                state.logger.info(&format!(
                    "[{req_id}] <- {status} SSE 流开始"
                ));
                let logger = state.logger.clone();
                let req_id2 = req_id.clone();
                let upstream_stream = resp.bytes_stream();
                let body_stream = futures_util::stream::unfold(
                    (upstream_stream, SseNormalizer::new(), false, logger, req_id2),
                    move |(mut stream, mut norm, mut done, logger, req_id)| async move {
                        if done {
                            return None;
                        }
                        loop {
                            match stream.next().await {
                                Some(Ok(chunk)) => {
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
                                            (stream, norm, done, logger, req_id),
                                        ));
                                    }
                                }
                                Some(Err(e)) => {
                                    logger.error(&format!("[{req_id}] 上游流错误: {e}"));
                                    return Some((
                                        Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                                        (stream, norm, true, logger, req_id),
                                    ));
                                }
                                None => {
                                    // 流到此结束。正常情况下内容行在各自换行时已即时下发，
                                    // done_seen=true（收到过 [DONE]）。若 buffer 里仍有残留，
                                    // 说明是上游异常切断的半截帧 → 丢弃，绝不伪造完整 frame。
                                    let info = norm.drain_abrupt();
                                    if let Some(m) = info {
                                        logger.error(&format!("[{req_id}] {m}"));
                                    }
                                    logger.info(&format!(
                                        "[{req_id}] SSE 流结束(done={})，规整下发 {} 行，丢弃 {} 个无效帧，耗时 {:.2}s",
                                        norm.done_seen,
                                        norm.emitted,
                                        norm.dropped,
                                        start.elapsed().as_secs_f32()
                                    ));
                                    return None;
                                }
                            }
                        }
                    },
                );

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
