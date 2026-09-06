//! 集成测试：mock SSE 上游 + 真实转发链路
use axum::{routing::{get, post}, Router};
use piglmbridger::logger::Logger;
use piglmbridger::state::AppState;
use std::sync::Arc;

async fn spawn_mock(respond: impl Fn() -> Vec<u8> + Send + Sync + 'static) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let respond = Arc::new(respond);
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let respond = respond.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let body = respond();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    format!("http://{addr}")
}

fn test_state(upstream: String, logger: Logger, auth_token: &str) -> AppState {
    AppState {
        client: reqwest::Client::new(),
        upstream,
        logger,
        inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        total_requests: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        total_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        stats_path: std::env::temp_dir().join(format!("piglmb-it-{}.jsonl", std::process::id())),
        idle_secs: 0,
        auth_token: auth_token.into(),
        active_streams: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    }
}

fn sse_frames() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"data: {\"choices\":[{\"delta\":{\"content\":\"A\"}}]}\n\n");
    let u = b"data: {\"usage\":{\"total_tokens\":150}}\n\n";
    let half = u.len() / 2;
    b.extend_from_slice(&u[..half]);
    b.extend_from_slice(&u[half..]);
    b.extend_from_slice(b"data: [DONE]\n\n");
    b
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(piglmbridger::proxy::passthrough))
        .route("/health", get(piglmbridger::proxy::health))
        .route("/logs", get(piglmbridger::proxy::logs))
        .fallback(piglmbridger::proxy::passthrough)
        .with_state(state)
}

/// 起 axum 服务（随机端口），返回 base url 与关闭句柄
async fn spawn_app(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    format!("http://{addr}")
}

async fn post_chat(base: &str, model: &str, token: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(format!("{{\"model\":\"{model}\"}}"));
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn normal_finish_logs_req_resp_tokens() {
    let upstream = spawn_mock(|| sse_frames()).await;
    let state = test_state(upstream, Logger::memory(), "");
    let base = spawn_app(state.clone()).await;
    let resp = post_chat(&base, "glm-5.3-flash", None).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().contains("text/event-stream"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("data: [DONE]"), "应含 [DONE]: {body}");
    assert!(!body.contains("proxy_upstream_error"), "正常收尾不应有 error 帧");

    let lines = state.logger.captured();
    // 回归断言：结束行只能打一次（K10/双打 bug）
    assert_eq!(lines.iter().filter(|l| l.contains("<- 200")).count(), 1, "结束行重复: {lines:?}");
    let finish = lines.iter().find(|l| l.contains("<- 200")).expect("应有结束行");
    assert!(finish.contains("req "), "req 字节数: {finish}");
    assert!(finish.contains("resp"), "resp 字节数: {finish}");
    assert!(finish.contains("150 tok"), "tokens: {finish}");
    assert!(finish.contains("首包"), "真·首包: {finish}");
}

#[tokio::test]
async fn truncated_upstream_drops_frame_and_emits_error() {
    let upstream = spawn_mock(|| "data: {\"choices\":[{\"delta\":{\"content\":\"半".as_bytes().to_vec()).await;
    let state = test_state(upstream, Logger::memory(), "");
    let base = spawn_app(state.clone()).await;
    let resp = post_chat(&base, "glm-5.3-flash", None).await;
    let body = resp.text().await.unwrap();
    assert!(body.contains("proxy_upstream_error"), "应含带内 error 帧: {body}");
    assert!(body.contains("data: [DONE]"), "error 帧后应跟 [DONE]");

    let lines = state.logger.captured();
    assert!(
        lines.iter().any(|l| l.contains("上游异常切断") || l.contains("残帧")),
        "应记录异常切断: {lines:?}"
    );
}

#[tokio::test]
async fn non_glm_model_passthrough_bytes_unchanged() {
    let upstream = spawn_mock(|| sse_frames()).await;
    let state = test_state(upstream, Logger::memory(), "");
    let base = spawn_app(state).await;
    let resp = post_chat(&base, "deepseek-v4", None).await;
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), &sse_frames()[..], "字节级直通应逐字节一致");
}

#[tokio::test]
async fn auth_token_rejects_without_reaching_upstream() {
    // 上游端口不可达；若鉴权被绕过会得到 502 而非 401
    let state = test_state("http://127.0.0.1:1".into(), Logger::memory(), "secret");
    let base = spawn_app(state).await;
    let resp = post_chat(&base, "glm-5.3-flash", None).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn health_endpoint_reports_ok_without_auth() {
    let state = test_state("http://127.0.0.1:1".into(), Logger::memory(), "secret");
    let base = spawn_app(state).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["name"], "piglmbridger");
    assert!(body["version"].as_str().is_some());
}

#[tokio::test]
async fn logs_endpoint_returns_single_line_summary() {
    let upstream = spawn_mock(|| sse_frames()).await;
    let state = test_state(upstream, Logger::memory(), "");
    let base = spawn_app(state.clone()).await;
    let resp = post_chat(&base, "glm-5.3-flash", None).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    let r = reqwest::Client::new()
        .get(format!("{base}/logs?lines=50"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let lines = body["lines"].as_array().expect("lines 数组");
    assert_eq!(lines.len(), 1, "单请求应恰好 1 条单行摘要: {lines:?}");
    let line = lines[0].as_str().unwrap();
    assert!(line.contains("▶ ["), "开始图标+id: {line}");
    assert!(line.contains("glm-5.3-flash → "), "模型+目标: {line}");
    assert!(line.contains("↑ "), "请求体: {line}");
    assert!(line.contains("↓ "), "响应体: {line}");
    assert!(line.contains("150 tok"), "usage 总量: {line}");
    assert!(line.contains("✔ 200"), "状态: {line}");
    assert!(line.contains("首包 "), "首包: {line}");

    // lines 上限/下限不炸
    let r = reqwest::Client::new().get(format!("{base}/logs?lines=99999")).send().await.unwrap();
    assert_eq!(r.status(), 200);
}
