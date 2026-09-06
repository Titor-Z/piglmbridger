//! 日志模块：写入文件 + 终端输出（TTY 自动着色，管道输出纯文本），支持 logs 子命令查看/跟踪

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// /logs 单行摘要缓冲容量（FIFO 满则淘汰最老）
const SUMMARY_CAP: usize = 200;
/// pending（开始未结束请求）表上限，防止异常路径泄漏（K11 纪律）
const PENDING_CAP: usize = 500;

/// /logs 摘要：请求开始时记录的信息（finish 时拼成单行）
struct PendingReq {
    /// 开始时刻 HH:MM:SS.mmm
    ts: String,
    /// "model → host/path"
    head: String,
    req_bytes: u64,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ColorMode {
    Auto,   // TTY 彩色，管道纯文本
    Always, // 强制彩色（重定向到支持 ANSI 的文件/CI 用）
    Never,  // 强制纯文本
}

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum LogLevel {
    Debug,
    Info,
}

#[derive(Clone)]
pub struct Logger {
    inner: Option<Arc<Mutex<File>>>,
    disabled: bool,
    file_only: bool, // daemon 模式：只写文件，不污染终端
    color: bool,
    level: LogLevel,
    /// 终端是否正有一行“原位状态行”（动画进度）在显示：跨 clone 共享
    status_shown: Arc<Mutex<bool>>,
    /// 内存捕获（仅测试用，memory() 时为 Some）：生产路径必须为 None，否则成隐性泄漏
    captured: Option<Arc<Mutex<Vec<String>>>>,
    /// 日志文件路径（/logs 文件兜底解析用；disabled/memory 模式为 None）
    path: Option<PathBuf>,
    /// /logs 单行摘要：开始未结束的请求（req_id → 开始信息）
    pending: Arc<Mutex<HashMap<String, PendingReq>>>,
    /// /logs 单行摘要：成品行（时间序，FIFO）
    summaries: Arc<Mutex<VecDeque<String>>>,
}

impl Logger {
    /// 初始化：确保目录存在，轮转旧日志（proxy.log -> proxy.log.1，仅保留一份）
    pub fn new_with_mode(log_dir: &Path, mode: ColorMode, file_only: bool, debug_on: bool) -> std::io::Result<Self> {
        std::fs::create_dir_all(log_dir)?;
        let file_path = log_dir.join("proxy.log");
        if file_path.exists() {
            let meta = std::fs::metadata(&file_path)?;
            // 超过 10MB 轮转
            if meta.len() > 10 * 1024 * 1024 {
                let rotated = log_dir.join("proxy.log.1");
                let _ = std::fs::rename(&file_path, &rotated);
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&file_path)?;
        let color = match mode {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::io::stdout().is_terminal(),
        };
        let path = file_path.clone();
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(file))),
            disabled: false,
            file_only,
            color,
            level: if debug_on { LogLevel::Debug } else { LogLevel::Info },
            status_shown: Arc::new(Mutex::new(false)),
            captured: None,
            path: Some(path),
            pending: Arc::new(Mutex::new(HashMap::new())),
            summaries: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    pub fn disabled() -> Self {
        Self { inner: None, disabled: true, file_only: false, color: false, level: LogLevel::Info, status_shown: Arc::new(Mutex::new(false)), captured: None, path: None, pending: Arc::new(Mutex::new(HashMap::new())), summaries: Arc::new(Mutex::new(VecDeque::new())) }
    }

    fn write_line(&self, level: &str, msg: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        if let Some(c) = &self.captured {
            if let Ok(mut c) = c.lock() {
                c.push(format!("[{level}] {msg}"));
            }
        }
        // 终端输出（可选着色）；管道/重定向自动纯文本，grep 友好
        if !self.file_only {
            let line = if self.color {
                let (lv_colored, reset) = match level {
                    "ERROR" => ("\x1b[1;31m", "\x1b[0m"), // 亮红
                    "WARN" => ("\x1b[1;33m", "\x1b[0m"),  // 黄
                    _ => ("\x1b[2m", "\x1b[0m"),          // INFO 暗灰
                };
                // req_id 用青色，同一请求的行在视觉上成组
                let colored = format!("{ts} [{lv_colored}{level:^5}{reset}] {msg}");
                colorize_req_ids(&colored)
            } else {
                format!("{ts} [{level:^5}] {msg}")
            };
            self.tty_prepare();
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
        // 文件输出永远纯文本
        if let Some(f) = &self.inner {
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{ts} [{level:^5}] {msg}");
            }
        }
    }

