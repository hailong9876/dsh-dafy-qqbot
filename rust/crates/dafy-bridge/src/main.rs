//! 大肥鱼 · QQ 机器人控制服务（Rust 版 bridge，默认端口 18902）。
//!
//! HTTP 契约与旧 `qqbot/bridge/bridge.js` 保持一致；真实机器人仍由
//! `start-qqbot.ps1` + dsh 启动，TTS / Agent Lightning / Harness2 仍复用原脚本。

mod agl;
mod bot;
mod config;
mod h2;
mod oll;
mod proactive;
mod remote;
mod scheduled;
mod tts;
mod util;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response};
use axum::routing::any;
use axum::Router;
use config::Paths;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tts::TTS_PORT;

pub struct App {
    pub paths: Paths,
    pub logger: Arc<dafy_core::logger::Logger>,
    pub http: reqwest::Client,
    pub st: Mutex<bot::BotState>,
    pub mem: Mutex<bot::MemWatch>,
    pub qq_token: Mutex<(String, i64)>,
    pub tts_pid: Mutex<Option<u32>>,
    pub edge_pid: Mutex<Option<u32>>,
    pub proactive_busy: AtomicBool,
    pub h2_epoch: AtomicU64,
    pub last_mem_count: AtomicUsize,
    pub port: u16,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let port: u16 = std::env::var("DAFY_BRIDGE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18902);
    let root = dafy_core::project_root();
    let paths = Paths::new(&root);

    let logger = Arc::new(
        dafy_core::logger::Logger::init(root.join("rust").join("logs"), "bridge")
            .unwrap_or_else(|_| dafy_core::logger::Logger::console_only("bridge")),
    );
    logger.info(format!("项目目录: {}", root.display()));

    let http = reqwest::Client::builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let app = Arc::new(App {
        paths,
        logger: logger.clone(),
        http,
        st: Mutex::new(bot::BotState::default()),
        mem: Mutex::new(bot::MemWatch::default()),
        qq_token: Mutex::new((String::new(), 0)),
        tts_pid: Mutex::new(None),
        edge_pid: Mutex::new(None),
        proactive_busy: AtomicBool::new(false),
        h2_epoch: AtomicU64::new(0),
        last_mem_count: AtomicUsize::new(0),
        port,
    });

    start_background(app.clone());

    let router = Router::new()
        .fallback(any(handle))
        .with_state(app.clone());

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            let msg = e.to_string();
            logger.error(format!("端口 {port} 监听失败: {msg}"));
            // 与 Node 旧实现一致：端口被占用时静默退出，让守护进程判定已就绪。
            std::process::exit(0);
        }
    };
    logger.info(format!("bridge 已启动: http://127.0.0.1:{port}"));
    if let Err(e) = axum::serve(listener, router).await {
        logger.error(format!("bridge 异常退出: {e}"));
    }
}

fn start_background(app: Arc<App>) {
    // 只读烟测用：只起 HTTP 服务，不启动/接管任何外部进程。
    if std::env::var("DAFY_BRIDGE_NOSTART").map(|v| v != "0").unwrap_or(false) {
        app.logger.warn("DAFY_BRIDGE_NOSTART=1：跳过所有后台任务");
        return;
    }
    oll::start(app.clone());
    {
        let a = app.clone();
        tokio::spawn(async move { ensure_ofm(&a).await });
    }
    bot::start_log_scan(app.clone());
    bot::start_memory_watch(app.clone());
    scheduled::start(app.clone());
    proactive::start(app.clone());

    let cfg = config::read_config(&app.paths);
    if cfg.get("autoStart") != Some(&Value::Bool(false)) {
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            bot::start_bot(&a);
        });
    }
    if util::truthy(&cfg["aglAutoStart"]) {
        let a = app.clone();
        tokio::spawn(async move {
            let _ = agl::start_all(&a).await;
        });
    }
    if cfg["tts"].get("enabled").map(|v| v != &Value::Bool(false)).unwrap_or(true)
        && tts::tts_installed(&app)
    {
        let a = app.clone();
        tokio::spawn(async move {
            tts::tts_start(&a).await;
        });
    }
    if tts::edge_installed(&app) {
        let a = app.clone();
        tokio::spawn(async move {
            tts::edge_start(&a).await;
        });
    }
}

