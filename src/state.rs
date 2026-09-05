//! 代理共享状态模块：`AppState`（ axum 全局状态）、`NotifyDrop`（取消透传感知）、
//! 终端原位状态行的文案与生命周期辅助。
//!
//! 职责边界（高内聚）：
//! - `AppState`：配置与计数器的唯一持有者，随 Router 分发到各 handler
//! - `NotifyDrop`：包裹下发流，Drop 时感知"下游提前断开"（K05），并清理活跃流表
//! - 状态行：全代理共用一条原位动画行，多流聚合显示（D13），并发安全

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::logger::Logger;

/// axum 全局共享状态：配置、HTTP 客户端、计数器、活跃流表。
#[derive(Clone)]
pub struct AppState {
    /// 上游 HTTP 客户端（rustls TLS）
    pub client: reqwest::Client,
    /// 上游基地址（如 https://open.bigmodel.cn/api/paas/v4）
    pub upstream: String,
    /// 日志器（终端 + 文件双通道）
    pub logger: Logger,
    /// 在途请求数（优雅退出时等待收尾）
    pub inflight: Arc<std::sync::atomic::AtomicU64>,
    /// 进程级累计统计
    pub total_requests: Arc<std::sync::atomic::AtomicU64>,
    pub total_dropped: Arc<std::sync::atomic::AtomicU64>,
    /// stats.jsonl 路径（每请求一行，供 stats 子命令汇总）
    pub stats_path: PathBuf,
    /// 读空闲超时（秒），0=禁用
    pub idle_secs: u64,
    /// 远端令牌（空=不鉴权）
    pub auth_token: String,
    /// 活跃 SSE 流：req_id → 已下行字节（驱动终端原位状态行）
    pub active_streams: Arc<Mutex<HashMap<String, u64>>>,
}

/// 终端原位状态行 spinner 帧
pub const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// 包裹下发流：Drop 时能感知"下游提前断开"（K05 盲区的观测面）。
/// 正常完成时 finished 已置位，不误报。
pub struct NotifyDrop<S> {
    inner: std::pin::Pin<Box<S>>,
    logger: Logger,
    req_id: String,
    finished: Arc<std::sync::atomic::AtomicBool>,
    /// 活跃流表（drop 时移除自身并刷新/擦除状态行）
    streams: Arc<Mutex<HashMap<String, u64>>>,
}

impl<S: futures_util::Stream> futures_util::Stream for NotifyDrop<S> {
    type Item = S::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<S::Item>> {
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
        // 无论正常结束还是取消，都从活跃流表移除；表空则擦除终端状态行
        cleanup_stream(&self.logger, &self.streams, &self.req_id);
    }
}

impl<S> NotifyDrop<S> {
    pub fn new(
        inner: S,
        logger: Logger,
        req_id: String,
        finished: Arc<std::sync::atomic::AtomicBool>,
        streams: Arc<Mutex<HashMap<String, u64>>>,
    ) -> Self {
        Self { inner: Box::pin(inner), logger, req_id, finished, streams }
    }
}

/// 生命周期行打印后刷新状态行：无活跃流则擦除，否则重画聚合（避免动画残留或丢失）
pub fn refresh_status(logger: &Logger, streams: &Arc<Mutex<HashMap<String, u64>>>) {
    let len = streams.lock().unwrap().len();
    if len == 0 {
        logger.clear_status();
    } else {
        logger.update_status(&stream_status_text(streams, "", '⠹'));
    }
}

/// 终端原位状态行文案：单流显示具体 req_id 与字节数，多流聚合
pub fn stream_status_text(
    streams: &Arc<Mutex<HashMap<String, u64>>>,
    single_id: &str,
    frame: char,
) -> String {
    let map = streams.lock().unwrap();
    let total: u64 = map.values().sum();
    if map.len() <= 1 {
        format!(
            "\x1b[36m{frame}\x1b[0m \x1b[1;36m[{single_id:^6}]\x1b[0m \x1b[34m↓\x1b[0m \x1b[2m{}…\x1b[0m",
            crate::logger::fmt_bytes(total)
        )
    } else {
        format!(
            "\x1b[36m{frame}\x1b[0m \x1b[2m{} 个流 · ↓ {}…\x1b[0m",
            map.len(),
            crate::logger::fmt_bytes(total)
        )
    }
}

/// 流收尾：从活跃流表移除；表空则擦除终端状态行，否则重画剩余聚合
pub fn cleanup_stream(logger: &Logger, streams: &Arc<Mutex<HashMap<String, u64>>>, req_id: &str) {
    let remaining = {
        let mut m = streams.lock().unwrap();
        m.remove(req_id);
        m.len()
    };
    if remaining == 0 {
        logger.clear_status();
    } else {
        logger.update_status(&stream_status_text(streams, "", '⠹'));
    }
}
