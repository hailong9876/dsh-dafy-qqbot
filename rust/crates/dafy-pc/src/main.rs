//! 大肥鱼 · 电脑控制服务（Rust 版，端口 18907）
//!
//! 与旧 `pc-server/server.js` 的 HTTP 契约完全一致：
//!   GET  /api/ping              探活
//!   POST /api/screen/look       窗口列表 + OCR 文字及坐标
//!   POST /api/screen/capture    截图，默认返回 dataUrl
//!   POST /api/screen/ocr        仅 OCR
//!   POST /api/windows           窗口列表
//!   POST /api/window/focus      激活窗口 {hwnd}
//!   POST /api/mouse             鼠标 {action,x,y,delta}
//!   POST /api/type              输入文字 {text}
//!   POST /api/apps/list         已安装程序
//!   POST /api/apps/open         启动程序 {name}
//!   POST /api/open              打开目标 {target}
//!   POST /api/key               按键 / 快捷键 {keys}
//!
//! 底层系统调用仍复用仓库根目录的 `native.ps1`（截屏/OCR/键鼠/窗口），
//! 这部分是 Windows UI Automation 封装，保持原样可确保行为零差异。

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Method, Request};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 截图最多保留的张数，超出的从旧到新删掉。
const SHOT_KEEP: usize = 6;

struct App {
    port: u16,
    ps1: PathBuf,
    shot_dir: PathBuf,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let port: u16 = std::env::var("DAFY_PC_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18907);

    let root = dafy_core::project_root();
    let ps1 = root.join("native.ps1");
    if !ps1.is_file() {
        println!("[pc] 警告：找不到底层脚本 {}", ps1.display());
    }
    let app = Arc::new(App {
        port,
        ps1,
        shot_dir: std::env::temp_dir().join("dafy-pc"),
    });

    let router = Router::new().fallback(any(handle)).with_state(app);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            println!("[pc] 端口 {port} 监听失败: {e}");
            std::process::exit(1);
        }
    };
    println!("[pc] 电脑控制服务已启动: http://127.0.0.1:{port}");
    if let Err(e) = axum::serve(listener, router).await {
        println!("[pc] 服务异常退出: {e}");
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    let path = req.uri().path().to_string();
    let is_post = req.method() == Method::POST;

    if path == "/api/ping" {
        return json_response(
            200,
            json!({ "ok": true, "service": "dafy-pc", "port": app.port, "pid": std::process::id() }),
        );
    }

    let body = if is_post { read_json(req).await } else { json!({}) };

    match path.as_str() {
        "/api/screen/look" => json_response(200, look(&app, &body).await),
        "/api/screen/capture" => {
            let shot = capture(&app).await;
            if shot.get("ok").and_then(Value::as_bool) != Some(true) {
                return json_response(200, shot);
            }
            let mut base = json!({
                "ok": true,
                "width": get(&shot, "width"),
                "height": get(&shot, "height"),
                "left": get(&shot, "left"),
                "top": get(&shot, "top"),
                "bytes": get(&shot, "bytes"),
                "shot": get(&shot, "path"),
            });
            if body.get("dataUrl") == Some(&Value::Bool(false)) {
                return json_response(200, base);
            }
            let file = shot.get("path").and_then(Value::as_str).unwrap_or("");
            match tokio::fs::read(file).await {
                Ok(buf) => {
                    if let Some(obj) = base.as_object_mut() {
                        obj.insert(
                            "image".into(),
                            Value::String(format!(
                                "data:image/png;base64,{}",
                                STANDARD.encode(buf)
                            )),
                        );
                    }
                    json_response(200, base)
                }
                Err(e) => json_response(
                    200,
                    json!({ "ok": false, "reason": format!("读取截图失败: {e}") }),
                ),
            }
        }
        "/api/screen/ocr" => {
            let shot = capture(&app).await;
            if shot.get("ok").and_then(Value::as_bool) != Some(true) {
                return json_response(200, shot);
            }
            let r = run_native(
                &app,
                "ocr",
                json!({ "path": get(&shot, "path") }),
                30_000,
            )
            .await;
            let mut map = r.as_object().cloned().unwrap_or_default();
            map.insert("width".into(), get(&shot, "width"));
            map.insert("height".into(), get(&shot, "height"));
            map.insert("shot".into(), get(&shot, "path"));
            json_response(200, Value::Object(map))
        }
        "/api/windows" => json_response(200, run_native(&app, "windows", json!({}), 20_000).await),
        "/api/window/focus" => {
            let hwnd = body.get("hwnd").cloned().unwrap_or(Value::Null);
            if hwnd.is_null() {
                return json_response(200, json!({ "ok": false, "reason": "缺少 hwnd" }));
            }
            json_response(
                200,
                run_native(&app, "focus", json!({ "hwnd": hwnd }), 15_000).await,
            )
        }
        "/api/mouse" => {
            let act = body
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("click")
                .to_ascii_lowercase();
            if !["move", "click", "double", "right", "wheel"].contains(&act.as_str()) {
                return json_response(
                    200,
                    json!({ "ok": false, "reason": "action 只能是 move/click/double/right/wheel" }),
                );
            }
            let x = to_number(body.get("x"));
            let y = to_number(body.get("y"));
            if act != "wheel" && (x.is_none() || y.is_none()) {
                return json_response(200, json!({ "ok": false, "reason": "缺少坐标 x/y" }));
            }
            let delta = to_number(body.get("delta")).map(|v| v.round() as i64).unwrap_or(-120);
            json_response(
                200,
                run_native(
                    &app,
                    "mouse",
                    json!({
                        "action": act,
                        "x": x.map(|v| v.round() as i64).unwrap_or(0),
                        "y": y.map(|v| v.round() as i64).unwrap_or(0),
                        "delta": delta,
                    }),
                    15_000,
                )
                .await,
            )
        }
        "/api/type" => {
            let text = string_of(body.get("text"));
            if text.is_empty() {
                return json_response(200, json!({ "ok": false, "reason": "缺少要输入的文字" }));
            }
            if text.encode_utf16().count() > 2000 {
                return json_response(
                    200,
                    json!({ "ok": false, "reason": "单次输入不要超过 2000 字" }),
                );
            }
            json_response(200, run_native(&app, "type", json!({ "text": text }), 30_000).await)
        }
        "/api/apps/list" => {
            json_response(200, run_native(&app, "apps", json!({}), 20_000).await)
        }
        "/api/apps/open" => {
            let name = string_of(body.get("name")).trim().to_string();
            if name.is_empty() {
                return json_response(200, json!({ "ok": false, "reason": "缺少程序名" }));
            }
            json_response(200, run_native(&app, "launch", json!({ "name": name }), 30_000).await)
        }
        "/api/open" => {
            let target = string_of(body.get("target")).trim().to_string();
            if target.is_empty() {
                return json_response(200, json!({ "ok": false, "reason": "缺少要打开的目标" }));
            }
            json_response(200, run_native(&app, "open", json!({ "target": target }), 35_000).await)
        }
        "/api/key" => {
            let keys = string_of(body.get("keys")).trim().to_string();
            if keys.is_empty() {
                return json_response(200, json!({ "ok": false, "reason": "缺少 keys，例如 ctrl+c" }));
            }
            json_response(200, run_native(&app, "key", json!({ "keys": keys }), 15_000).await)
        }
        _ => json_response(404, json!({ "ok": false, "reason": format!("未知接口: {path}") })),
    }
}