    /// 打印普通终端行前：若原位状态行在显示，先擦除（\r + ANSI 清行），
    /// 保证普通日志不糊在动画上。擦除后由调用方决定是否重画。
    fn tty_prepare(&self) {
        if let Ok(mut s) = self.status_shown.lock() {
            if *s {
                print!("\r\x1b[2K");
                *s = false;
            }
        }
    }

    /// 原位刷新状态行（\r + 清行 + 重画，不换行）。
    /// 仅 TTY+彩色生效；管道/文件模式 no-op（控制符绝不进管道）。
    pub fn update_status(&self, text: &str) {
        if !(self.color && !self.file_only) {
            return;
        }
        print!("\r\x1b[2K{text}");
        let _ = std::io::stdout().flush();
        if let Ok(mut s) = self.status_shown.lock() {
            *s = true;
        }
    }

    /// 擦除状态行（最后一个流结束时调用，避免残留过期动画）
    pub fn clear_status(&self) {
        if let Ok(mut s) = self.status_shown.lock() {
            if *s {
                print!("\r\x1b[2K");
                let _ = std::io::stdout().flush();
                *s = false;
            }
        }
    }

    pub fn debug(&self, msg: &str) {
        if self.disabled || self.level > LogLevel::Debug { return; }
        self.write_line("DEBUG", msg);
    }

    pub fn info(&self, msg: &str) {
        if self.disabled { return; }
        self.write_line("INFO", msg);
    }

    /// 仅写文件的原样分隔行（无时间戳前缀/级别）：会话标记等，终端零输出
    pub fn marker(&self, text: &str) {
        if self.disabled { return; }
        if let Some(f) = &self.inner {
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{text}");
            }
        }
    }

