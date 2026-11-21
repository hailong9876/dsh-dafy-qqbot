//! Agent Lightning 三组件（Gateway / Controller / Trainer）启停与状态。
//!
//! 与旧 `bridge.js` 的 agl 段逐项对齐：目录、pid/log 文件、启动命令、
//! 返回字段（含 tails 与 rollouts 统计）都保持一致。

use crate::util;
use crate::{config, App};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct Comp {
    log: PathBuf,
    pid: PathBuf,
}

struct AglPaths {
    dir: PathBuf,
    svc: PathBuf,
    exe: PathBuf,
    gateway: Comp,
    controller: Comp,
    trainer: Comp,
    port: u16,
    trainer_cmd: String,
    installed: bool,
}

fn resolve_path(p: &str, base: &Path) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        base.join(pb)
    }
}

fn agl_paths(app: &Arc<App>) -> AglPaths {
    let cfg = config::read_config(&app.paths);
    let base = app
        .paths
        .root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| app.paths.root.clone());
    let dir = if util::truthy(&cfg["aglDir"]) {
        resolve_path(&util::js_string(&cfg["aglDir"]), &base)
    } else {
        base.join("agent-lightning")
    };
    let svc = dir.join("service");
    let port = util::js_number(&cfg["aglGatewayPort"])
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as u16)
        .unwrap_or(8080);
    let exe = dir.join(".venv").join("Scripts").join("python.exe");
    AglPaths {
        gateway: Comp {
            log: svc.join("gateway.log"),
            pid: svc.join("gateway.pid"),
        },
        controller: Comp {
            log: svc.join("controller.log"),
            pid: svc.join("controller.pid"),
        },
        trainer: Comp {
            log: svc.join("trainer.log"),
            pid: svc.join("trainer.pid"),
        },
        installed: exe.is_file(),
        trainer_cmd: util::js_string(&cfg["aglTrainerCmd"]),
        port,
        exe,
        svc,
        dir,
    }
}

fn agl_read_pid(file: &Path) -> u32 {
    match util::read_text(file)
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
    {
        Some(n) if util::pid_alive(n) => n,
        _ => 0,
    }
}

async fn probe(app: &Arc<App>, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/healthz");
    match app
        .http
        .get(&url)
        .timeout(Duration::from_millis(1500))
        .send()
        .await
    {
        Ok(r) => r.status().as_u16() == 200,
        Err(_) => false,
    }
}

async fn http_json(app: &Arc<App>, port: u16, path: &str, timeout: Duration) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = app.http.get(&url).timeout(timeout).send().await.ok()?;
    let text = resp.text().await.ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

async fn fetch_rollouts(app: &Arc<App>, port: u16) -> Option<Value> {
    let q = "/api/rollouts?state_in=queuing&state_in=running&state_in=succeeded&state_in=failed&limit=500";
    let list = http_json(app, port, q, Duration::from_millis(3000)).await?;
    let list = list.as_array()?.clone();
    let mut by_status = Map::new();
    for r in &list {
        let s = util::js_or_string(&r["status"]["state"], "unknown");
        let cur = by_status.get(&s).and_then(|v| v.as_i64()).unwrap_or(0);
        by_status.insert(s, json!(cur + 1));
    }
    let done: Vec<&Value> = list
        .iter()
        .filter(|r| {
            let s = util::js_string(&r["status"]["state"]);
            s == "succeeded" || s == "failed"
        })
        .collect();
    let recent = &done[done.len().saturating_sub(20)..];
    let mut rewards: Vec<Value> = Vec::new();
    for r in recent {
        let rid = {
            let a = util::js_string(&r["rollout_id"]);
            if a.is_empty() {
                util::js_string(&r["id"])
            } else {
                a
            }
        };
        if rid.is_empty() {
            continue;
        }
        let path = format!(
            "/api/rollouts/{}/events",
            util::percent_encode(&rid)
        );
        let Some(evs) = http_json(app, port, &path, Duration::from_millis(2000)).await else {
            continue;
        };
        let Some(arr) = evs.as_array() else { continue };
        let last = arr
            .iter()
            .filter(|e| {
                util::js_string(&e["event_type"]) == "reward" && e["payload"]["value"].is_number()
            })
            .last();
        if let Some(e) = last {
            if let Some(v) = e["payload"]["value"].as_f64() {
                rewards.push(json!({ "id": rid, "reward": v }));
            }
        }
    }
    if rewards.len() > 100 {
        rewards = rewards[rewards.len() - 100..].to_vec();
    }
    Some(json!({
        "total": list.len(),
        "byStatus": Value::Object(by_status),
        "rewards": rewards,
    }))
}

pub async fn status(app: &Arc<App>) -> Value {
    let p = agl_paths(app);
    let gw_pid = agl_read_pid(&p.gateway.pid);
    let ct_pid = agl_read_pid(&p.controller.pid);
    let tr_pid = agl_read_pid(&p.trainer.pid);
    let gateway_up = if gw_pid > 0 { probe(app, p.port).await } else { false };
    let rollouts = if gateway_up {
        fetch_rollouts(app, p.port).await
    } else {
        None
    };
    json!({
        "ok": true,
        "installed": p.installed,
        "dir": p.dir.display().to_string(),
        "port": p.port,
        "trainerCmd": p.trainer_cmd,
        "gateway": {
            "running": gateway_up,
            "pid": if gw_pid > 0 { json!(gw_pid) } else { Value::Null },
            "tail": util::tail_lines(&p.gateway.log, 262_144, 60),
        },
        "controller": {
            "running": ct_pid > 0,
            "pid": if ct_pid > 0 { json!(ct_pid) } else { Value::Null },
            "tail": util::tail_lines(&p.controller.log, 262_144, 60),
        },
        "trainer": {
            "running": tr_pid > 0,
            "pid": if tr_pid > 0 { json!(tr_pid) } else { Value::Null },
            "tail": util::tail_lines(&p.trainer.log, 262_144, 60),
            "supported": !p.trainer_cmd.is_empty(),
        },
        "rollouts": rollouts,
    })
}

