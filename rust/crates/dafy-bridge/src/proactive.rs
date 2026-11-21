//! 主动发言：到点让模型判断要不要说话，避免刷屏。

use crate::util;
use crate::{bot, config, remote, App};
use chrono::{Timelike, Utc};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub fn proactive_cfg(app: &Arc<App>) -> Value {
    let cfg = config::read_config(&app.paths);
    let p = &cfg["proactive"];
    let fallback = if p["fallbackModels"].is_array() {
        util::string_array(&p["fallbackModels"])
    } else {
        vec!["big-pickle".to_string(), "pulse-alpha-free".to_string()]
    };
    json!({
        "enabled": util::truthy(&p["enabled"]),
        "intervalMinutes": util::clamp_num(p.get("intervalMinutes"), 5, 1440, 60),
        "minGapMinutes": util::clamp_num(p.get("minGapMinutes"), 10, 2880, 180),
        "dailyLimit": util::clamp_num(p.get("dailyLimit"), 1, 20, 3),
        "startHour": util::clamp_num(p.get("startHour"), 0, 23, 9),
        "endHour": util::clamp_num(p.get("endHour"), 0, 24, 23),
        "targetType": if util::js_string(&p["targetType"]) == "group" { "group" } else { "c2c" },
        "targetId": util::js_string(&p["targetId"]),
        "hint": util::js_string(&p["hint"]),
        "fallbackModels": fallback,
    })
}

pub fn load_state(app: &Arc<App>) -> Value {
    util::read_json_or(&app.paths.proactive_state(), json!({}))
}

fn save_state(app: &Arc<App>, st: &Value) {
    let _ = util::write_json(&app.paths.proactive_state(), st, false);
}

fn proactive_target(app: &Arc<App>, p: &Value) -> String {
    let id = util::js_string(&p["targetId"]);
    if !id.is_empty() {
        return id;
    }
    let cfg = config::read_config(&app.paths);
    let key = if util::js_string(&p["targetType"]) == "group" { "groupAllow" } else { "c2cAllow" };
    let list = util::string_array(&cfg[key]);
    list.first().cloned().unwrap_or_default()
}

fn in_window(p: &Value, hour: i64) -> bool {
    let s = p["startHour"].as_i64().unwrap_or(9);
    let e = p["endHour"].as_i64().unwrap_or(23);
    if s == e {
        return true;
    }
    if s < e {
        hour >= s && hour < e
    } else {
        hour >= s || hour < e
    }
}

fn is_skip(reply: &str) -> bool {
    let lower = reply.to_lowercase();
    if !lower.starts_with("skip") {
        return false;
    }
    match lower[4..].chars().next() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_'),
    }
}

fn trim_quotes(s: &str) -> String {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'))
        .trim()
        .to_string()
}

pub async fn tick(app: &Arc<App>, force: bool) -> Value {
    if app.proactive_busy.swap(true, Ordering::SeqCst) {
        return json!({ "ok": false, "skipped": "busy" });
    }
    let r = tick_inner(app, force).await;
    app.proactive_busy.store(false, Ordering::SeqCst);
    r
}