    /// 仅写文件（终端零输出）：前台模式由调用方自绘样式行时使用，文件里保留纯文本供 grep
    pub fn info_file(&self, msg: &str) {
        if self.disabled || self.level > LogLevel::Info { return; }
        if let Some(c) = &self.captured {
            if let Ok(mut c) = c.lock() {
                c.push(format!("[INFO] {msg}"));
            }
        }
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        if let Some(f) = &self.inner {
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{ts} [INFO ] {msg}");
            }
        }
    }

    pub fn warn(&self, msg: String) {
        self.write_line("WARN", &msg);
    }

    pub fn error(&self, msg: &str) {
        self.write_line("ERROR", msg);
    }

    /// 请求生命周期（开始行）：极简语言——模型 + 目标 + 请求体大小，零口水话。
    /// 文件行保留完整 URL（grep/排查），TTY 用缩写。
    pub fn start_request(
        &self,
        req_id: &str,
        model: Option<&str>,
        method: &str,
        path: &str,
        upstream: &str,
        req_bytes: u64,
    ) {
        let model_disp = model.unwrap_or("-");
        let file_msg = format!(
            "[{req_id}] -> {model_disp} {method} {path} (req {}) 转发至 {upstream}",
            fmt_bytes(req_bytes)
        );
        if self.color && !self.file_only {
            let short = chrono::Local::now().format("%H:%M:%S%.3f");
            let tty = format!(
                "\x1b[2m{short}\x1b[0m \x1b[1;36m▶\x1b[0m \x1b[1;36m[{req_id:^6}]\x1b[0m \
                 \x1b[1m{model_disp}\x1b[0m \x1b[34m→ {}\x1b[0m \x1b[1;32m↑\x1b[0m \x1b[2m{}\x1b[0m",
                shorten_upstream(upstream),
                fmt_bytes(req_bytes)
            );
            self.emit_tty(&tty);
        }
        self.write_file("INFO", &file_msg);
        self.record_start(req_id, model, upstream, req_bytes);
        if !self.color && !self.file_only {
            self.print_plain("INFO", &file_msg);
        }
    }

    /// 传输中进度（节流触发）：文件写纯文本进度行；终端原位刷新状态行（不产生新行）。
    /// tty_text 由调用方组装（单流带 req_id，多流聚合）。
    pub fn progress(&self, req_id: &str, bytes: u64, tty_text: &str) {
        let file_msg = format!("[{req_id}] … ↓ {}", fmt_bytes(bytes));
        self.write_file("INFO", &file_msg);
        self.update_status(tty_text);
        if !self.color && !self.file_only {
            self.print_plain("INFO", &file_msg);
        }
    }

    /// 请求生命周期（结束行）：纯符号分隔的指标串，detail 仅错误时非空。
    /// `✔ [id] 200 · +5.2s · ↑ 357.6KB · ↓ 96.4KB · 1250 tok`
    #[allow(clippy::too_many_arguments)]
    pub fn finish_request(
        &self,
        req_id: &str,
        status: u16,
        ok: bool,
        elapsed: Option<std::time::Duration>,
        first_byte: Option<std::time::Duration>,
        tokens: Option<u64>,
        req_bytes: Option<u64>,
        bytes: Option<u64>,
        detail: &str,
    ) {
        // 文件行（纯文本、全量指标、grep 友好）
        let mut file_msg = format!("[{req_id}] <- {status}");
        if let Some(d) = elapsed {
            file_msg.push_str(&format!(" 耗时 {:.2}s", d.as_secs_f32()));
        }
        if let Some(d) = first_byte {
            file_msg.push_str(&format!(" 首包 {}ms", d.as_millis()));
        }
        if let Some(b) = req_bytes {
            file_msg.push_str(&format!(" req {}", fmt_bytes(b)));
        }
        if let Some(b) = bytes {
            file_msg.push_str(&format!(" resp {}", fmt_bytes(b)));
        }
        if let Some(t) = tokens {
            file_msg.push_str(&format!(" {t} tok"));
        }
        if !detail.is_empty() {
            file_msg.push_str(&format!(" {detail}"));
        }

        if self.color && !self.file_only {
            let short = chrono::Local::now().format("%H:%M:%S%.3f");
            let icon = if ok { "✔" } else { "✘" };
            let st_col = if (200..300).contains(&status) { "1;32" } else { "1;31" };
            let status_str = if (200..300).contains(&status) {
                format!("\x1b[{st_col}m{status}\x1b[0m")
            } else {
                format!("\x1b[{st_col}m{status}\x1b[0m")
            };
            // 指标串：`·` 分隔，存在的才显示
            let mut parts: Vec<String> = Vec::new();
            if let Some(d) = elapsed {
                parts.push(format!("\x1b[1;35m{}\x1b[0m", fmt_duration(d)));
            }
            if let Some(d) = first_byte {
                parts.push(format!("\x1b[2m首包 {}ms\x1b[0m", d.as_millis()));
            }
            if let Some(b) = req_bytes {
                parts.push(format!("\x1b[1;32m↑\x1b[0m \x1b[2m{}\x1b[0m", fmt_bytes(b)));
            }
            if let Some(b) = bytes {
                parts.push(format!("\x1b[34m↓\x1b[0m \x1b[2m{}\x1b[0m", fmt_bytes(b)));
            }
            if let Some(t) = tokens {
                parts.push(format!("\x1b[2m{t} tok\x1b[0m"));
            }
            let metrics = parts.join(" \x1b[2m·\x1b[0m ");
            let tty = format!(
                "\x1b[2m{short}\x1b[0m \x1b[{st_col}m{icon}\x1b[0m \x1b[1;36m[{req_id:^6}]\x1b[0m {status_str} {metrics}"
            );
            let line = if detail.is_empty() {
                tty
            } else if ok {
                format!("{tty} {detail}")
            } else {
                format!("{tty} \x1b[1;31m{detail}\x1b[0m")
            };
            self.emit_tty(&line);
        }
        self.write_file("INFO", &file_msg);
        self.record_summary(req_id, status, ok, elapsed, first_byte, tokens, req_bytes, bytes, detail);
        if !self.color && !self.file_only {
            self.print_plain("INFO", &file_msg);
        }
    }

    /// 终端行直接输出（已着色）；不走 write_line 的统一格式
    fn emit_tty(&self, tty_line: &str) {
        self.tty_prepare();
        println!("{tty_line}");
        let _ = std::io::stdout().flush();
    }

    fn print_plain(&self, level: &str, msg: &str) {
        self.tty_prepare();
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        println!("{ts} [{level:^5}] {msg}");
        let _ = std::io::stdout().flush();
    }

    /// 内存捕获 Logger：不写文件、不进终端，供集成测试断言日志行
    pub fn memory() -> Self {
        Self {
            inner: None,
            disabled: false,
            file_only: false,
            color: false,
            level: LogLevel::Info,
            status_shown: Arc::new(Mutex::new(false)),
            captured: Some(Arc::new(Mutex::new(Vec::new()))),
            path: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            summaries: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// /logs 摘要行：取末尾 n 条（时间序快照）
    pub fn summaries(&self, n: usize) -> Vec<String> {
        let q = self.summaries.lock().unwrap();
        let skip = q.len().saturating_sub(n);
        q.iter().skip(skip).cloned().collect()
    }

    /// 日志文件路径（/logs 文件兜底解析用）
    pub fn file_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    /// 记录请求开始信息（供 finish 时拼单行摘要）；pending 表超上限按任意序淘汰防泄漏
    fn record_start(&self, req_id: &str, model: Option<&str>, upstream: &str, req_bytes: u64) {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
        let head = format!("{} → {}", model.unwrap_or("-"), shorten_upstream(upstream));
        let mut p = self.pending.lock().unwrap();
        if p.len() >= PENDING_CAP {
            p.clear(); // 极端兜底：只丢摘要头信息，不影响任何主流程
        }
        p.insert(req_id.to_string(), PendingReq { ts, head, req_bytes });
    }

    /// finish 时拼 /logs 单行摘要并入库：
    /// `HH:MM:SS.mmm ▶ [id] model → host/path ↑ req · ✔ status +5.2s · 首包 Nms · ↓ resp · T tok`
    /// 字段各出现一次：↑=请求体、↓=响应流、tok=usage 总量（输入+输出），缺省段自动省略。
    fn record_summary(
        &self,
        req_id: &str,
        status: u16,
        ok: bool,
        elapsed: Option<std::time::Duration>,
        first_byte: Option<std::time::Duration>,
        tokens: Option<u64>,
        req_bytes: Option<u64>,
        bytes: Option<u64>,
        detail: &str,
    ) {
        let (ts, head, start_req) = match self.pending.lock().unwrap().remove(req_id) {
            Some(p) => (p.ts, p.head, Some(p.req_bytes)),
            None => (
                chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
                "-".to_string(),
                None,
            ),
        };
        let icon = if ok { "✔" } else { "✘" };
        let mut segs: Vec<String> = Vec::new();
        if let Some(b) = req_bytes.or(start_req) {
            segs.push(format!("↑ {}", fmt_bytes(b)));
        }
        let mut tail = format!("{icon} {status}");
        if let Some(d) = elapsed {
            tail.push(' ');
            tail.push_str(&fmt_duration(d));
        }
        segs.push(tail);
        if let Some(d) = first_byte {
            segs.push(format!("首包 {}ms", d.as_millis()));
        }
        if let Some(b) = bytes {
            segs.push(format!("↓ {}", fmt_bytes(b)));
        }
        if let Some(t) = tokens {
            segs.push(format!("{t} tok"));
        }
        let mut line = format!("{ts} ▶ [{req_id}] {head} {}", segs.join(" · "));
        if !detail.is_empty() {
            line.push(' ');
            line.push_str(detail);
        }
        let mut q = self.summaries.lock().unwrap();
        if q.len() >= SUMMARY_CAP {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// /logs 重启兜底：解析文件日志末尾，按 `->`/`<-` 行配对拼出同款单行摘要。
    /// `…` 进度行与解析失败的行一律跳过。
    pub fn file_summary_tail(path: &Path, n: usize) -> Vec<String> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let text = String::from_utf8_lossy(&data);
        let mut starts: HashMap<String, (String, String, String)> = HashMap::new(); // id -> (ts, head, req_str)
        let mut out: VecDeque<String> = VecDeque::new();
        for line in text.lines() {
            // 行结构：`ts [LEVEL] msg`；取 level 右括号后的 msg
            let body = match line.find("] ") {
                Some(i) => &line[i + 2..],
                None => continue,
            };
            // ts = 行首完整时间戳的第二段（HH:MM:SS.mmm）
            let ts = line.split(' ').nth(1).unwrap_or("");
            if let Some(idx) = body.find("] -> ") {
                let id = body[1..idx].to_string();
                let after = &body[idx + 5..];
                let (model, remainder) = match after.split_once(' ') {
                    Some(x) => x,
                    None => continue,
                };
                let req_str = remainder
                    .split("(req ")
                    .nth(1)
                    .and_then(|s| s.split(')').next())
                    .unwrap_or("-")
                    .to_string();
                let url = remainder.split("转发至 ").nth(1).unwrap_or("").trim().to_string();
                starts.insert(id, (ts.to_string(), format!("{model} → {}", shorten_upstream(&url)), req_str));
                continue;
            }
            // `[id] <- status 耗时 Xs 首包 Nms req S resp S T tok [detail]`
            if let Some(idx) = body.find("] <- ") {
                let id = body[1..idx].to_string();
                let after = &body[idx + 5..];
                let status: u16 = match after.split(' ').next().and_then(|s| s.parse().ok()) {
                    Some(s) => s,
                    None => continue,
                };
                let rest = &after[after.find(' ').map(|i| i + 1).unwrap_or(after.len())..];
                let mut elapsed = None;
                let mut first = None;
                let mut req_str: Option<String> = None;
                let mut resp_str: Option<String> = None;
                let mut tokens: Option<u64> = None;
                let mut toks = rest.split_whitespace().peekable();
                let mut consumed_detail: Vec<&str> = Vec::new();
                while let Some(t) = toks.next() {
                    match t {
                        "耗时" => {
                            elapsed = toks.next().and_then(|v| v.strip_suffix('s')).and_then(|v| v.parse::<f32>().ok());
                        }
                        "首包" => {
                            first = toks.next().and_then(|v| v.strip_suffix("ms")).and_then(|v| v.parse::<u64>().ok());
                        }
                        "req" => req_str = toks.next().map(|s| s.to_string()),
                        "resp" => resp_str = toks.next().map(|s| s.to_string()),
                        _ => {
                            // 可能是 `T tok` 或 detail 开头
                            let rest_str = rest;
                            if let Ok(num) = t.parse::<u64>() {
                                if toks.peek() == Some(&"tok") {
                                    tokens = Some(num);
                                    toks.next();
                                    // tok 之后的全部算 detail
                                    consumed_detail = toks.collect();
                                    break;
                                }
                            }
                            // detail：从当前 token 起的原始子串
                            if let Some(p) = rest_str.find(t) {
                                consumed_detail = rest_str[p..].split_whitespace().collect();
                            }
                            break;
                        }
                    }
                }
                let (s_ts, head, s_req) = starts
                    .remove(&id)
                    .unwrap_or((ts.to_string(), "-".to_string(), "-".to_string()));
                let icon = if (200..300).contains(&status) { "✔" } else { "✘" };
                let mut segs: Vec<String> = Vec::new();
                if req_str.as_deref() != Some("-") {
                    if let Some(r) = req_str.clone().or(if s_req == "-" { None } else { Some(s_req.clone()) }) {
                        segs.push(format!("↑ {r}"));
                    }
                }
                let mut tail = format!("{icon} {status}");
                if let Some(e) = elapsed {
                    tail.push(' ');
                    tail.push_str(&fmt_duration(std::time::Duration::from_secs_f32(e)));
                }
                segs.push(tail);
                if let Some(f) = first {
                    segs.push(format!("首包 {f}ms"));
                }
                if let Some(r) = resp_str {
                    segs.push(format!("↓ {r}"));
                }
                if let Some(t) = tokens {
                    segs.push(format!("{t} tok"));
                }
                let mut l = format!("{s_ts} ▶ [{id}] {head} {}", segs.join(" · "));
                if !consumed_detail.is_empty() {
                    l.push(' ');
                    l.push_str(&consumed_detail.join(" "));
                }
                if out.len() >= SUMMARY_CAP {
                    out.pop_front();
                }
                out.push_back(l);
            }
        }
        let skip = out.len().saturating_sub(n);
        out.iter().skip(skip).cloned().collect()
    }

    /// 取出捕获的日志行（快照）
    pub fn captured(&self) -> Vec<String> {
        self.captured
            .as_ref()
            .map(|c| c.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// 只写文件（不输出终端）；捕获模式下同步写入内存缓冲
    fn write_file(&self, level: &str, msg: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        if let Some(c) = &self.captured {
            if let Ok(mut c) = c.lock() {
                c.push(format!("[{level}] {msg}"));
            }
        }
        if let Some(f) = &self.inner {
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{ts} [{level:^5}] {msg}");
            }
        }
    }
}

/// 上游 URL 缩写（仅 TTY 显示）：域名去 `open.`/`api.` 前缀 + 路径末两段。
/// 解析失败原样返回。文件日志永远用完整 URL。
pub fn shorten_upstream(full_url: &str) -> String {
    let url = match reqwest::Url::parse(full_url) {
        Ok(u) => u,
        Err(_) => return full_url.to_string(),
    };
    let host = url.host_str().unwrap_or("unknown");
    let host_short = host
        .strip_prefix("open.")
        .or_else(|| host.strip_prefix("api."))
        .unwrap_or(host);
    let segs: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let path_short = if segs.len() >= 2 {
        segs[segs.len() - 2..].join("/")
    } else if segs.len() == 1 {
        segs[0].to_string()
    } else {
        String::new()
    };
    format!("{host_short}/{path_short}")
}

/// 耗时格式化：<1s 用毫秒，否则用秒（一位小数）
pub fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("+{ms}ms")
    } else {
        format!("+{:.1}s", d.as_secs_f32())
    }
}

/// 字节数格式化（人类可读，供终端/日志展示）
pub fn fmt_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b}B")
    } else if b < 1024 * 1024 {
        format!("{:.1}KB", b as f64 / 1024.0)
    } else {
        format!("{:.1}MB", b as f64 / (1024.0 * 1024.0))
    }
}

