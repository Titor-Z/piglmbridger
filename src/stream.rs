//! SSE 流归一化模块：把 GLM 上游切碎的 SSE 字节流重组为完整的 `data:` 行。
//!
//! 职责边界（高内聚）：
//! - UTF-8 字节边界缓冲（中文不切半）
//! - 行级重组：只有以换行收尾的片段才算完整行，残缺尾段留缓冲等下一块（D06 根因修复）
//! - 重复 `[DONE]` 去重、空帧丢弃、usage.total_tokens 轻量探测
//! - 异常切断时丢弃残帧（绝不补 `\n` 伪造完整帧，K03）
//!
//! 本模块是纯逻辑组件，不依赖任何网络/IO 类型，可独立单元测试。

use serde_json::json;

/// SSE 归一化器：逐块 `push` 原始字节，产出规整后的完整行。
pub struct SseNormalizer {
    /// 字节缓冲（含 UTF-8 多字节残尾与半截行）
    buffer: Vec<u8>,
    /// 是否收到过 `[DONE]`（流正常完成的标志）
    pub done_seen: bool,
    /// 统计：丢弃的空帧/重复 [DONE] 数
    pub dropped: u64,
    /// 统计：成功下发的行数
    pub emitted: u64,
    /// 统计：跨块拼接后才发出的行数（分片缓冲命中）
    pub rejoined: u64,
    /// 上一轮是否留下了残尾（用于 rejoined 计数）
    had_partial: bool,
    /// 上游最后一个带 usage 的 data 帧里的 total_tokens（计费观测用）
    pub usage_tokens: Option<u64>,
}

impl SseNormalizer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            done_seen: false,
            dropped: 0,
            emitted: 0,
            rejoined: 0,
            had_partial: false,
            usage_tokens: None,
        }
    }

    /// 喂入一个上游网络块，返回本块重组出的完整行（不含换行）。
    ///
    /// 核心判据（D06）：本块是否以 `\n` 收尾，决定最后一个 split 片段是不是完整行。
    /// GLM 上游经常在同一个网络块里塞"多条完整 data 行 + 一个只写到一半的下一条"，
    /// 只有以换行收尾的片段才算完整，残缺尾段必须放回 buffer 等下块拼接。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let is_continuation = self.had_partial;

        // 逐字节窗口处理；每轮回先把能从合法 UTF-8 边界切开的部分取走。
        while let Some(valid_len) = valid_utf8_prefix_len(&self.buffer) {
            let s = String::from_utf8_lossy(&self.buffer[..valid_len]).into_owned();
            self.buffer.drain(..valid_len);

            let ends_nl = s.ends_with('\n');
            let mut parts = s.split('\n').peekable();
            while let Some(line) = parts.next() {
                let is_last = parts.peek().is_none();
                if is_last && !ends_nl && !line.is_empty() {
                    // 残缺尾行（不带换行收尾）：放回缓冲，绝不当下发（K02/K03）
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
    pub fn error_frame(&self, message: &str) -> Vec<u8> {
        let payload = json!({
            "error": {
                "message": message,
                "type": "proxy_upstream_error",
                "proxy": "piglmbridger",
            }
        });
        format!("data: {payload}\n\ndata: [DONE]\n\n").into_bytes()
    }

    /// 单行规整：`data:` 前缀识别、[DONE] 去重、空帧丢弃、usage 探测。
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
            // 轻量 usage 探测：仅当行含 "usage" 才解析（避免每帧 JSON 开销）
            if data.contains("\"usage\"") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(t) = v.get("usage").and_then(|u| u.get("total_tokens")).and_then(|t| t.as_u64()) {
                        self.usage_tokens = Some(t);
                    }
                }
            }
            return Some(format!("data: {data}"));
        }
        Some(line.to_string())
    }

    /// 异常中断时调用：缓冲区里若还残留一行，则说明该 SSE 帧没有以换行/空行收尾，
    /// 是上游把帧切半后丢弃了（GLM 的已知坑）。绝不能补 \n 伪造完整帧发给下游，
    /// 否则会产出 "Unterminated string in JSON"。这里直接丢弃并返回描述供记日志。
    pub fn drain_abrupt(&mut self) -> Option<String> {
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

/// 中间截断长字符串（保留头尾，防日志被超长 payload 撑爆）
fn truncate_mid(s: &str, n: usize) -> String {
    if s.chars().count() <= n || n == 0 {
        return s.to_string();
    }
    let half = n / 2;
    let start: String = s.chars().take(half).collect();
    let end: String = s.chars().skip(s.chars().count() - half).collect();
    format!("{start}…{end}")
}

/// 计算缓冲区中合法 UTF-8 前缀长度。
/// 中文等多字节字符被 TCP 从中间切开时，残尾（<4 字节且是首字节形态）留缓冲等待拼接。
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

#[cfg(test)]
mod tests {
    use super::SseNormalizer;

    #[test]
    fn forwards_complete_lines_and_joins_split_frames() {
        let mut n = SseNormalizer::new();
        let out1 = n.push(b"data: {\"a\":1}\n\ndata: {\"b\":");
        assert_eq!(out1, vec!["data: {\"a\":1}"]);
        let out2 = n.push(b"2}\n\n");
        assert_eq!(out2, vec!["data: {\"b\":2}"]);
        assert!(n.buffer.is_empty());
    }

    #[test]
    fn drops_abruptly_truncated_data_frame() {
        let mut n = SseNormalizer::new();
        // 先来一行完整行（emitted=1），再来半截残帧后断流
        let out1 = n.push(b"data: {\"role\": \"assistant\", \"content\": \"changg\n");
        assert_eq!(out1, vec!["data: {\"role\": \"assistant\", \"content\": \"changg"]);
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

    #[test]
    fn usage_extraction_from_final_chunk() {
        let mut n = SseNormalizer::new();
        let _ = n.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert_eq!(n.usage_tokens, None, "无 usage 的帧不产生值");
        let _ = n.push(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1240,\"total_tokens\":1250}}\n\n",
        );
        assert_eq!(n.usage_tokens, Some(1250));
        // 行仍原样透传，字节零改动
        let out = n.push(b"data: [DONE]\n\n");
        assert_eq!(out, vec!["data: [DONE]"]);
    }

    #[test]
    fn usage_extraction_works_after_cross_chunk_rejoin() {
        // usage 帧本身被 TCP 切半：残尾拼接后才能解析（K06 场景 2）
        let mut n = SseNormalizer::new();
        let _ = n.push(b"data: {\"usage\":{\"total_tokens\":");
        assert_eq!(n.usage_tokens, None, "残帧不解析");
        let _ = n.push(b"999}}\n\n");
        assert_eq!(n.usage_tokens, Some(999));
    }
}