fn node_path(root: &std::path::Path) -> PathBuf {
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

/// 与旧脚本一致：只有当模型地址指向本机 18900 且探活失败时才拉起 ofm。
async fn ensure_ofm(app: &Arc<App>) {
    let cfg = config::read_config(&app.paths);
    if util::js_string(&cfg["llmMode"]) == "ollama" {
        return;
    }
    let base = util::js_or_string(&cfg["deepseekBaseUrl"], "");
    if !base.contains("127.0.0.1:18900") && !base.contains("localhost:18900") {
        return;
    }
    if dafy_core::net::http_get(18900, "/v1/models", Duration::from_secs(2))
        .await
        .is_some()
    {
        return;
    }
    let dir = app
        .paths
        .root
        .parent()
        .map(|p| p.join("dsh-our-free-model"))
        .unwrap_or_else(|| app.paths.root.join("dsh-our-free-model"));
    let cli = dir.join("cli.mjs");
    if !cli.is_file() {
        app.logger.warn(format!("[ofm] 找不到 {}", cli.display()));
        return;
    }
    let mut cmd = Command::new(node_path(&app.paths.root));
    cmd.arg(&cli)
        .args(["--port", "18900"])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    match cmd.spawn() {
        Ok(child) => app
            .logger
            .info(format!("[ofm] 已拉起本地模型服务（pid {}）", child.id())),
        Err(e) => app.logger.error(format!("[ofm] 启动失败: {e}")),
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let headers = req.headers().clone();

    if method == Method::OPTIONS {
        return status_204();
    }
    let is_write = matches!(method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE);
    let body_bytes = if is_write {
        to_bytes(req.into_body(), 4_000_000)
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let body: Value = if body_bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or_else(|_| json!({}))
    };

    dispatch(app, method, &path, &query, &headers, body, body_bytes).await
}

async fn dispatch(
    app: Arc<App>,
    method: Method,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    body: Value,
    raw: Vec<u8>,
) -> Response<Body> {
    match path {
        "/api/health" => return ok(json!({ "ok": true })),
        "/api/model" => {
            if method == Method::POST {
                return ok(model_set(&app, &body));
            }
            return ok(model_get(&app));
        }
        "/api/status" => return ok(status_json(&app)),
        "/api/start" => return ok(bot::start_bot(&app)),
        "/api/stop" => return ok(bot::stop_bot(&app)),
        "/api/proactive" => {
            if method == Method::POST {
                let mut cfg = config::read_config(&app.paths);
                let cur = cfg["proactive"].clone();
                let cur = if cur.is_object() { cur } else { json!({}) };
                let pick = |k: &str| -> Value {
                    match body.get(k) {
                        Some(v) if !v.is_null() => v.clone(),
                        _ => cur.get(k).cloned().unwrap_or(Value::Null),
                    }
                };
                let fallback = if body["fallbackModels"].is_array() {
                    json!(util::string_array(&body["fallbackModels"]))
                } else if cur["fallbackModels"].is_array() {
                    json!(util::string_array(&cur["fallbackModels"]))
                } else {
                    Value::Null
                };
                let mut p = json!({
                    "enabled": util::truthy(&pick("enabled")),
                    "intervalMinutes": util::clamp_num(Some(&pick("intervalMinutes")), 5, 1440, 60),
                    "minGapMinutes": util::clamp_num(Some(&pick("minGapMinutes")), 10, 2880, 180),
                    "dailyLimit": util::clamp_num(Some(&pick("dailyLimit")), 1, 20, 3),
                    "startHour": util::clamp_num(Some(&pick("startHour")), 0, 23, 9),
                    "endHour": util::clamp_num(Some(&pick("endHour")), 0, 24, 23),
                    "targetType": if util::js_or_string(&pick("targetType"), "") == "group" { "group" } else { "c2c" },
                    "targetId": util::js_string(&pick("targetId")),
                    "hint": util::js_string(&pick("hint")),
                });
                if !fallback.is_null() {
                    p["fallbackModels"] = fallback;
                }
                cfg["proactive"] = p;
                config::write_config(&app.paths, &cfg);
                bot::push_line(
                    &app,
                    &format!(
                        "[主动] 设置已保存：{}",
                        if util::truthy(&cfg["proactive"]["enabled"]) { "已开启" } else { "已关闭" }
                    ),
                );
                return ok(json!({ "ok": true, "cfg": proactive::proactive_cfg(&app) }));
            }
        }
        "/api/proactive/test" => {
            if method == Method::POST {
                return ok(proactive::tick(&app, true).await);
            }
        }
        "/api/harness2/status" => return ok(h2::status(&app).await),
        "/api/harness2/start" => {
            if method == Method::POST {
                return ok(h2::start(&app).await);
            }
        }
        "/api/harness2/stop" => {
            if method == Method::POST {
                return ok(h2::stop(&app).await);
            }
        }
        "/api/agl/status" => return ok(agl::status(&app).await),
        "/api/agl/start" => {
            if method == Method::POST {
                return ok(agl::start_all(&app).await);
            }
        }
        "/api/agl/stop" => {
            if method == Method::POST {
                return ok(agl::stop_all(&app));
            }
        }
        "/api/agl/trainer" => {
            if method == Method::POST {
                return ok(agl::start_trainer(&app));
            }
        }
        "/api/tts/status" => return ok(tts::tts_status(&app).await),
        "/api/tts/start" => {
            if method == Method::POST {
                return ok(tts::tts_start(&app).await);
            }
        }
        "/api/tts/stop" => {
            if method == Method::POST {
                return ok(tts::tts_stop(&app).await);
            }
        }
        "/api/tts2/status" => return ok(tts::edge_status(&app).await),
        "/api/tts2/start" => {
            if method == Method::POST {
                return ok(tts::edge_start(&app).await);
            }
        }
        "/api/tts2/stop" => {
            if method == Method::POST {
                return ok(tts::edge_stop(&app).await);
            }
        }
        "/api/tts-cache" => return ok(tts::cache_info(&app)),
        "/api/tts-cache/clear" => {
            if method == Method::POST {
                return ok(tts::cache_clear(&app, &body));
            }
        }
        "/api/rebind" => {
            if method == Method::POST {
                return rebind(&app);
            }
        }
        "/api/open-group" => {
            if method == Method::POST {
                let mut cfg = config::read_config(&app.paths);
                cfg["groupOpen"] = Value::Bool(true);
                config::write_config(&app.paths, &cfg);
                restart_if_running(&app);
                return ok(json!({ "ok": true }));
            }
        }
        "/api/open-bind" => {
            if method == Method::POST {
                return open_bind(&app);
            }
        }
        "/api/whitelist" => {
            if method == Method::POST {
                return save_whitelist(&app, &body);
            }
        }
        "/api/memory-sync" => {
            if method == Method::POST {
                return memory_sync(&app);
            }
        }
        "/api/fish-tone" => {
            if method == Method::POST {
                return save_fish_tone(&app, &body);
            }
            if method == Method::GET {
                return ok(fish_tone_get(&app));
            }
        }
        "/api/fish-tone/style" => {
            if method == Method::POST {
                return save_fish_style(&app);
            }
            if method == Method::GET {
                return ok(fish_style_get(&app));
            }
        }
        "/api/scheduled-tasks" => {
            if method == Method::POST {
                let (code, v) = scheduled::create(&app, &body);
                return json_status(code, v);
            }
            return ok(scheduled::list(&app));
        }
        _ => {}
    }

    if let Some(rest) = path.strip_prefix("/api/scheduled-tasks/") {
        if let Some(id) = rest.strip_suffix("/run") {
            if method == Method::POST {
                let (code, v) = scheduled::run_now(&app, &util::percent_decode(id)).await;
                return json_status(code, v);
            }
        } else if !rest.is_empty() && !rest.contains('/') {
            let id = util::percent_decode(rest);
            return match method {
                Method::PUT => {
                    let (code, v) = scheduled::update(&app, &id, &body);
                    json_status(code, v)
                }
                Method::DELETE => {
                    let (code, v) = scheduled::remove(&app, &id);
                    json_status(code, v)
                }
                _ => not_found(),
            };
        }
    }

    if let Some(rest) = path.strip_prefix("/api/agl/component/") {
        let (name, action) = match rest.split_once('/') {
            Some(v) => v,
            None => (rest, ""),
        };
        let name = name.to_string();
        if !matches!(name.as_str(), "gateway" | "controller" | "trainer") {
            return not_found();
        }
        if method != Method::POST {
            return not_found();
        }
        return match action {
            "start" => ok(agl::start_component(&app, &name)),
            "stop" => ok(agl::stop_component(&app, &name)),
            _ => not_found(),
        };
    }

    // TTS 反向代理：/api/tts/xxx → 18903，/api/tts2/xxx → 18904
    if let Some(rest) = path.strip_prefix("/api/tts") {
        if rest.starts_with('/') {
            let sub = rest;
            if !dafy_core::net::http_get(TTS_PORT, "/health", Duration::from_millis(500))
                .await
                .map(|r| r.body.contains("\"ok\":true"))
                .unwrap_or(false)
            {
                tts::tts_start(&app).await;
            }
            return tts::proxy(
                &app,
                TTS_PORT,
                &format!("{sub}{query}"),
                method,
                headers,
                raw,
                "TTS",
            )
            .await;
        }
    }
    if let Some(rest) = path.strip_prefix("/api/tts2") {
        if rest.starts_with('/') {
            let sub = rest;
            if !dafy_core::net::http_get(tts::EDGE_TTS_PORT, "/health", Duration::from_millis(500))
                .await
                .map(|r| r.body.contains("\"ok\":true"))
                .unwrap_or(false)
            {
                tts::edge_start(&app).await;
            }
            return tts::proxy(
                &app,
                tts::EDGE_TTS_PORT,
                &format!("{sub}{query}"),
                method,
                headers,
                raw,
                "Edge TTS",
            )
            .await;
        }
    }

    not_found()
}

/* ------------------------------- 基础接口 ------------------------------- */

fn model_mode(cfg: &Value) -> &'static str {
    if util::js_string(&cfg["llmMode"]) == "ollama" {
        "ollama"
    } else if util::truthy(&cfg["useLocalGateway"]) {
        "gw"
    } else {
        "ds"
    }
}

fn model_of(cfg: &Value, mode: &str) -> String {
    match mode {
        "ollama" => util::js_string(&cfg["ollamaModel"]),
        "gw" => util::js_string(&cfg["gatewayModel"]),
        _ => util::js_string(&cfg["deepseekModel"]),
    }
}

fn model_get(app: &Arc<App>) -> Value {
    let cfg = config::read_config(&app.paths);
    let mode = model_mode(&cfg);
    json!({
        "ok": true,
        "mode": mode,
        "model": model_of(&cfg, mode),
        "visionModel": util::js_string(&cfg["visionModel"]),
    })
}

fn model_set(app: &Arc<App>, body: &Value) -> Value {
    let mut cfg = config::read_config(&app.paths);
    let llm_mode = util::js_string(&body["llmMode"]);
    if !llm_mode.is_empty() {
        cfg["llmMode"] = Value::String(llm_mode);
    }
    let model = util::js_string(&body["model"]);
    let base = util::js_string(&body["base"]);
    if util::js_string(&cfg["llmMode"]) == "ollama" {
        if !model.is_empty() {
            cfg["ollamaModel"] = Value::String(model);
        }
        if !base.is_empty() {
            cfg["ollamaBase"] = Value::String(base);
        }
    } else if util::truthy(&cfg["useLocalGateway"]) {
        if !model.is_empty() {
            cfg["gatewayModel"] = Value::String(model);
        }
        if !base.is_empty() {
            cfg["gatewayBase"] = Value::String(base);
        }
    } else {
        if !model.is_empty() {
            cfg["deepseekModel"] = Value::String(model);
        }
        if !base.is_empty() {
            cfg["deepseekBaseUrl"] = Value::String(base);
        }
    }
    let vision = util::js_string(&body["visionModel"]);
    if !vision.is_empty() {
        cfg["visionModel"] = Value::String(vision);
    }
    config::write_config(&app.paths, &cfg);
    config::write_llm_patch(&app.paths, &cfg);
    restart_if_running(app);
    let mode = util::js_string(&cfg["llmMode"]);
    let model = if mode == "ollama" {
        util::js_string(&cfg["ollamaModel"])
    } else {
        util::js_or_string(
            &cfg["deepseekModel"],
            &util::js_string(&cfg["gatewayModel"]),
        )
    };
    json!({ "ok": true, "mode": mode, "model": model })
}

fn status_json(app: &Arc<App>) -> Value {
    let cfg = config::read_config(&app.paths);
    let st = app.st.lock().unwrap();
    let ollama = util::js_string(&cfg["llmMode"]) == "ollama";
    let bound = st.bind_url.is_empty() && st.lines.iter().any(|l| l.contains("Bot ready!"));
    let qr = st.qr.clone();
    let bind_url = st.bind_url.clone();
    let running = st.running;
    let log: Vec<String> = st.lines.iter().rev().take(6).cloned().collect::<Vec<_>>();
    let log: Vec<String> = log.into_iter().rev().collect();
    drop(st);

    let (app_id, _app_secret) = config::profile_credentials(&app.paths);
    let c2c = util::string_array(&cfg["c2cAllow"]);
    let group = util::string_array(&cfg["groupAllow"]);
    let p = proactive::proactive_cfg(app);
    let target = {
        let id = util::js_string(&p["targetId"]);
        if !id.is_empty() {
            id
        } else {
            let list = if util::js_string(&p["targetType"]) == "group" {
                group.clone()
            } else {
                c2c.clone()
            };
            list.first().cloned().unwrap_or_default()
        }
    };
    let state = proactive::load_state(app);
    json!({
        "bridge": true,
        "running": running,
        "bound": bound,
        "qr": qr,
        "bindUrl": bind_url,
        "appId": app_id,
        "hasCred": !app_id.is_empty(),
        "model": if ollama { util::js_string(&cfg["ollamaModel"]) } else { util::js_string(&cfg["deepseekModel"]) },
        "c2c": c2c,
        "group": group,
        "memCount": app.last_mem_count.load(Ordering::SeqCst),
        "proactive": {
            "cfg": p,
            "state": state,
            "target": target,
        },
        "log": log,
    })
}

fn restart_if_running(app: &Arc<App>) {
    if bot::is_running(app) {
        bot::stop_bot(app);
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            bot::start_bot(&a);
        });
    }
}