async fn tick_inner(app: &Arc<App>, force: bool) -> Value {
    let p = proactive_cfg(app);
    let mut st = load_state(app);
    if !st.is_object() {
        st = json!({});
    }
    let now = chrono::Local::now();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let now_ms = util::now_ms();

    if !util::truthy(&p["enabled"]) && !force {
        return json!({ "ok": true, "skipped": "disabled" });
    }
    if !bot::is_running(app) {
        return json!({ "ok": true, "skipped": "机器人未启动" });
    }
    if !force && !in_window(&p, now.hour() as i64) {
        return json!({ "ok": true, "skipped": "不在发送时段" });
    }
    if util::js_string(&st["day"]) != today {
        st["day"] = Value::String(today);
        st["sentToday"] = json!(0);
    }
    if !force {
        let last_check = util::parse_iso_ms(&util::js_string(&st["lastCheckAt"])).unwrap_or(0);
        let interval = p["intervalMinutes"].as_i64().unwrap_or(60);
        if now_ms - last_check < interval * 60_000 {
            return json!({ "ok": true, "skipped": "未到判断间隔" });
        }
        let last_sent = util::parse_iso_ms(&util::js_string(&st["lastSentAt"])).unwrap_or(0);
        let gap = p["minGapMinutes"].as_i64().unwrap_or(180);
        if now_ms - last_sent < gap * 60_000 {
            return json!({ "ok": true, "skipped": "距上次发言太近" });
        }
        if st["sentToday"].as_i64().unwrap_or(0) >= p["dailyLimit"].as_i64().unwrap_or(3) {
            return json!({ "ok": true, "skipped": "已达今日上限" });
        }
    }
    st["lastCheckAt"] = Value::String(util::iso_now());

    let target = proactive_target(app, &p);
    if target.is_empty() {
        st["lastError"] = Value::String("没有可用的发送对象（白名单为空）".into());
        return json!({ "ok": false, "skipped": "没有可用的发送对象" });
    }

    let cfg = config::read_config(&app.paths);
    let persona = util::js_or_string(
        &cfg["persona"],
        "你是「大肥鱼」，一只穿女仆装的人鱼小精灵。",
    );
    let last_sent_ms = util::parse_iso_ms(&util::js_string(&st["lastSentAt"]));
    let gap_min = last_sent_ms
        .map(|t| ((now_ms - t) as f64 / 60000.0).round() as i64)
        .unwrap_or(-1);

    let sys = [
        persona.as_str(),
        "",
        "现在不是用户在跟你聊天，而是你要主动给用户发一条消息。",
        "规则：",
        "1. 你平时就爱跟主人说话，默认倾向于说点什么（关心、吐槽、分享小日常都行）。",
        "2. 只有两种情况才输出 SKIP：刚才已经主动说过、或者这个时间点明显不合适。",
        "3. 要发就只写消息本身，1-2 句，口语化，像真人朋友。",
        "4. 不要解释你为什么发，不要加引号，不要用「你好」「在吗」这种空话开头。",
    ]
    .join("\n");

    let mut usr_lines = vec![format!(
        "现在时间：{}",
        now.format("%Y/%m/%d %H:%M:%S")
    )];
    if gap_min < 0 {
        usr_lines.push("你还没主动找过用户说话。".into());
    } else {
        usr_lines.push(format!("距上次主动说话：{gap_min} 分钟前。"));
    }
    usr_lines.push(format!(
        "今天已经主动说了 {} 次（上限 {}）。",
        st["sentToday"].as_i64().unwrap_or(0),
        p["dailyLimit"].as_i64().unwrap_or(3)
    ));
    let hint = util::js_string(&p["hint"]);
    if !hint.is_empty() {
        usr_lines.push(format!("额外倾向：{hint}"));
    }
    usr_lines.push("请判断并输出。".into());
    let usr = usr_lines.join("\n");

    let msgs = json!([
        { "role": "system", "content": sys },
        { "role": "user", "content": usr },
    ]);

    let use_fallback = util::js_string(&cfg["llmMode"]) != "ollama";
    let mut candidates: Vec<String> = vec![String::new()];
    if use_fallback {
        candidates.extend(util::string_array(&p["fallbackModels"]));
    }

    let mut reply = String::new();
    let mut last_err = String::new();
    let mut used_model = String::new();
    for m in &candidates {
        let model = if m.is_empty() { None } else { Some(m.as_str()) };
        match remote::chat_complete(app, &msgs, model, None, None, None).await {
            Ok(r) => {
                if r.trim().is_empty() {
                    last_err = format!(
                        "模型 {} 返回空内容",
                        if m.is_empty() { "主模型" } else { m.as_str() }
                    );
                    continue;
                }
                reply = r;
                last_err.clear();
                used_model = if m.is_empty() { "主模型".into() } else { m.clone() };
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    if !last_err.is_empty() {
        st["lastError"] = Value::String(last_err.clone());
        save_state(app, &st);
        bot::push_line(app, &format!("[主动] 判断失败: {last_err}"));
        return json!({ "ok": false, "error": last_err });
    }
    st["lastError"] = Value::String(String::new());
    st["lastModel"] = Value::String(used_model);

    if reply.trim().is_empty() || is_skip(&reply) {
        save_state(app, &st);
        bot::push_line(app, "[主动] 这次判断为：不说话");
        return json!({
            "ok": true,
            "skipped": "模型判断这次不说话",
            "raw": reply.chars().take(120).collect::<String>(),
        });
    }
    let text = trim_quotes(&reply);
    let target_type = util::js_string(&p["targetType"]);
    if let Err(e) = remote::send_qq_message(app, &target_type, &target, &text).await {
        st["lastError"] = Value::String(e.to_string());
        save_state(app, &st);
        bot::push_line(app, &format!("[主动] 发送失败: {e}"));
        return json!({ "ok": false, "error": e.to_string() });
    }
    st["lastSentAt"] = Value::String(util::iso_now());
    st["sentToday"] = json!(st["sentToday"].as_i64().unwrap_or(0) + 1);
    st["lastText"] = Value::String(text.clone());
    save_state(app, &st);
    bot::push_line(
        app,
        &format!(
            "[主动] 已发送({}): {}",
            target_type,
            text.chars().take(60).collect::<String>()
        ),
    );
    json!({
        "ok": true,
        "sent": true,
        "text": text,
        "targetType": target_type,
        "targetId": target,
    })
}

pub fn start(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let a = app.clone();
            let r = tick(&a, false).await;
            if let Some(err) = r.get("error").and_then(|v| v.as_str()) {
                bot::push_line(&a, &format!("[主动] 异常: {err}"));
            }
        }
    });
}
