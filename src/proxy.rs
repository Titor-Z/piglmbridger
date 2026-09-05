//! 转发核心模块：/chat/completions 的请求鉴权、模型感知路由、SSE 归一化流循环。
//!
//! 职责边界（高内聚）：
//! - `passthrough`：axum handler 入口（req_id 生成、inflight/stats 计数）
//! - `passthrough_inner`：鉴权 → 模型感知（仅 glm-5.3* 归一化）→ Header 白名单 → 上游转发
//! - `StreamState`：SSE 流循环的可变状态（收敛原 14 项 unfold 元组）
//!
//! 取消透传说明：客户端断开 → hyper drop 本 body → unfold 状态（含 reqwest
//! bytes_stream）一并 drop → 上游连接立即关闭。依赖 Rust drop 传播。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::json;

use crate::logger::Logger;
use crate::state::{cleanup_stream, refresh_status, stream_status_text, AppState, NotifyDrop, SPINNER};
use crate::stream::SseNormalizer;

/// axum handler 入口：生成 req_id、维护 inflight/total 计数、落 stats.jsonl
pub async fn passthrough(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let start = Instant::now();
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

/// 转发主流程：鉴权 → 模型感知 → Header 白名单 → 上游请求 → SSE 归一化/直通
async fn passthrough_inner(
    state: AppState,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
    req_id: String,
    start: Instant,
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
    let req_bytes = body.len() as u64;
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

    state.logger.start_request(&req_id, model.as_deref(), "POST", uri.path(), &url, req_bytes);

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
                // 不打"SSE 流开始"行——传输期间由原位动画报告状态，
                // 真·首包时间（第一个上游 chunk，非响应头）在结束行给出。
                let streams = state.active_streams.clone();
                streams.lock().unwrap().insert(req_id.clone(), 0);
                let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let body_stream = NotifyDrop::new(
                    futures_util::stream::unfold(
                        StreamState::new(resp, status.as_u16(), state.clone(), req_id.clone(), req_bytes, start),
                        move |mut s| async move {
                            let item = s.step().await;
                            item.map(|i| (i, s))
                        },
                    ),
                    state.logger.clone(),
                    req_id.clone(),
                    finished.clone(),
                    streams.clone(),
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
            } else if content_type.contains("text/event-stream") {
                // 其他模型 / 未识别 body：SSE 字节级直通，零缓冲零过滤
                state.logger.debug(&format!("[{req_id}] SSE 字节级直通（非 glm-5.3，不归一化）"));
                state.logger.finish_request(
                    &req_id, status.as_u16(), true, Some(start.elapsed()), None, None, Some(req_bytes), None, "",
                );
                refresh_status(&state.logger, &state.active_streams);
                let mut resp = Response::new(Body::from_stream(resp.bytes_stream()));
                *resp.status_mut() = status;
                if let Some(ct) = out_headers.remove("content-type") {
                    resp.headers_mut().insert("content-type", ct);
                }
                resp
            } else {
                match resp.bytes().await {
                    Ok(bytes) => {
                        state.logger.finish_request(
                            &req_id, status.as_u16(), status.is_success(), Some(start.elapsed()),
                            None, None, Some(req_bytes), Some(bytes.len() as u64),
                            if status.is_success() { "" } else { "上游拒绝" },
                        );
                        refresh_status(&state.logger, &state.active_streams);
                        let mut resp = (status, bytes).into_response();
                        if let Some(ct) = out_headers.get("content-type") {
                            resp.headers_mut().insert("content-type", ct.clone());
                        }
                        resp
                    }
                    Err(e) => {
                        state.logger.finish_request(
                            &req_id, status.as_u16(), false, Some(start.elapsed()),
                            None, None, Some(req_bytes), None, "上游读取错误",
                        );
                        refresh_status(&state.logger, &state.active_streams);
                        state.logger.error(&format!("[{req_id}] 上游读取错误: {e}"));
                        (StatusCode::BAD_GATEWAY, format!("upstream read error: {e}")).into_response()
                    }
                }
            }
        }
        Err(e) => {
            state.logger.finish_request(
                &req_id, 502, false, Some(start.elapsed()),
                None, None, Some(req_bytes), None, "上游连接失败",
            );
            refresh_status(&state.logger, &state.active_streams);
            state.logger.error(&format!("[{req_id}] 上游连接失败: {e}"));
            (StatusCode::BAD_GATEWAY, format!("upstream connect error: {e}")).into_response()
        }
    }
}

/// SSE 流循环的会话状态：可变部分收敛于此（替代原 14 项 unfold 元组）。
/// 不变量（logger/req_id/idle/status/total_dropped/streams/req_bytes/start）在 new 时捕获。
struct StreamState {
    /// 上游字节流
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    /// SSE 归一化器
    norm: SseNormalizer,
    /// 流是否已完成（[DONE] 已见或异常终止）
    done: bool,
    /// 真·首包（第一个上游 chunk）到达时刻
    first_chunk_at: Option<Instant>,
    /// 活性行节流：上次报告时刻 + 当时字节数
    last_report: (Instant, u64),
    /// spinner 帧计数
    tick: usize,
    /// 已从上游收到的总字节数
    streamed: u64,

    // ---- 不变量 ----
    logger: Logger,
    req_id: String,
    idle: u64,
    status: u16,
    total_dropped: Arc<std::sync::atomic::AtomicU64>,
    streams: Arc<Mutex<HashMap<String, u64>>>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    req_bytes: u64,
    start: Instant,
}