fn rebind(app: &Arc<App>) -> Response<Body> {
    bot::stop_bot(app);
    let profile = &app.paths.profile;
    let stamp = util::iso_now().replace([':', '.'], "-");
    if profile.is_file() {
        let bak = profile.with_file_name(format!(
            "{}.bak-{stamp}",
            profile.file_name().unwrap_or_default().to_string_lossy()
        ));
        if let Err(e) = fs::copy(profile, &bak) {
            return ok(json!({ "error": e.to_string() }));
        }
    }
    if let Err(e) = util::write_text(profile, "[]\n") {
        return ok(json!({ "error": e.to_string() }));
    }
    let mut cfg = config::read_config(&app.paths);
    cfg["discover"] = Value::Bool(true);
    config::write_config(&app.paths, &cfg);
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        bot::start_bot(&a);
    });
    ok(json!({ "ok": true, "restarting": true }))
}

fn open_bind(app: &Arc<App>) -> Response<Body> {
    bot::stop_bot(app);
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-Command",
        "Start-Process -FilePath '.\\start-qqbot.cmd'",
    ])
    .current_dir(&app.paths.qb)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let _ = cmd.spawn();
    ok(json!({ "ok": true }))
}

fn save_whitelist(app: &Arc<App>, body: &Value) -> Response<Body> {
    let mut cfg = config::read_config(&app.paths);
    let c2c = util::string_array(&body["c2c"]);
    let group = util::string_array(&body["group"]);
    cfg["c2cAllow"] = json!(c2c);
    cfg["groupAllow"] = json!(group);
    cfg["discover"] = Value::Bool(false);
    cfg["groupOpen"] = Value::Bool(false);
    config::write_config(&app.paths, &cfg);
    let was_running = bot::is_running(app);
    restart_if_running(app);
    ok(json!({ "ok": true, "restarting": was_running }))
}

