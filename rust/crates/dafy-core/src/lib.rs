//! 大肥鱼 Rust 化公共库：日志、探活、进程启动、单实例。
//!
//! 只放与具体业务无关的基础能力，保证各服务重写时行为一致。

pub mod logger;
pub mod net;
pub mod process;
pub mod single;

use std::path::{Path, PathBuf};

/// 项目根目录（大肥鱼目录本身）。
///
/// 优先用 `DAFY_ROOT` 环境变量；否则以当前可执行文件位置向上一级寻找
/// 同时包含 `index.html` 与 `keep_alive.js` 的目录；再退回当前工作目录。
pub fn project_root() -> PathBuf {
    if let Ok(v) = std::env::var("DAFY_ROOT") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.as_path();
        for _ in 0..6 {
            if let Some(dir) = cur.parent() {
                if is_project_root(dir) {
                    return dir.to_path_buf();
                }
                cur = dir;
            } else {
                break;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn is_project_root(dir: &Path) -> bool {
    dir.join("index.html").is_file() && dir.join("keep_alive.js").is_file()
}
