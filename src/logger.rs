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

#[derive(Clone)]
pub struct Logger {
    inner: Option<Arc<Mutex<File>>>,
    disabled: bool,
    file_only: bool, // daemon 模式：只写文件，不污染终端
    color: bool,
}

impl Logger {
    /// 初始化：确保目录存在，轮转旧日志（proxy.log -> proxy.log.1，仅保留一份）
    pub fn new_with_mode(log_dir: &Path, mode: ColorMode, file_only: bool) -> std::io::Result<Self> {
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
        })
    }

    pub fn disabled() -> Self {
        Self { inner: None, disabled: true, file_only: false, color: false }
    }

    fn write_line(&self, level: &str, msg: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
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
    use super::colorize_req_ids;

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
}
