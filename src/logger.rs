//! 日志模块：写入文件 + 终端输出（TTY 自动着色，管道输出纯文本），支持 logs 子命令查看/跟踪

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

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
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(file))),
            disabled: false,
            file_only,
            color,
            level: if debug_on { LogLevel::Debug } else { LogLevel::Info },
            status_shown: Arc::new(Mutex::new(false)),
            captured: None,
        })
    }

    pub fn disabled() -> Self {
        Self { inner: None, disabled: true, file_only: false, color: false, level: LogLevel::Info, status_shown: Arc::new(Mutex::new(false)), captured: None }
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
        }
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
        println!("跟踪日志: {}（Ctrl+C 退出）", path.display());
        if let Err(e) = follow_file(path) {
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

fn read_last_lines(path: &Path, n: usize) -> std::io::Result<String> {
    let file = File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let all: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
    let start = all.len().saturating_sub(n);
    Ok(all[start..].join("\n") + "\n")
}

/// 轮询式 tail -f
fn follow_file(path: &Path) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let mut pos = std::fs::metadata(path)?.len();
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
}