fn memory_sync(app: &Arc<App>) -> Response<Body> {
    let mem = config::load_shared_memory(&app.paths);
    let running = bot::is_running(app);
    bot::push_line(
        app,
        &format!(
            "[bridge] 记忆同步：{} 条（{}）",
            mem.len(),
            app.paths.conv.display()
        ),
    );
    restart_if_running(app);
    ok(json!({
        "ok": true,
        "restarting": running,
        "memCount": mem.len(),
        "memDir": app.paths.conv.display().to_string(),
    }))
}

/* ------------------------------ 鱼音配置 ------------------------------ */

fn fish_main_path(app: &Arc<App>) -> PathBuf {
    app.paths
        .qb
        .join("plugins")
        .join("dsh-fish-tone")
        .join("packages")
        .join("dsh-fish-tone")
        .join("config.json")
}

fn fish_flavor_path(app: &Arc<App>) -> PathBuf {
    app.paths
        .qb
        .join("plugins")
        .join("dsh-fish-tone")
        .join("packages")
        .join("dsh-fish-tone-flavor")
        .join("config.json")
}

fn fish_style_path(app: &Arc<App>) -> PathBuf {
    app.paths
        .qb
        .join("plugins")
        .join("dsh-fish-tone")
        .join("packages")
        .join("dsh-fish-tone-flavor")
        .join("style-profile.json")
}

