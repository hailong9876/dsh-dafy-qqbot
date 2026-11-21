//! QQ 机器人进程管理：启动/停止、扫码二维码、日志扫描、记忆同步、额度换模型。

use crate::util::{self};
use crate::{config, App};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Default)]
pub struct BotState {
    pub pid: Option<u32>,
    pub running: bool,
    pub bind_url: String,
    pub qr: String,
    pub out_buf: String,
    pub lines: Vec<String>,
    pub bind_handled: bool,
    pub after_bind_pending: bool,
    pub quota_hits: u32,
    pub last_model_switch: i64,
}

#[derive(Default)]
pub struct MemWatch {
    pub t: i64,
    pub last_restart: i64,
    pub pending: bool,
}

pub fn push_line(app: &Arc<App>, s: &str) {
    let t = s.trim();
    if t.is_empty() {
        return;
    }
    let mut st = app.st.lock().unwrap();
    st.lines.push(t.to_string());
    let len = st.lines.len();
    if len > 300 {
        st.lines.drain(0..len - 300);
    }
}

pub fn is_running(app: &Arc<App>) -> bool {
    app.st.lock().unwrap().running
}

fn resolve_node(root: &std::path::Path) -> PathBuf {
    if let Ok(v) = std::env::var("DAFY_NODE") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return p;
        }
    }
    let bundled = root.join("node").join("node.exe");
    if bundled.is_file() {
        return bundled;
    }
    PathBuf::from("node")
}

fn hidden(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

fn read_all(mut r: Option<impl Read>) -> String {
    let mut buf = Vec::new();
    if let Some(r) = r.as_mut() {
        let _ = r.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn run_capture(mut cmd: Command, timeout: Duration) -> Option<(String, String)> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hidden(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let h1 = std::thread::spawn(move || read_all(out));
    let h2 = std::thread::spawn(move || read_all(err));
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    Some((h1.join().unwrap_or_default(), h2.join().unwrap_or_default()))
}

/// 生成扫码二维码（PNG data URL），等价 qrcode.toDataURL(..., {width:320,ecLevel:M})。
fn qr_data_url(data: &str) -> Option<String> {
    let code =
        qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::M).ok()?;
    let img = code
        .render::<image::Luma<u8>>()
        .min_dimensions(320, 320)
        .quiet_zone(true)
        .build();
    let mut buf = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut buf);
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .ok()?;
    }
    Some(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(&buf)
    ))
}

fn outbound_transform(app: &Arc<App>) {
    let node = resolve_node(&app.paths.root);
    let script = app
        .paths
        .qb
        .join("patch")
        .join("outbound-transform.patch.js");
    let profile = app.paths.qb.join(".dsh").join("profiles").join("qqbot");
    let mut cmd = Command::new(node);
    cmd.arg(script).arg(profile).current_dir(&app.paths.qb);
    match run_capture(cmd, Duration::from_secs(15)) {
        Some((out, err)) => {
            let mut msg = out;
            if !err.trim().is_empty() {
                msg.push(' ');
                msg.push_str(&err);
            }
            let msg = msg.trim();
            if !msg.is_empty() {
                if let Some(last) = msg.lines().filter(|l| !l.trim().is_empty()).last() {
                    push_line(app, &format!("[patch] {}", last.trim()));
                }
            }
        }
        None => push_line(app, "[patch] outbound-transform skipped: 启动失败"),
    }
}

pub fn start_bot(app: &Arc<App>) -> Value {
    {
        let mut st = app.st.lock().unwrap();
        if st.pid.is_some() {
            return json!({ "ok": true, "already": true });
        }
        st.bind_url.clear();
        st.qr.clear();
        st.out_buf.clear();
        st.lines.clear();
        st.bind_handled = false;
        st.after_bind_pending = false;
    }
    let _ = std::fs::write(&app.paths.log, b"");

    let cfg = config::read_config(&app.paths);
    let prep = match config::prepare_patches(&app.paths, &cfg) {
        Ok(p) => p,
        Err(e) => {
            push_line(app, &format!("[bridge] prepare failed: {e}"));
            return json!({ "ok": false, "error": e.to_string() });
        }
    };

    push_line(
        app,
        &format!("[bridge] memory={} dir={}", prep.mem_count, prep.mem_dir),
    );
    app.last_mem_count.store(prep.mem_count, std::sync::atomic::Ordering::SeqCst);

    outbound_transform(app);

    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(app.paths.script())
        .current_dir(&app.paths.qb)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &prep.envs {
        cmd.env(k, v);
    }
    hidden(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            push_line(app, &format!("[bridge] bot 启动失败: {e}"));
            return json!({ "ok": false, "error": e.to_string() });
        }
    };
    let pid = child.id();

    if let Some(out) = child.stdout.take() {
        let a = app.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut r = std::io::BufReader::new(out);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => handle_output(&a, &String::from_utf8_lossy(&buf)),
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let a = app.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut r = std::io::BufReader::new(err);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => handle_output(&a, &String::from_utf8_lossy(&buf)),
                }
            }
        });
    }

    {
        let a = app.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let mut st = a.st.lock().unwrap();
            if st.pid == Some(pid) {
                st.pid = None;
                st.running = false;
            }
            drop(st);
            push_line(&a, &format!("[bridge] bot exited ({code})"));
        });
    }

    {
        let mut st = app.st.lock().unwrap();
        st.pid = Some(pid);
        st.running = true;
    }
    json!({ "ok": true })
}

