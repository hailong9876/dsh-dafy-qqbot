//! TTS 双服务：tts_server(18903) / edge_tts_server(18904)、缓存查看、请求反向代理。

use crate::util;
use crate::{bot, App};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub const TTS_PORT: u16 = 18903;
pub const EDGE_TTS_PORT: u16 = 18904;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn hidden(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

pub fn resolve_python(app: &Arc<App>) -> PathBuf {
    if let Ok(env) = std::env::var("DAFY_PYTHON") {
        let p = PathBuf::from(&env);
        if p.is_file() {
            return p;
        }
    }
    let c1 = app.paths.root.join("python").join("python.exe");
    if c1.is_file() {
        return c1;
    }
    let c2 = app
        .paths
        .root
        .parent()
        .map(|p| p.join("agent-lightning").join(".venv").join("Scripts").join("python.exe"))
        .unwrap_or_else(|| PathBuf::from("python"));
    if c2.is_file() {
        return c2;
    }
    PathBuf::from("python")
}

pub fn tts_installed(app: &Arc<App>) -> bool {
    app.paths.tts.join("tts_server.py").is_file()
}

pub fn edge_installed(app: &Arc<App>) -> bool {
    app.paths.tts.join("edge_tts_server.py").is_file()
}

async fn probe(_app: &Arc<App>, port: u16, path: &str, timeout_ms: u64) -> bool {
    dafy_core::net::http_get(port, path, Duration::from_millis(timeout_ms))
        .await
        .map(|r| r.body.contains("\"ok\":true"))
        .unwrap_or(false)
}

pub async fn tts_status(app: &Arc<App>) -> Value {
    let running = probe(app, TTS_PORT, "/health", 2500).await;
    let pid = *app.tts_pid.lock().unwrap();
    json!({
        "installed": tts_installed(app),
        "running": running,
        "port": TTS_PORT,
        "pid": pid,
    })
}

pub async fn edge_status(app: &Arc<App>) -> Value {
    let running = probe(app, EDGE_TTS_PORT, "/health", 2500).await;
    let pid = *app.edge_pid.lock().unwrap();
    json!({
        "installed": edge_installed(app),
        "running": running,
        "port": EDGE_TTS_PORT,
        "pid": pid,
    })
}

fn spawn_with_log(
    app: &Arc<App>,
    script: &str,
    args: &[&str],
    log_name: &str,
) -> anyhow::Result<u32> {
    let logs = app.paths.root.join("logs");
    fs::create_dir_all(&logs)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join(log_name))?;
    let err = log.try_clone()?;
    let mut cmd = Command::new(resolve_python(app));
    cmd.arg(script)
        .args(args)
        .current_dir(&app.paths.tts)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .env("CUDA_VISIBLE_DEVICES", "")
        .env("HF_ENDPOINT", "https://hf-mirror.com");
    hidden(&mut cmd);
    let child = cmd.spawn()?;
    Ok(child.id())
}