impl StreamState {
    fn new(
        stream: reqwest::Response,
        status: u16,
        state: AppState,
        req_id: String,
        req_bytes: u64,
        start: Instant,
    ) -> Self {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            stream: Box::pin(stream.bytes_stream()),
            norm: SseNormalizer::new(),
            done: false,
            first_chunk_at: None,
            // 回拨 2s：让首条活性行在 ~1s 就出现
            last_report: (Instant::now() - Duration::from_secs(2), 0),
            tick: 0,
            streamed: 0,
            logger: state.logger.clone(),
            req_id,
            idle: state.idle_secs,
            status,
            total_dropped: state.total_dropped.clone(),
            streams: state.active_streams.clone(),
            finished,
            req_bytes,
            start,
        }
    }

    /// 流循环的单步：读一个上游块 → 归一化 → 产出下发字节。
    /// 返回 None 表示流结束。
    async fn step(&mut self) -> Option<Result<bytes::Bytes, std::io::Error>> {
        if self.done {
            return None;
        }
        loop {
            // 读空闲看门狗：GLM 长思考可能几十秒无输出；0=禁用
            let next = if self.idle > 0 {
                tokio::time::timeout(Duration::from_secs(self.idle), self.stream.next()).await
            } else {
                Ok(self.stream.next().await)
            };
            match next {
                Err(_elapsed) => {
                    let m = format!("读空闲 {}s 无数据，主动中止上游（疑似链路静默断开）", self.idle);
                    self.logger.error(&format!("[{}] {}", self.req_id, m));
                    let _ = self.norm.drain_abrupt();
                    self.total_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let out = self.norm.error_frame(&m);
                    self.logger.finish_request(
                        &self.req_id, 504, false, Some(self.start.elapsed()),
                        self.first_chunk_at.map(|t| t.duration_since(self.start)),
                        self.norm.usage_tokens, Some(self.req_bytes), Some(self.streamed), "读空闲中止",
                    );
                    cleanup_stream(&self.logger, &self.streams, &self.req_id);
                    self.finished.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Some(Ok(bytes::Bytes::from(out)));
                }
                Ok(Some(Ok(chunk))) => {
                    // 真·首包 = 第一个上游 chunk（不是响应头）
                    if self.first_chunk_at.is_none() {
                        self.first_chunk_at = Some(Instant::now());
                    }
                    self.streamed += chunk.len() as u64;
                    self.streams.lock().unwrap().insert(self.req_id.clone(), self.streamed);
                    // 活性行节流：首报 1s，之后 ≥3s 或新增 ≥256KB
                    if self.last_report.0.elapsed() >= Duration::from_secs(3)
                        || self.streamed - self.last_report.1 >= 256 * 1024
                    {
                        self.tick += 1;
                        let frame = SPINNER[self.tick % SPINNER.len()];
                        let tty_text = stream_status_text(&self.streams, &self.req_id, frame);
                        self.logger.progress(&self.req_id, self.streamed, &tty_text);
                        self.last_report = (Instant::now(), self.streamed);
                    }
                    let lines = self.norm.push(&chunk);
                    if self.norm.done_seen {
                        self.done = true;
                        // 收到 [DONE] 即流逻辑完成。必须在置位处就地收尾：
                        // 下一轮 poll 走 if done 提前退出，Ok(None) 分支执行不到（K10）。
                        self.logger.finish_request(
                            &self.req_id, self.status, true, Some(self.start.elapsed()),
                            self.first_chunk_at.map(|t| t.duration_since(self.start)),
                            self.norm.usage_tokens, Some(self.req_bytes), Some(self.streamed), "",
                        );
                        cleanup_stream(&self.logger, &self.streams, &self.req_id);
                    }
                    if !lines.is_empty() {
                        let mut out = Vec::new();
                        for l in lines {
                            out.extend_from_slice(l.as_bytes());
                            out.push(b'\n');
                            out.push(b'\n');
                        }
                        return Some(Ok(bytes::Bytes::from(out)));
                    }
                }
                Ok(Some(Err(e))) => {
                    self.logger.error(&format!("[{}] 上游流错误: {}", self.req_id, e));
                    self.done = true;
                    cleanup_stream(&self.logger, &self.streams, &self.req_id);
                    return Some(Err(std::io::Error::new(std::io::ErrorKind::Other, e)));
                }
                Ok(None) => {
                    // 上游流自然关闭。若 done_seen=false 且缓冲有残留，说明是异常切断的半截帧
                    // → 丢弃，绝不伪造完整 frame；并用标准 error 事件告知下游。
                    let info = self.norm.drain_abrupt();
                    let mut abnormal = false;
                    if let Some(m) = info {
                        self.logger.error(&format!("[{}] {}", self.req_id, m));
                        self.total_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        abnormal = true;
                    }
                    let mut out: Vec<u8> = Vec::new();
                    if abnormal && !self.norm.done_seen {
                        out.extend_from_slice(&self.norm.error_frame("upstream stream terminated prematurely"));
                    }
                    self.logger.finish_request(
                        &self.req_id, self.status, self.norm.done_seen && !abnormal,
                        Some(self.start.elapsed()),
                        self.first_chunk_at.map(|t| t.duration_since(self.start)),
                        self.norm.usage_tokens, Some(self.req_bytes), Some(self.streamed),
                        if abnormal { "上游异常切断" } else { "" },
                    );
                    cleanup_stream(&self.logger, &self.streams, &self.req_id);
                    self.finished.store(true, std::sync::atomic::Ordering::Relaxed);
                    if out.is_empty() {
                        return None;
                    }
                    return Some(Ok(bytes::Bytes::from(out)));
                }
            }
        }
    }
}

/// 6 位十六进制 req_id 的随机源（轻量 xorshift，无需额外依赖）
fn rand_u24() -> u32 {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x & 0xff_ffff
}
