//! 定时任务：字段、默认值与旧 `bridge.js` 完全一致。

use crate::util;
use crate::{bot, config, remote, App};
use chrono::Timelike;
use serde_json::{json, Value};
use std::sync::Arc;

fn load_tasks(app: &Arc<App>) -> (Value, Vec<Value>) {
    let cfg = config::read_config(&app.paths);
    let tasks = match cfg["scheduledTasks"].as_array() {
        Some(a) => a.clone(),
        None => Vec::new(),
    };
    (cfg, tasks)
}

fn save_tasks(app: &Arc<App>, cfg: &mut Value, tasks: &[Value]) {
    cfg["scheduledTasks"] = Value::Array(tasks.to_vec());
    config::write_config(&app.paths, cfg);
}

/// 旧实现的 `clampHour(v, def)`：parseInt 后必须在 0..=23，否则用默认值。
fn clamp_hour(v: &Value, dflt: i64) -> i64 {
    match util::js_parse_int(v) {
        Some(n) if (0..=23).contains(&n) => n,
        _ => dflt,
    }
}

pub fn list(app: &Arc<App>) -> Value {
    let (_, tasks) = load_tasks(app);
    json!({ "tasks": tasks })
}

fn task_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ ((std::process::id() as u64) << 17) ^ (util::now_ms() as u64);
    let suffix = util::base36(mixed);
    let suffix: String = suffix.chars().rev().take(5).collect::<String>().chars().rev().collect();
    format!("t_{}{}", util::base36(util::now_ms() as u64), suffix)
}