fn spawn_logged(
    exe: &Path,
    args: Vec<String>,
    cwd: &Path,
    log: &Path,
    extra_env: &[(&str, String)],
) -> std::io::Result<u32> {
    if let Some(dir) = log.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let f = fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = f.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(f))
        .stderr(Stdio::from(err))
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd.spawn()?.id())
}

pub fn start_component(app: &Arc<App>, name: &str) -> Value {
    let p = agl_paths(app);
    if !p.installed {
        return json!({
            "ok": false,
            "error": format!("没有找到 Agent Lightning 环境：{}（.venv 未安装）", p.dir.display()),
        });
    }
    match name {
        "gateway" => {
            if agl_read_pid(&p.gateway.pid) > 0 {
                return json!({ "ok": true, "already": true });
            }
            let args = vec![
                "-m".into(),
                "agentlightning.server".into(),
                "host=127.0.0.1".into(),
                format!("port={}", p.port),
            ];
            match spawn_logged(&p.exe, args, &p.svc, &p.gateway.log, &[]) {
                Ok(pid) => {
                    let _ = fs::write(&p.gateway.pid, pid.to_string());
                    json!({ "ok": true, "started": true, "pid": pid })
                }
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        "controller" => {
            if agl_read_pid(&p.controller.pid) > 0 {
                return json!({ "ok": true, "already": true });
            }
            let args = vec![
                "-m".into(),
                "agentlightning.controller".into(),
                "runner_type=local".into(),
                format!("agl_server.url=http://127.0.0.1:{}", p.port),
            ];
            match spawn_logged(&p.exe, args, &p.svc, &p.controller.log, &[]) {
                Ok(pid) => {
                    let _ = fs::write(&p.controller.pid, pid.to_string());
                    json!({ "ok": true, "started": true, "pid": pid })
                }
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        "trainer" => {
            if p.trainer_cmd.is_empty() {
                return json!({
                    "ok": false,
                    "error": "本机显卡（4GB）跑不了 verl 训练。请在 config.json 配置 aglTrainerCmd 连接远程 GPU 训练后再启动。",
                });
            }
            if agl_read_pid(&p.trainer.pid) > 0 {
                return json!({ "ok": true, "already": true });
            }
            if let Some(dir) = p.trainer.log.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let log = match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p.trainer.log)
            {
                Ok(f) => f,
                Err(e) => return json!({ "ok": false, "error": e.to_string() }),
            };
            let err = match log.try_clone() {
                Ok(f) => f,
                Err(e) => return json!({ "ok": false, "error": e.to_string() }),
            };
            let mut cmd = Command::new("cmd.exe");
            cmd.arg("/c")
                .arg(&p.trainer_cmd)
                .current_dir(&p.dir)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(err));
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            match cmd.spawn() {
                Ok(child) => {
                    let pid = child.id();
                    let _ = fs::write(&p.trainer.pid, pid.to_string());
                    json!({ "ok": true, "started": true, "pid": pid })
                }
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        other => json!({ "ok": false, "error": format!("未知组件：{other}") }),
    }
}

pub fn stop_component(app: &Arc<App>, name: &str) -> Value {
    let p = agl_paths(app);
    let comp = match name {
        "gateway" => &p.gateway,
        "controller" => &p.controller,
        "trainer" => &p.trainer,
        other => return json!({ "ok": false, "error": format!("未知组件：{other}") }),
    };
    let pid = agl_read_pid(&comp.pid);
    let _ = fs::remove_file(&comp.pid);
    if pid == 0 {
        return json!({ "ok": true, "already": true });
    }
    dafy_core::process::kill_tree(pid);
    json!({ "ok": true, "stopping": true, "pid": pid })
}

pub async fn start_all(app: &Arc<App>) -> Value {
    let g = start_component(app, "gateway");
    if !util::truthy(&g["ok"]) {
        return g;
    }
    if !util::truthy(&g["already"]) {
        let p = agl_paths(app);
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if probe(app, p.port).await {
                break;
            }
        }
    }
    let c = start_component(app, "controller");
    let mut cfg = config::read_config(&app.paths);
    cfg["aglAutoStart"] = Value::Bool(true);
    config::write_config(&app.paths, &cfg);
    json!({ "ok": true, "gateway": g, "controller": c })
}

pub fn stop_all(app: &Arc<App>) -> Value {
    let c = stop_component(app, "controller");
    let g = stop_component(app, "gateway");
    let mut cfg = config::read_config(&app.paths);
    cfg["aglAutoStart"] = Value::Bool(false);
    config::write_config(&app.paths, &cfg);
    json!({ "ok": true, "gateway": g, "controller": c })
}

pub fn start_trainer(app: &Arc<App>) -> Value {
    start_component(app, "trainer")
}