pub async fn tts_start(app: &Arc<App>) -> Value {
    if !tts_installed(app) {
        return json!({ "ok": false, "error": "没找到 tts/tts_server.py" });
    }
    if probe(app, TTS_PORT, "/health", 800).await {
        return json!({ "ok": true, "already": true });
    }
    let pid = match spawn_with_log(
        app,
        "tts_server.py",
        &["--port", &TTS_PORT.to_string(), "--preload"],
        "tts.log",
    ) {
        Ok(p) => p,
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    *app.tts_pid.lock().unwrap() = Some(pid);
    bot::push_line(app, &format!("[tts] 启动（pid {pid}），首次加载模型约 55s"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    for _ in 0..60 {
        if probe(app, TTS_PORT, "/health", 500).await {
            bot::push_line(app, "[tts] 已就绪");
            return json!({ "ok": true, "pid": pid });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    json!({ "ok": false, "error": "TTS 启动超时（60s），请看 logs/tts.log" })
}

pub async fn edge_start(app: &Arc<App>) -> Value {
    if !edge_installed(app) {
        return json!({ "ok": false, "error": "没找到 tts/edge_tts_server.py" });
    }
    if probe(app, EDGE_TTS_PORT, "/health", 800).await {
        return json!({ "ok": true, "already": true });
    }
    let pid = match spawn_with_log(
        app,
        "edge_tts_server.py",
        &["--port", &EDGE_TTS_PORT.to_string()],
        "edge-tts.log",
    ) {
        Ok(p) => p,
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    *app.edge_pid.lock().unwrap() = Some(pid);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for _ in 0..20 {
        if probe(app, EDGE_TTS_PORT, "/health", 800).await {
            return json!({ "ok": true, "pid": pid });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    json!({ "ok": false, "error": "Edge TTS 启动超时，请看 logs/edge-tts.log" })
}

async fn pids_on_port(port: u16) -> Vec<u32> {
    let out = tokio::process::Command::new("netstat").arg("-ano").output().await;
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = format!(":{port}");
    let mut pids = Vec::new();
    for line in text.lines() {
        if !line.contains(&needle) || !line.to_uppercase().contains("LISTENING") {
            continue;
        }
        if let Some(pid) = line.split_whitespace().last().and_then(|s| s.parse::<u32>().ok()) {
            pids.push(pid);
        }
    }
    pids
}

pub async fn tts_stop(app: &Arc<App>) -> Value {
    for pid in pids_on_port(TTS_PORT).await {
        dafy_core::process::kill_tree(pid);
    }
    *app.tts_pid.lock().unwrap() = None;
    json!({ "ok": true })
}

pub async fn edge_stop(app: &Arc<App>) -> Value {
    for pid in pids_on_port(EDGE_TTS_PORT).await {
        dafy_core::process::kill_tree(pid);
    }
    *app.edge_pid.lock().unwrap() = None;
    json!({ "ok": true })
}

fn cache_dirs(app: &Arc<App>) -> [(&'static str, PathBuf); 2] {
    [
        ("edge", app.paths.tts.join("cache_edge")),
        ("chattts", app.paths.tts.join("cache")),
    ]
}

fn is_audio(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".mp3") || lower.ends_with(".wav")
}

pub fn cache_info(app: &Arc<App>) -> Value {
    let mut total_count = 0i64;
    let mut total_bytes = 0i64;
    let mut per_dir: Vec<(String, Value)> = Vec::new();
    for (key, dir) in cache_dirs(app) {
        let mut files = Vec::new();
        let mut bytes = 0i64;
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !is_audio(&name) {
                    continue;
                }
                if let Ok(st) = e.metadata() {
                    bytes += st.len() as i64;
                    files.push(json!({
                        "name": name,
                        "size": st.len(),
                        "mtime": st.modified().ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0),
                    }));
                }
            }
        }
        files.sort_by(|a, b| {
            b["mtime"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["mtime"].as_i64().unwrap_or(0))
        });
        let count = files.len() as i64;
        total_count += count;
        total_bytes += bytes;
        files.truncate(300);
        per_dir.push((
            key.to_string(),
            json!({
                "dir": dir.display().to_string(),
                "count": count,
                "bytes": bytes,
                "files": files,
            }),
        ));
    }
    let mut out = serde_json::Map::new();
    out.insert("ok".into(), Value::Bool(true));
    out.insert("total".into(), json!({ "count": total_count, "bytes": total_bytes }));
    for (k, v) in per_dir {
        out.insert(k, v);
    }
    Value::Object(out)
}

pub fn cache_clear(app: &Arc<App>, body: &Value) -> Value {
    let engine = util::js_or_string(&body["engine"], "all");
    let file = util::js_string(&body["file"]);
    let keys: Vec<String> = if engine == "all" {
        cache_dirs(app).iter().map(|(k, _)| k.to_string()).collect()
    } else {
        vec![engine]
    };
    let mut removed = 0i64;
    let mut freed = 0i64;
    let mut errors = 0i64;
    for k in keys {
        let Some(dir) = cache_dirs(app)
            .into_iter()
            .find(|(name, _)| *name == k)
            .map(|(_, d)| d)
        else {
            errors += 1;
            continue;
        };
        if !file.is_empty() {
            // 单个删除：只允许纯文件名，防止路径穿越。
            let safe = Path::new(&file)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if safe != file || !is_audio(&safe) {
                errors += 1;
                continue;
            }
            let fp = dir.join(&safe);
            match fs::metadata(&fp) {
                Ok(st) => match fs::remove_file(&fp) {
                    Ok(()) => {
                        freed += st.len() as i64;
                        removed += 1;
                    }
                    Err(_) => errors += 1,
                },
                Err(_) => errors += 1,
            }
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_audio(&name) {
                continue;
            }
            let fp = e.path();
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            match fs::remove_file(&fp) {
                Ok(()) => {
                    freed += size as i64;
                    removed += 1;
                }
                Err(_) => errors += 1,
            }
        }
    }
    bot::push_line(
        app,
        &format!(
            "[tts-cache] 清理语音缓存 {removed} 个，释放 {:.1} MB",
            freed as f64 / 1048576.0
        ),
    );
    json!({ "ok": errors == 0, "removed": removed, "freed": freed, "errors": errors })
}

pub async fn proxy(
    app: &Arc<App>,
    port: u16,
    sub_path_and_query: &str,
    method: Method,
    headers: &HeaderMap,
    body: Vec<u8>,
    tag: &str,
) -> Response<Body> {
    let url = format!("http://127.0.0.1:{port}{sub_path_and_query}");
    let mut rb = app
        .http
        .request(method, &url)
        .timeout(Duration::from_secs(120))
        .body(body);
    if let Some(ct) = headers.get("content-type") {
        rb = rb.header("content-type", ct.as_bytes());
    }
    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json; charset=utf-8")
                .to_string();
            match resp.bytes().await {
                Ok(bytes) => Response::builder()
                    .status(status)
                    .header("content-type", ct)
                    .header("access-control-allow-origin", "*")
                    .header("cache-control", "no-store")
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| empty_500()),
                Err(e) => error_json(502, &format!("{tag} 读取响应失败: {e}")),
            }
        }
        Err(e) => error_json(502, &format!("{tag} 服务未就绪: {e}")),
    }
}

fn empty_500() -> Response<Body> {
    Response::builder()
        .status(500)
        .body(Body::from("{}"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn error_json(code: u16, msg: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .body(Body::from(json!({ "error": msg }).to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
