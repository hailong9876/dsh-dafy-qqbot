//! 子进程启动：隐藏窗口、日志按标签转发，保留旧脚本的进程语义。
//!
//! 刻意不启用 Job Object：现有行为是「守护进程退出后子服务继续跑」，
//! 贸然改成随守护进程退出会改变用户习惯；需要清理时用 `kill_tree`。

use crate::logger::Logger;
use anyhow::Context;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// CREATE_NO_WINDOW：等价旧 Node 脚本的 `windowsHide: true`，后台服务不弹黑窗。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct SpawnSpec<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub envs: &'a [(String, String)],
    pub tag: &'a str,
}

/// 启动子进程并转发输出，返回 PID。
pub fn spawn_hidden(spec: SpawnSpec<'_>, logger: Arc<Logger>) -> anyhow::Result<u32> {
    let mut cmd = Command::new(spec.program);
    cmd.args(spec.args)
        .current_dir(spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in spec.envs {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动 {} 失败: {}", spec.tag, spec.program.display()))?;
    let pid = child.id();

    if let Some(out) = child.stdout.take() {
        spawn_reader(out, spec.tag.to_string(), logger.clone(), false);
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, spec.tag.to_string(), logger, true);
    }

    // 子进程句柄立即释放，进程继续在后台运行（Windows 特性）。
    std::mem::drop(child);
    Ok(pid)
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    tag: String,
    logger: Arc<Logger>,
    is_err: bool,
) {
    std::thread::spawn(move || {
        let mut buf = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match buf.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = line.trim_end_matches(['\r', '\n']);
                    if text.is_empty() {
                        continue;
                    }
                    if is_err {
                        logger.tagged(&format!("{tag}-err"), text);
                    } else {
                        logger.tagged(&tag, text);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// 结束整棵进程树（用于显式清理，默认不启用）。
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(windows))]
pub fn kill_tree(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}