async fn run_task(app: &Arc<App>, task: &Value) -> Value {
    let target_type = if util::js_string(&task["targetType"]) == "group" { "group" } else { "c2c" };
    let target = util::js_string(&task["targetId"]);
    let content = util::js_string(&task["content"]);
    match remote::send_qq_message(app, target_type, &target, &content).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

pub fn create(app: &Arc<App>, body: &Value) -> (u16, Value) {
    let (mut cfg, mut tasks) = load_tasks(app);
    let now = util::now_ms();
    let interval = util::js_parse_int(&body["intervalMinutes"]).unwrap_or(60).max(1);
    let task = json!({
        "id": task_id(),
        "name": util::js_or_string(&body["name"], "未命名任务").chars().take(50).collect::<String>(),
        "enabled": body.get("enabled").map(|v| v != &Value::Bool(false)).unwrap_or(true),
        "targetType": if util::js_string(&body["targetType"]) == "group" { "group" } else { "c2c" },
        "targetId": util::js_string(&body["targetId"]).trim().to_string(),
        "content": util::js_string(&body["content"]),
        "intervalMinutes": interval,
        "startHour": clamp_hour(&body["startHour"], 8),
        "endHour": clamp_hour(&body["endHour"], 23),
        "nextRun": if util::truthy(&body["nextRun"]) {
            util::js_string(&body["nextRun"])
        } else {
            util::iso_from_ms(now + interval * 60_000)
        },
        "lastRun": Value::Null,
    });
    let target_id = util::js_string(&task["targetId"]);
    let content = util::js_string(&task["content"]);
    if target_id.is_empty() || content.is_empty() {
        return (400, json!({ "error": "targetId 和 content 必填" }));
    }
    tasks.push(task.clone());
    save_tasks(app, &mut cfg, &tasks);
    (200, json!({ "ok": true, "task": task }))
}

pub fn update(app: &Arc<App>, id: &str, body: &Value) -> (u16, Value) {
    let (mut cfg, mut tasks) = load_tasks(app);
    let Some(t) = tasks.iter_mut().find(|t| util::js_string(&t["id"]) == id) else {
        return (404, json!({ "error": "task not found" }));
    };
    if body.get("name").is_some() {
        t["name"] = Value::String(util::js_string(&body["name"]).chars().take(50).collect());
    }
    if body.get("enabled").is_some() {
        t["enabled"] = Value::Bool(util::truthy(&body["enabled"]));
    }
    if body.get("targetType").is_some() {
        t["targetType"] = Value::String(
            if util::js_string(&body["targetType"]) == "group" { "group" } else { "c2c" }.into(),
        );
    }
    if body.get("targetId").is_some() {
        t["targetId"] = Value::String(util::js_string(&body["targetId"]).trim().to_string());
    }
    if body.get("content").is_some() {
        t["content"] = Value::String(util::js_string(&body["content"]));
    }
    if body.get("intervalMinutes").is_some() {
        let iv = util::js_parse_int(&body["intervalMinutes"]).unwrap_or(60).max(1);
        t["intervalMinutes"] = json!(iv);
    }
    if body.get("startHour").is_some() {
        let cur = t["startHour"].as_i64().unwrap_or(8);
        let v = clamp_hour(&body["startHour"], cur);
        t["startHour"] = json!(v);
    }
    if body.get("endHour").is_some() {
        let cur = t["endHour"].as_i64().unwrap_or(23);
        let v = clamp_hour(&body["endHour"], cur);
        t["endHour"] = json!(v);
    }
    if body.get("nextRun").is_some() {
        t["nextRun"] = body["nextRun"].clone();
    }
    let out = t.clone();
    save_tasks(app, &mut cfg, &tasks);
    (200, json!({ "ok": true, "task": out }))
}

pub fn remove(app: &Arc<App>, id: &str) -> (u16, Value) {
    let (mut cfg, mut tasks) = load_tasks(app);
    let before = tasks.len();
    tasks.retain(|t| util::js_string(&t["id"]) != id);
    if tasks.len() == before {
        return (404, json!({ "error": "task not found" }));
    }
    save_tasks(app, &mut cfg, &tasks);
    (200, json!({ "ok": true }))
}

pub async fn run_now(app: &Arc<App>, id: &str) -> (u16, Value) {
    let (mut cfg, mut tasks) = load_tasks(app);
    let Some(idx) = tasks.iter().position(|t| util::js_string(&t["id"]) == id) else {
        return (404, json!({ "error": "task not found" }));
    };
    let r = run_task(app, &tasks[idx]).await;
    if util::truthy(&r["ok"]) {
        tasks[idx]["lastRun"] = Value::String(util::iso_now());
        save_tasks(app, &mut cfg, &tasks);
    }
    (200, r)
}

fn in_window(t: &Value, hour: i64) -> bool {
    let s = t["startHour"].as_i64().unwrap_or(8);
    let e = t["endHour"].as_i64().unwrap_or(23);
    if s == e {
        return true;
    }
    if s < e {
        hour >= s && hour < e
    } else {
        hour >= s || hour < e
    }
}

pub async fn tick(app: &Arc<App>) {
    let (mut cfg, mut tasks) = load_tasks(app);
    if tasks.is_empty() {
        return;
    }
    let now = util::now_ms();
    let hour = chrono::Local::now().hour() as i64;
    let mut changed = false;

    for t in tasks.iter_mut() {
        if !util::truthy(&t["enabled"]) {
            continue;
        }
        let Some(next) = util::parse_iso_ms(&util::js_string(&t["nextRun"])) else {
            continue;
        };
        if next > now {
            continue;
        }
        let name = util::js_or_string(&t["name"], &util::js_string(&t["id"]));
        if in_window(t, hour) {
            let a = app.clone();
            let task = t.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let r = run_task(&a, &task).await;
                let msg = if util::truthy(&r["ok"]) {
                    "成功".to_string()
                } else {
                    format!("失败: {}", util::js_string(&r["error"]))
                };
                bot::push_line(&a, &format!("[定时] {name} → {msg}"));
            });
        } else {
            bot::push_line(app, &format!("[定时] {name} → 非发送时段，跳过"));
        }
        let ivm = util::js_parse_int(&t["intervalMinutes"]).unwrap_or(60).max(1);
        let iv = (ivm * 60_000).max(60_000);
        t["lastRun"] = Value::String(util::iso_from_ms(now));
        t["nextRun"] = Value::String(util::iso_from_ms(now + iv));
        changed = true;
    }

    if changed {
        save_tasks(app, &mut cfg, &tasks);
    }
}

pub fn start(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tick(&app).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}