fn handle_output(app: &Arc<App>, chunk: &str) {
    for line in chunk.lines() {
        push_line(app, line);
    }
    if let Some(url) = util::last_bind_url(chunk) {
        set_bind_url(app, &url);
    }
    if chunk.contains("Bot ready!") {
        let mut st = app.st.lock().unwrap();
        st.bind_url.clear();
        st.qr.clear();
    }
    if chunk.contains("扫码绑定成功") {
        schedule_after_bind(app);
    }
    let low = chunk.to_lowercase();
    if low.contains("free usage limit reached")
        || chunk.contains("QUOTA:")
        || low.contains("quota_exceeded")
        || low.contains("free models work in opencode")
    {
        note_quota(app);
    }
}

fn set_bind_url(app: &Arc<App>, url: &str) {
    {
        let st = app.st.lock().unwrap();
        if st.bind_url == url {
            return;
        }
    }
    if let Some(qr) = qr_data_url(url) {
        let mut st = app.st.lock().unwrap();
        st.bind_url = url.to_string();
        st.qr = qr;
    } else {
        app.st.lock().unwrap().bind_url = url.to_string();
    }
}

fn note_quota(app: &Arc<App>) {
    let hits = {
        let mut st = app.st.lock().unwrap();
        st.quota_hits += 1;
        st.quota_hits
    };
    if hits < 2 {
        return;
    }
    let last = app.st.lock().unwrap().last_model_switch;
    if util::now_ms() - last < 10 * 60_000 {
        return;
    }
    let mut cfg = config::read_config(&app.paths);
    let list = util::string_array(&cfg["modelFallbacks"]);
    if list.is_empty() {
        return;
    }
    let cur = util::js_string(&cfg["deepseekModel"]);
    let idx = list.iter().position(|m| *m == cur).map(|i| i as i64).unwrap_or(-1);
    let len = list.len() as i64;
    let next = list[(((idx + 1 + len) % len) as usize).min(list.len() - 1)].clone();
    if next.is_empty() || next == cur {
        return;
    }
    {
        let mut st = app.st.lock().unwrap();
        st.quota_hits = 0;
        st.last_model_switch = util::now_ms();
    }
    cfg["deepseekModel"] = Value::String(next.clone());
    config::write_config(&app.paths, &cfg);
    config::write_llm_patch(&app.paths, &cfg);
    push_line(app, &format!("[模型] 免费额度耗尽，自动切换到 {next}"));
    if is_running(app) {
        stop_bot(app);
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            start_bot(&a);
        });
    }
}

fn schedule_after_bind(app: &Arc<App>) {
    {
        let mut st = app.st.lock().unwrap();
        st.bind_url.clear();
        st.qr.clear();
        if st.bind_handled || config::bound_app_id(&app.paths).is_empty() {
            return;
        }
        st.bind_handled = true;
        if st.after_bind_pending {
            return;
        }
        st.after_bind_pending = true;
    }
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        a.st.lock().unwrap().after_bind_pending = false;
        stop_bot(&a);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        start_bot(&a);
    });
}

pub fn stop_bot(app: &Arc<App>) -> Value {
    let pid = {
        let mut st = app.st.lock().unwrap();
        let p = st.pid.take();
        st.running = false;
        p
    };
    if let Some(pid) = pid {
        dafy_core::process::kill_tree(pid);
    }
    json!({ "ok": true })
}

pub fn scan_log(app: &Arc<App>) {
    let Ok(bytes) = std::fs::read(&app.paths.log) else {
        return;
    };
    let text = util::decode_log_bytes(&bytes);
    if let Some(url) = util::last_bind_url(&text) {
        set_bind_url(app, &url);
    }
    if text.contains("Bot ready!") {
        let mut st = app.st.lock().unwrap();
        st.bind_url.clear();
        st.qr.clear();
    }
    if text.contains("扫码绑定成功") {
        schedule_after_bind(app);
    }
}

pub fn start_log_scan(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if is_running(&app) {
                scan_log(&app);
            }
        }
    });
}

fn bot_log_idle_ms(app: &Arc<App>) -> i64 {
    match util::mtime_ms(&app.paths.log) {
        Some(t) => util::now_ms() - t,
        None => i64::MAX,
    }
}

pub fn start_memory_watch(app: Arc<App>) {
    tokio::spawn(async move {
        let mut last = config::memory_latest_mtime(&app.paths);
        app.mem.lock().unwrap().t = last;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let cur = config::memory_latest_mtime(&app.paths);
            let changed = last > 0 && cur > last;
            last = cur;
            app.mem.lock().unwrap().t = cur;
            if !changed {
                continue;
            }
            let (active, no_bind) = {
                let st = app.st.lock().unwrap();
                (st.pid.is_some() && st.running, st.bind_url.is_empty())
            };
            let pending = {
                let mut m = app.mem.lock().unwrap();
                if !active || !no_bind || m.pending {
                    continue;
                }
                m.pending = true;
                true
            };
            if pending {
                let a = app.clone();
                tokio::spawn(async move { memory_sync(a).await });
            }
        }
    });
}

async fn memory_sync(app: Arc<App>) {
    let mut delay = 15u64;
    loop {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        let (has_bot, has_bind) = {
            let st = app.st.lock().unwrap();
            (st.pid.is_some(), !st.bind_url.is_empty())
        };
        if !has_bot || has_bind {
            app.mem.lock().unwrap().pending = false;
            return;
        }
        if util::now_ms() - app.mem.lock().unwrap().last_restart < 600_000 {
            app.mem.lock().unwrap().pending = false;
            return;
        }
        if bot_log_idle_ms(&app) < 180_000 {
            delay = 60;
            continue;
        }
        app.mem.lock().unwrap().last_restart = util::now_ms();
        stop_bot(&app);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        start_bot(&app);
        push_line(&app, "[memory] 检测到新记忆，已自动同步并重启机器人");
        app.mem.lock().unwrap().pending = false;
        return;
    }
}