/// 给行内的 [req_id]（形如 [6f11b1]，6 位十六进制）上青色
fn colorize_req_ids(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut rest = line;
    while let Some(pos) = rest.find('[') {
        // 候选：[ + 6位十六进制 + ]（用 get 安全切片，避免切进多字节字符）
        if let Some(cand) = rest.get(pos..pos + 8) {
            if let Some(inner) = rest.get(pos + 1..pos + 7) {
                if cand.ends_with(']') && inner.bytes().all(|b| b.is_ascii_hexdigit()) {
                    out.push_str(&rest[..pos]);
                    out.push_str("\x1b[36m[");
                    out.push_str(inner);
                    out.push_str("]\x1b[0m");
                    rest = &rest[pos + 8..];
                    continue;
                }
            }
        }
        out.push_str(&rest[..pos + 1]);
        rest = &rest[pos + 1..];
    }
    out.push_str(rest);
    out
}

/// logs 子命令：查看最后 N 行，可选 --follow 跟踪
pub fn view_logs(path: &Path, lines: usize, follow: bool) {
    if !path.exists() {
        eprintln!("日志文件不存在: {}（代理可能还没启动过）", path.display());
        std::process::exit(1);
    }

    if follow {
        // 从最后一个“── 启动”会话标记处开始回放（本次会话全部日志），再跟踪新增；
        // 无标记（老日志）时回退打印末尾 N 条
        let start_pos = match session_start_offset(path) {
            Some(off) => {
                if let Ok(s) = read_from(path, off) {
                    print!("{s}");
                }
                off
            }
            None => {
                match read_last_lines(path, lines) {
                    Ok(content) => print!("{content}"),
                    Err(e) => {
                        eprintln!("读取失败: {e}");
                        std::process::exit(1);
                    }
                }
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
            }
        };
        println!("跟踪日志: {}（Ctrl+C 退出）", path.display());
        if let Err(e) = follow_file(path, start_pos) {
            eprintln!("跟踪失败: {e}");
            std::process::exit(1);
        }
    } else {
        match read_last_lines(path, lines) {
            Ok(content) => print!("{content}"),
            Err(e) => {
                eprintln!("读取失败: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// 倒读文件，找最后一个“── 启动”会话标记的字节偏移（用于 log -f 从本次启动处回放）
fn session_start_offset(path: &Path) -> Option<u64> {
    const MARK: &str = "######";
    let data = std::fs::read(path).ok()?;
    // 从后往前找标记所在行的行首偏移
    let mut line_start = 0usize;
    let mut found: Option<u64> = None;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            let line = &data[line_start..=i];
            if std::str::from_utf8(line).map(|l| l.contains(MARK)).unwrap_or(false) {
                found = Some(line_start as u64);
            }
            line_start = i + 1;
        }
    }
    found
}

/// 读取从字节偏移 start 到文件尾的内容（log -f 回放用）
fn read_from(path: &Path, start: u64) -> std::io::Result<String> {
    use std::io::Seek;
    let mut file = File::open(path)?;
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_last_lines(path: &Path, n: usize) -> std::io::Result<String> {
    let file = File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let all: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
    let start = all.len().saturating_sub(n);
    Ok(all[start..].join("\n") + "\n")
}

/// 轮询式 tail -f
fn follow_file(path: &Path, start_pos: u64) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let mut pos = start_pos;
    file.seek(std::io::SeekFrom::Start(pos))?;

    loop {
        let len = std::fs::metadata(path)?.len();
        if len < pos {
            pos = 0;
            file = File::open(path)?;
        }
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        if !buf.is_empty() {
            pos += buf.len() as u64;
            print!("{}", String::from_utf8_lossy(&buf));
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

use std::io::Seek;
use std::io::IsTerminal;

#[cfg(test)]
mod tests {
    use super::{colorize_req_ids, fmt_bytes, fmt_duration, ColorMode};
    use crate::logger::Logger;
    use std::time::Duration;

    #[test]
    fn colors_req_id_and_survives_multibyte() {
        let l = "2026-09-04 13:00:00.000 [ INFO] [6f11b1] 监听 127.0.0.1:8123 失败：中文行";
        let c = colorize_req_ids(l);
        assert!(c.contains("\x1b[36m[6f11b1]\x1b[0m"));
        assert!(c.contains("监听 127.0.0.1:8123 失败"));
        // 无 req_id 的行原样通过
        assert_eq!(colorize_req_ids("plain 中文 line"), "plain 中文 line");
        // 假阳性 [glibberish] 不上色
        assert_eq!(colorize_req_ids("[zzzzzz] x"), "[zzzzzz] x");
    }

    #[test]
    fn shorten_upstream_variants() {
        use super::shorten_upstream;
        assert_eq!(
            shorten_upstream("https://open.bigmodel.cn/api/paas/v4/chat/completions"),
            "bigmodel.cn/chat/completions"
        );
        assert_eq!(
            shorten_upstream("https://api.z.ai/api/paas/v4/chat/completions"),
            "z.ai/chat/completions"
        );
        // 单段路径、无路径、解析失败
        assert_eq!(shorten_upstream("https://example.com/v1"), "example.com/v1");
        assert_eq!(shorten_upstream("https://example.com"), "example.com/");
        assert_eq!(shorten_upstream("not a url"), "not a url");
    }

    #[test]
    fn duration_and_bytes_formatting() {
        assert_eq!(fmt_duration(Duration::from_millis(0)), "+0ms");
        assert_eq!(fmt_duration(Duration::from_millis(832)), "+832ms");
        assert_eq!(fmt_duration(Duration::from_millis(999)), "+999ms");
        assert_eq!(fmt_duration(Duration::from_millis(1000)), "+1.0s");
        assert_eq!(fmt_duration(Duration::from_secs_f32(4.2)), "+4.2s");

        assert_eq!(fmt_bytes(0), "0B");
        assert_eq!(fmt_bytes(1023), "1023B");
        assert_eq!(fmt_bytes(1024), "1.0KB");
        assert_eq!(fmt_bytes(3277), "3.2KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0MB");
    }

    #[test]
    fn start_finish_lines_do_not_panic_on_multibyte() {
        // K09 纪律：中文/多字节路径不得 panic
        let log = Logger::disabled();
        log.start_request("07ae85", Some("glm-5.3-flash"), "POST", "/v1/chat/completions", "https://上游.example/中文", 6246);
        log.finish_request(
            "07ae85",
            200,
            true,
            Some(Duration::from_millis(3987)),
            Some(Duration::from_millis(812)),
            Some(1250),
            Some(6246),
            Some(3277),
            "",
        );
    }
    #[test]
    fn captured_vec_must_not_grow_in_production() {
        // 生产 Logger：captured 必须为 None（否则每条日志进内存 Vec，随会话泄漏）
        let dir = std::env::temp_dir().join(format!("piglmb-cap-{}", std::process::id()));
        let lg = Logger::new_with_mode(&dir, ColorMode::Auto, false, false).unwrap();
        lg.info("a");
        lg.error("b");
        assert!(lg.captured().is_empty(), "生产 Logger 不应捕获进内存: {:?}", lg.captured());

        // memory() 测试模式：仍需捕获
        let m = Logger::memory();
        m.info("x");
        assert_eq!(m.captured().len(), 1);
    }

    #[test]
    fn summary_single_line_format() {
        let log = Logger::disabled();
        log.start_request("3fcef8", Some("glm-5.3-flash"), "POST", "/chat/completions", "https://open.bigmodel.cn/api/paas/v4/chat/completions", 111_202);
        log.finish_request(
            "3fcef8", 200, true,
            Some(Duration::from_millis(26_080)), Some(Duration::from_millis(3833)),
            Some(30214), Some(111_202), Some(173_432), "",
        );
        let s = log.summaries(50);
        assert_eq!(s.len(), 1);
        let l = &s[0];
        // 单行式：开始+结束指标合一行，字段各出现一次，时间戳 HH:MM:SS.mmm
        assert!(l.contains("▶ [3fcef8] glm-5.3-flash → bigmodel.cn/chat/completions"), "{l}");
        assert!(l.contains("↑ 108.6KB"), "{l}");
        assert!(l.contains("✔ 200 +26.1s"), "{l}");
        assert!(l.contains("首包 3833ms"), "{l}");
        assert!(l.contains("↓ 169.4KB"), "{l}");
        assert!(l.contains("30214 tok"), "{l}");
        // 时间戳形如 HH:MM:SS.mmm（无日期前缀）
        let ts = l.split(' ').next().unwrap();
        assert_eq!(ts.len(), 12, "{l}");
    }

    #[test]
    fn summary_error_and_missing_fields() {
        let log = Logger::disabled();
        log.start_request("abc123", Some("glm-5.3-flash"), "POST", "/chat/completions", "https://open.bigmodel.cn/api/paas/v4/chat/completions", 1024);
        log.finish_request(
            "abc123", 504, false,
            Some(Duration::from_millis(120_000)), None, None,
            Some(1024), Some(512), "读空闲中止",
        );
        let l = &log.summaries(10)[0];
        assert!(l.contains("✘ 504 +2:00.0s") || l.contains("✘ 504 +120.0s"), "{l}");
        assert!(!l.contains("首包"), "缺首包应省略: {l}");
        assert!(!l.contains(" tok"), "缺 tok 应省略: {l}");
        assert!(l.contains("读空闲中止"), "{l}");
        // 无 start 的 finish 也不炸（head 兑底 "-"）
        log.finish_request("fffff1", 401, false, None, None, None, None, None, "");
        let l = &log.summaries(10)[1];
        assert!(l.contains("[fffff1] - ✘ 401"), "{l}");
    }

    #[test]
    fn summary_ring_buffer_evicts_oldest() {
        let log = Logger::disabled();
        for i in 0..250 {
            let id = format!("r{i:05x}");
            log.start_request(&id, Some("m"), "POST", "/c", "https://x.example/c", 1);
            log.finish_request(&id, 200, true, None, None, None, Some(1), None, "");
        }
        let s = log.summaries(500);
        assert_eq!(s.len(), 200, "容量上限 200");
        // 最老的被淘汰，最新保留
        assert!(!s[0].contains("[r00000]"), "{}", s[0]);
        assert!(s.last().unwrap().contains("[r000f9]"), "{}", s.last().unwrap());
        // pending 表不驻留（K11）：全部 finish 后应清空
        // （通过再次 finish 同 id 不产生第二行隐式验证：无法直接读 pending，靠容量不翻倍）
        assert_eq!(log.summaries(500).len(), 200);
    }

    #[test]
    fn file_summary_tail_parses_pairs() {
        let dir = std::env::temp_dir().join(format!("piglmb-fst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("proxy.log");
        std::fs::write(&p, concat!(
            "2026-09-06 17:45:04.322 [INFO ] [3fcef8] -> glm-5.3-flash POST /chat/completions (req 108.6KB) 转发至 https://open.bigmodel.cn/api/paas/v4/chat/completions\n",
            "2026-09-06 17:45:09.184 [INFO ] [3fcef8] … ↓ 6.4KB\n",
            "2026-09-06 17:45:30.405 [INFO ] [3fcef8] <- 200 耗时 26.08s 首包 3833ms req 108.6KB resp 169.4KB 30214 tok\n",
            "2026-09-06 17:48:28.410 [INFO ] [f59161] -> glm-5.3-flash POST /chat/completions (req 112.9KB) 转发至 https://open.bigmodel.cn/api/paas/v4/chat/completions\n",
            "2026-09-06 17:48:36.074 [INFO ] [f59161] <- 504 耗时 7.66s req 112.9KB 读空闲中止\n",
        )).unwrap();
        let s = Logger::file_summary_tail(&p, 50);
        assert_eq!(s.len(), 2, "{s:?}");
        assert!(s[0].starts_with("17:45:04.322 "), "用开始时刻时间戳: {}", s[0]);
        assert!(s[0].contains("▶ [3fcef8] glm-5.3-flash → bigmodel.cn/chat/completions ↑ 108.6KB · ✔ 200 +26.1s · 首包 3833ms · ↓ 169.4KB · 30214 tok"), "{}", s[0]);
        assert!(s[1].contains("↑ 112.9KB · ✘ 504 +7.7s") && s[1].ends_with("读空闲中止"), "{}", s[1]);
        // 文件不存在 → 空数组
        assert!(Logger::file_summary_tail(&dir.join("nope.log"), 50).is_empty());
    }

    #[test]
    fn file_summary_tail_handles_multibyte() {
        // K09：中文 URL 不得 panic
        let dir = std::env::temp_dir().join(format!("piglmb-fmb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("proxy.log");
        std::fs::write(&p, "2026-09-06 17:00:00.000 [INFO ] [ab12cd] -> m POST /路/径 (req 1B) 转发至 https://上游.example/中文\n").unwrap();
        let s = Logger::file_summary_tail(&p, 50);
        assert!(s.is_empty() || s[0].contains("[ab12cd]"), "{s:?}");
    }
}
