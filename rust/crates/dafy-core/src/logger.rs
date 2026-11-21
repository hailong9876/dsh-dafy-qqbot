//! 轻量日志：控制台 + 按天滚动的文件，格式与旧 Node 脚本的 `[tag] msg` 保持一致，
//! 方便新旧版本日志直接对比。

use anyhow::Context;
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Logger {
    dir: PathBuf,
    prefix: String,
    /// (日期, 文件句柄)
    file: Mutex<Option<(String, BufWriter<File>)>>,
    console: bool,
}

impl Logger {
    pub fn init(dir: impl AsRef<Path>, prefix: &str) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        create_dir_all(&dir).with_context(|| format!("创建日志目录失败: {}", dir.display()))?;
        Ok(Self {
            dir,
            prefix: prefix.to_string(),
            file: Mutex::new(None),
            console: true,
        })
    }

    /// 只输出到控制台，不落文件（用于 --once 这类一次性命令）。
    pub fn console_only(prefix: &str) -> Self {
        Self {
            dir: PathBuf::new(),
            prefix: prefix.to_string(),
            file: Mutex::new(None),
            console: true,
        }
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        self.write("info", msg.as_ref());
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        self.write("warn", msg.as_ref());
    }

    pub fn error(&self, msg: impl AsRef<str>) {
        self.write("error", msg.as_ref());
    }

    /// 带固定标签输出，例如 `[bridge] 浏览器已启动`。
    pub fn tagged(&self, tag: &str, msg: &str) {
        self.write("info", &format!("[{tag}] {msg}"));
    }

    fn write(&self, level: &str, msg: &str) {
        let now = chrono::Local::now();
        let line = format!("[{}] [{level}] {msg}\n", now.format("%Y-%m-%d %H:%M:%S"));
        if self.console {
            print!("{line}");
            let _ = std::io::stdout().flush();
        }
        if self.dir.as_os_str().is_empty() {
            return;
        }
        let day = now.format("%Y-%m-%d").to_string();
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let need_open = match guard.as_ref() {
            Some((d, _)) => d != &day,
            None => true,
        };
        if need_open {
            let path = self.dir.join(format!("{}-{day}.log", self.prefix));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => *guard = Some((day.clone(), BufWriter::new(f))),
                Err(_) => return,
            }
        }
        if let Some((_, w)) = guard.as_mut() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    }
}