fn fish_main_defaults() -> Value {
    json!({ "enabled": true, "strength": "light", "seed": 7, "grammar": true, "marks": "light" })
}

fn fish_flavor_defaults() -> Value {
    json!({ "enabled": false, "mode": "cute", "rate": "low", "seed": 7 })
}

/// 旧实现 `{ ...def, ...JSON.parse(file) }`。
fn fish_read_merged(path: &std::path::Path, defaults: Value) -> Value {
    let cur = util::read_json_or(path, json!({}));
    let mut out = defaults.as_object().cloned().unwrap_or_default();
    if let Some(obj) = cur.as_object() {
        for (k, v) in obj {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn one_of(v: &Value, set: &[&str], dflt: &str) -> String {
    let s = v.as_str();
    match s {
        Some(s) if set.contains(&s) => s.to_string(),
        _ => dflt.to_string(),
    }
}

/// 旧实现 `Number.isFinite(v) ? v : def`（只认数字，不解析字符串）。
fn finite_or(v: &Value, dflt: i64) -> Value {
    match v.as_f64() {
        Some(f) if f.is_finite() => json!(f),
        _ => json!(dflt),
    }
}

fn fish_tone_get(app: &Arc<App>) -> Value {
    json!({
        "main": fish_read_merged(&fish_main_path(app), fish_main_defaults()),
        "flavor": fish_read_merged(&fish_flavor_path(app), fish_flavor_defaults()),
    })
}

fn save_fish_tone(app: &Arc<App>, body: &Value) -> Response<Body> {
    let bmain = &body["main"];
    let bflavor = &body["flavor"];
    let main = json!({
        "enabled": bmain["enabled"].as_bool().unwrap_or(true),
        "strength": one_of(&bmain["strength"], &["off", "light", "normal", "max"], "light"),
        "seed": finite_or(&bmain["seed"], 7),
        "grammar": bmain["grammar"].as_bool().unwrap_or(true),
        "marks": one_of(&bmain["marks"], &["off", "light", "normal", "max"], "light"),
    });
    let flavor = json!({
        "enabled": bflavor["enabled"].as_bool().unwrap_or(false),
        "mode": one_of(&bflavor["mode"], &["cute", "abstract"], "cute"),
        "rate": one_of(&bflavor["rate"], &["low", "normal", "high"], "low"),
        "seed": finite_or(&bflavor["seed"], 7),
    });
    let _ = util::write_json(&fish_main_path(app), &main, true);
    let _ = util::write_json(&fish_flavor_path(app), &flavor, true);
    let restarting = bot::is_running(app);
    restart_if_running(app);
    ok(json!({ "ok": true, "restarting": restarting, "main": main, "flavor": flavor }))
}

fn top_entries(v: &Value, n: usize) -> Vec<Value> {
    let mut items: Vec<(String, f64)> = v
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, val)| (k.clone(), val.as_f64().unwrap_or(0.0)))
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    items
        .into_iter()
        .take(n)
        .map(|(k, v)| {
            if v.fract() == 0.0 {
                json!({ "k": k, "v": v as i64 })
            } else {
                json!({ "k": k, "v": v })
            }
        })
        .collect()
}

fn fish_style_get(app: &Arc<App>) -> Value {
    let Some(st) = util::read_text(&fish_style_path(app))
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return json!({ "ok": true, "learning": false, "samples": 0, "recent": [] });
    };
    if !st.is_object() {
        return json!({ "ok": true, "learning": false, "samples": 0, "recent": [] });
    }
    let samples = st["samples"].as_i64().unwrap_or(0);
    let total_len = st["totalLen"].as_i64().unwrap_or(0);
    let avg_len = if samples != 0 {
        (total_len as f64 / samples as f64).round() as i64
    } else {
        0
    };
    let recent: Vec<Value> = util::as_array(&st["recent"])
        .into_iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    json!({
        "ok": true,
        "learning": samples >= 5,
        "samples": samples,
        "avgLen": avg_len,
        "particles": top_entries(&st["particles"], 5),
        "words": top_entries(&st["words"], 8),
        "punct": if st["punct"].is_object() { st["punct"].clone() } else { json!({}) },
        "recent": recent,
        "updatedAt": util::js_string(&st["updatedAt"]),
    })
}

fn save_fish_style(app: &Arc<App>) -> Response<Body> {
    let reset = json!({
        "samples": 0,
        "totalLen": 0,
        "particles": {},
        "punct": { "!": 0, "?": 0, "~": 0, "…": 0 },
        "words": {},
        "recent": [],
        "updatedAt": "",
    });
    let _ = util::write_json(&fish_style_path(app), &reset, true);
    ok(json!({ "ok": true }))
}

/* ------------------------------- 响应助手 ------------------------------- */

fn ok(v: Value) -> Response<Body> {
    json_status(200, v)
}

fn json_status(code: u16, v: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-headers", "content-type")
        .header("access-control-allow-methods", "GET,POST,OPTIONS")
        .header("cache-control", "no-store")
        .body(Body::from(v.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("{}")))
}

fn not_found() -> Response<Body> {
    json_status(404, json!({ "error": "not found" }))
}

fn status_204() -> Response<Body> {
    Response::builder()
        .status(204)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET,POST,PUT,DELETE,OPTIONS")
        .header("access-control-allow-headers", "content-type,authorization")
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