async fn read_json(req: Request<Body>) -> Value {
    match to_bytes(req.into_body(), 1_000_000).await {
        Ok(b) if !b.is_empty() => serde_json::from_slice(&b).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

fn json_response(code: u16, body: Value) -> Response {
    Response::builder()
        .status(code)
        .header("content-type", "application/json; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("{}")))
}

/* ---------------- JSON 取值助手 ---------------- */

fn get(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

/// 模拟 JS 的 Number()：数字直接取，字符串去空白后解析，布尔转 0/1。
fn to_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// 模拟 JS 的 String(v)：null/undefined 视为空串。
fn string_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn num_of(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/* ---------------- 截图 / OCR ---------------- */

async fn capture(app: &App) -> Value {
    let _ = tokio::fs::create_dir_all(&app.shot_dir).await;
    let file = app
        .shot_dir
        .join(format!("shot-{}.png", now_millis()));
    let mut r = run_native(
        app,
        "capture",
        json!({ "path": file.to_string_lossy() }),
        20_000,
    )
    .await;
    if r.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Ok(md) = tokio::fs::metadata(&file).await {
            if let Some(obj) = r.as_object_mut() {
                obj.insert("bytes".into(), json!(md.len()));
            }
        }
        prune_shots(app).await;
    }
    r
}

async fn prune_shots(app: &App) {
    let Ok(mut dir) = tokio::fs::read_dir(&app.shot_dir).await else {
        return;
    };
    let mut names = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".png") {
            names.push(name);
        }
    }
    names.sort();
    if names.len() > SHOT_KEEP {
        for name in &names[..names.len() - SHOT_KEEP] {
            let _ = tokio::fs::remove_file(app.shot_dir.join(name)).await;
        }
    }
}

async fn look(app: &App, body: &Value) -> Value {
    let shot = capture(app).await;
    if shot.get("ok").and_then(Value::as_bool) != Some(true) {
        return shot;
    }
    let ocr = run_native(app, "ocr", json!({ "path": get(&shot, "path") }), 30_000).await;
    let wins = run_native(app, "windows", json!({}), 20_000).await;

    let mut words: Vec<Value> = ocr
        .get("words")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 与旧脚本一致：同一行（y 差 ≤6）内按 x 排，不同行按 y 排。
    // 注意：不能直接照搬旧比较器——它不满足严格全序，Rust 排序会 panic。
    // 这里先按 y 排序分带，再在每条带内按 x 排序，结果与旧行为一致且稳定。
    words.sort_by(|a, b| {
        num_of(a, "y")
            .partial_cmp(&num_of(b, "y"))
            .unwrap_or(Ordering::Equal)
    });
    let mut bands: Vec<Vec<Value>> = Vec::new();
    for w in words {
        let y = num_of(&w, "y");
        if let Some(band) = bands.last_mut() {
            if let Some(first) = band.first() {
                if (y - num_of(first, "y")).abs() <= 6.0 {
                    band.push(w);
                    continue;
                }
            }
        }
        bands.push(vec![w]);
    }
    let mut words: Vec<Value> = Vec::with_capacity(bands.iter().map(Vec::len).sum());
    for mut band in bands {
        band.sort_by(|a, b| {
            num_of(a, "x")
                .partial_cmp(&num_of(b, "x"))
                .unwrap_or(Ordering::Equal)
        });
        words.extend(band);
    }

    let raw_max = to_number(body.get("maxWords")).unwrap_or(0.0);
    let max_words = (if raw_max == 0.0 { 80.0 } else { raw_max }).clamp(10.0, 300.0) as usize;
    if words.len() > max_words {
        words.truncate(max_words);
    }

    struct Line {
        t: String,
        x: f64,
        y: f64,
        h: f64,
        x2: f64,
    }
    let mut lines: Vec<Line> = Vec::new();
    for w in &words {
        let t = w.get("t").and_then(Value::as_str).unwrap_or("").to_string();
        let (x, y, ww, h) = (
            num_of(w, "x"),
            num_of(w, "y"),
            num_of(w, "w"),
            num_of(w, "h"),
        );
        if let Some(last) = lines.last_mut() {
            if (last.y - y).abs() <= 6.0 && (last.h - h).abs() <= 8.0 {
                last.t.push_str(&t);
                last.x2 = last.x2.max(x + ww);
                continue;
            }
        }
        lines.push(Line {
            t,
            x,
            y,
            h,
            x2: x + ww,
        });
    }

    let wins_ok = wins.get("ok").and_then(Value::as_bool) == Some(true);
    let windows: Vec<Value> = if wins_ok {
        wins.get("windows")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .take(25)
                    .map(|win| {
                        json!({
                            "hwnd": get(win, "hwnd"),
                            "title": truncate_chars(&string_of(win.get("title")), 80),
                            "proc": get(win, "proc"),
                            "x": get(win, "x"),
                            "y": get(win, "y"),
                            "w": get(win, "w"),
                            "h": get(win, "h"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    json!({
        "ok": true,
        "screen": { "width": get(&shot, "width"), "height": get(&shot, "height") },
        "shot": get(&shot, "path"),
        "ocrOk": ocr.get("ok").and_then(Value::as_bool) == Some(true),
        "ocrReason": if ocr.get("ok").and_then(Value::as_bool) == Some(true) { String::new() } else { string_of(ocr.get("reason")) },
        "text": truncate_chars(&string_of(ocr.get("text")), 4000),
        "lines": lines.iter().map(|l| json!({
            "t": truncate_chars(&l.t, 80),
            "x": l.x,
            "y": l.y,
            "w": l.x2 - l.x,
            "h": l.h,
        })).collect::<Vec<_>>(),
        "foreground": if wins_ok { get(&wins, "foreground") } else { json!(0) },
        "windows": windows,
        "windowsReason": if wins_ok { String::new() } else { string_of(wins.get("reason")) },
    })
}

/* ---------------- PowerShell 调用 ---------------- */

/// 调用 native.ps1，把 JSON 结果带回来；超时杀掉进程。
async fn run_native(app: &App, action: &str, payload: Value, timeout_ms: u64) -> Value {
    let arg = STANDARD.encode(payload.to_string().as_bytes());
    let mut cmd = Command::new("powershell.exe");
    cmd.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&app.ps1)
        .arg("-Action")
        .arg(action)
        .arg("-Arg")
        .arg(&arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        cmd.creation_flags(dafy_core::process::CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json!({ "ok": false, "reason": format!("无法启动 PowerShell: {e}") })
        }
    };

    let out_task = spawn_read(child.stdout.take());
    let err_task = spawn_read(child.stderr.take());

    let timed_out = match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await
    {
        Ok(_) => false,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            true
        }
    };

    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();

    if timed_out {
        return json!({ "ok": false, "reason": format!("操作超时（{timeout_ms}ms）: {action}") });
    }

    let stdout = String::from_utf8_lossy(&out);
    let line = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or("");
    match serde_json::from_str::<Value>(line) {
        Ok(v) => v,
        Err(_) => {
            let stderr = String::from_utf8_lossy(&err);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() { line } else { stderr };
            json!({
                "ok": false,
                "reason": format!("底层脚本没有返回 JSON: {}", truncate_chars(detail, 300)),
            })
        }
    }
}

/// 把管道读到结束；返回 JoinHandle<Vec<u8>>。
fn spawn_read<R>(pipe: Option<R>) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf).await;
        }
        buf
    })
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
