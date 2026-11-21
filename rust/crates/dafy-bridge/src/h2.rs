//! Harness2 训练进程管理（与旧 `bridge.js` 的 h2 段逐行对齐）。
//!
//! Rust 侧只负责启停与状态展示，真实训练由 Python `service/loop.py` 完成。

use crate::util;
use crate::{config, App};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct H2Paths {
    dir: PathBuf,
    state: PathBuf,
    log: PathBuf,
    pid: PathBuf,
    stop_flag: PathBuf,
    exe: PathBuf,
    script: PathBuf,
    svc: PathBuf,
}

fn resolve_path(p: &str, base: &Path) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        base.join(pb)
    }
}

fn h2_paths(app: &Arc<App>) -> H2Paths {
    let cfg = config::read_config(&app.paths);
    let base = app
        .paths
        .root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| app.paths.root.clone());
    let dir = if util::truthy(&cfg["harness2Dir"]) {
        resolve_path(&util::js_string(&cfg["harness2Dir"]), &base)
    } else {
        base.join("harness2")
    };
    let svc = dir.join("service");
    H2Paths {
        exe: dir.join(".venv").join("Scripts").join("python.exe"),
        script: svc.join("loop.py"),
        state: svc.join("state.json"),
        log: svc.join("service.log"),
        stop_flag: svc.join("stop.flag"),
        pid: svc.join("service.pid"),
        svc,
        dir,
    }
}

fn read_state(path: &Path) -> Option<Value> {
    serde_json::from_str::<Value>(&util::read_text(path)?).ok()
}

fn h2_pid(p: &H2Paths) -> u32 {
    let mut cands: Vec<u32> = Vec::new();
    if let Some(st) = read_state(&p.state) {
        if util::truthy(&st["running"]) {
            if let Some(n) = util::js_parse_int(&st["pid"]) {
                if n > 0 {
                    cands.push(n as u32);
                }
            }
        }
    }
    if let Some(n) = util::read_text(&p.pid)
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
    {
        cands.push(n);
    }
    cands
        .iter()
        .find(|n| util::pid_alive(**n))
        .copied()
        .or_else(|| cands.first().copied())
        .unwrap_or(0)
}

fn h2_running(p: &H2Paths) -> bool {
    let Some(st) = read_state(&p.state) else {
        return false;
    };
    if !util::truthy(&st["running"]) {
        return false;
    }
    match util::js_parse_int(&st["pid"]) {
        Some(n) if n > 0 => util::pid_alive(n as u32),
        _ => false,
    }
}

/// `cfg.x || dflt`，把值按 JS 语义转成字符串。
fn cfg_or(cfg: &Value, key: &str, dflt: &str) -> String {
    let v = &cfg[key];
    if !util::truthy(v) {
        dflt.to_string()
    } else {
        util::js_string(v)
    }
}

pub async fn status(app: &Arc<App>) -> Value {
    let p = h2_paths(app);
    let installed = p.exe.is_file() && p.script.is_file();
    let running = h2_running(&p);
    let state = read_state(&p.state);
    let st_pid = state
        .as_ref()
        .filter(|s| util::truthy(&s["running"]))
        .and_then(|s| util::js_parse_int(&s["pid"]))
        .filter(|n| *n > 0)
        .map(|n| n as u32);
    let pid = st_pid.or_else(|| {
        let n = h2_pid(&p);
        if n > 0 {
            Some(n)
        } else {
            None
        }
    });
    let engine = state
        .as_ref()
        .map(|s| s["engine"].clone())
        .filter(|v| util::truthy(v));
    let tail = util::tail_lines(&p.log, 262_144, 80);
    json!({
        "ok": true,
        "installed": installed,
        "running": running,
        "pid": pid,
        "dir": p.dir.display().to_string(),
        "engine": engine,
        "state": state.unwrap_or(Value::Null),
        "tail": tail,
    })
}

pub async fn start(app: &Arc<App>) -> Value {
    let p = h2_paths(app);
    if !p.exe.is_file() || !p.script.is_file() {
        return json!({
            "ok": false,
            "error": format!("没有找到 Harness² 安装：{}", p.dir.display()),
        });
    }
    if h2_running(&p) {
        return json!({ "ok": true, "already": true, "pid": h2_pid(&p) });
    }
    if let Err(e) = fs::create_dir_all(&p.svc) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    let _ = fs::remove_file(&p.stop_flag);
    app.h2_epoch.fetch_add(1, Ordering::SeqCst);
    let _ = fs::write(&p.log, b"");

    let cfg = config::read_config(&app.paths);
    let engine = cfg_or(&cfg, "harness2Engine", "real");
    let model = cfg_or(&cfg, "harness2Model", "dafy/mimo-v2.6-flash-free");
    let path_env = format!(
        "{};{}",
        p.dir
            .join("dependencies")
            .join("tools")
            .join("node_modules")
            .join(".bin")
            .display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut cmd = Command::new(&p.exe);
    cmd.arg(&p.script)
        .arg("--root")
        .arg(&p.dir)
        .arg("--engine")
        .arg(&engine)
        .current_dir(&p.dir)
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .env("H2_ENGINE", &engine)
        .env("H2_STEPS", cfg_or(&cfg, "harness2Steps", "40"))
        .env("H2_ROLLOUT_STEPS", cfg_or(&cfg, "harness2RolloutSteps", "12"))
        .env("H2_ROLLOUT_TIMEOUT", cfg_or(&cfg, "harness2RolloutTimeout", "180"))
        .env("H2_ROUND_TIMEOUT", cfg_or(&cfg, "harness2RoundTimeout", "1800"))
        .env("H2_TASKS", cfg_or(&cfg, "harness2Tasks", ""))
        .env("H2_MODE", cfg_or(&cfg, "harness2Mode", "both"))
        .env("HARNESS2_SOLVER_MODEL", &model)
        .env(
            "HARNESS2_IMPROVER_MODEL",
            cfg_or(&cfg, "harness2ImproverModel", &model),
        )
        .env("DAFY_GW_KEY", cfg_or(&cfg, "deepseekApiKey", ""))
        .env("OPENCODE_CONFIG", p.svc.join("opencode.json"))
        .env("PATH", path_env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            let _ = fs::write(&p.pid, pid.to_string());
            json!({ "ok": true, "started": true, "pid": pid })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

pub async fn stop(app: &Arc<App>) -> Value {
    let p = h2_paths(app);
    let pid = h2_pid(&p);
    if !util::pid_alive(pid) {
        return json!({ "ok": true, "already": true });
    }
    let _ = fs::write(&p.stop_flag, b"");
    let epoch = app.h2_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(12)).await;
        if a.h2_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        if util::pid_alive(pid) {
            dafy_core::process::kill_tree(pid);
        }
        let _ = fs::remove_file(h2_paths(&a).stop_flag);
    });
    json!({ "ok": true, "stopping": true, "pid": pid })
}
