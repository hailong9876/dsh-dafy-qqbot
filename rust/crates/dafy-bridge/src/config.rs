//! 配置与补丁文件：完全对齐 `qqbot/bridge/bridge.js` 的 preparePatches / writeLlmPatch。
//!
//! 注意：`start-qqbot.ps1` 随后也会重写这两份 patch，但 bridge 先写一遍是旧行为，
//! 并且 preparePatches 计算出的环境变量会传给机器人子进程，必须保持一致。

use crate::util;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Paths {
    pub root: PathBuf,
    pub qb: PathBuf,
    pub config: PathBuf,
    pub profile: PathBuf,
    pub log: PathBuf,
    pub tts: PathBuf,
    pub conv: PathBuf,
}

impl Paths {
    pub fn new(root: &Path) -> Self {
        let qb = root.join("qqbot");
        let conv = root
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("Conversation_Records"))
            .unwrap_or_else(|| root.join("Conversation_Records"));
        Self {
            root: root.to_path_buf(),
            config: qb.join("config.json"),
            profile: qb.join(".dsh").join("profiles").join("qqbot").join("cordis.patch.yml"),
            log: qb.join("bot.log"),
            tts: root.join("tts"),
            qb,
            conv,
        }
    }

    pub fn patch_dir(&self) -> PathBuf {
        self.qb.join("patch")
    }

    pub fn proactive_state(&self) -> PathBuf {
        self.qb.join("proactive-state.json")
    }

    pub fn script(&self) -> PathBuf {
        self.qb.join("start-qqbot.ps1")
    }
}

pub fn read_config(p: &Paths) -> Value {
    util::read_json_or(&p.config, json!({}))
}

pub fn write_config(p: &Paths, cfg: &Value) {
    let _ = util::write_json(&p.config, cfg, true);
}

/// 读取 profile patch 里的 appId / appSecret（等价正则 `appId:\s*"?([^"\r\n]+)"?`）。
pub fn profile_credentials(p: &Paths) -> (String, String) {
    let Some(text) = util::read_text(&p.profile) else {
        return (String::new(), String::new());
    };
    (extract_yaml_value(&text, "appId"), extract_yaml_value(&text, "appSecret"))
}

pub fn bound_app_id(p: &Paths) -> String {
    profile_credentials(p).0
}

fn extract_yaml_value(text: &str, key: &str) -> String {
    for line in text.lines() {
        let Some(pos) = line.find(key) else { continue };
        let rest = &line[pos + key.len()..];
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(':') else { continue };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        let end = rest
            .find(['"', '\r', '\n'])
            .unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    String::new()
}

fn list_yaml(name: &str, items: &[String], indent: &str) -> String {
    if items.is_empty() {
        return format!("{indent}{name}: []");
    }
    let body = items
        .iter()
        .map(|i| format!("{indent}  - {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{indent}{name}:\n{body}")
}

/// Conversation_Records/dafy_memory*.json 按修改时间倒序。
pub fn memory_files(p: &Paths) -> Vec<(PathBuf, i64)> {
    let Ok(entries) = fs::read_dir(&p.conv) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !(name.starts_with("dafy_memory") && name.ends_with(".json")) {
            continue;
        }
        let path = e.path();
        if let Some(t) = util::mtime_ms(&path) {
            out.push((path, t));
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

pub fn memory_latest_mtime(p: &Paths) -> i64 {
    memory_files(p).first().map(|(_, t)| *t).unwrap_or(0)
}

/// 与旧脚本一致的记忆筛选：视频记忆最多 12 条，个人相关优先，总数上限 60。
pub fn load_shared_memory(p: &Paths) -> Vec<String> {
    for (path, _) in memory_files(p) {
        let Ok(text) = util::read_text(&path).ok_or(()) else {
            continue;
        };
        let Ok(obj) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let facts = util::string_array(&obj["facts"]);
        if facts.is_empty() {
            continue;
        }
        let is_video = |s: &String| s.starts_with("【刷视频】");
        let mut personal: Vec<String> = facts.iter().filter(|s| !is_video(s)).take(48).cloned().collect();
        let video: Vec<String> = facts.iter().filter(|s| is_video(s)).take(12).cloned().collect();
        personal.extend(video);
        return personal;
    }
    Vec::new()
}

/// 按当前 llmMode 写 `patch/llm.patch.yml`；没有可用配置时不写。
pub fn write_llm_patch(p: &Paths, cfg: &Value) -> Option<PathBuf> {
    let dir = p.patch_dir();
    let _ = fs::create_dir_all(&dir);
    let llm_mode = util::js_string(&cfg["llmMode"]);
    let path = dir.join("llm.patch.yml");

    let text = if llm_mode == "ollama" {
        let base = util::js_or_string(&cfg["ollamaBase"], "http://127.0.0.1:11434/v1");
        let model = util::js_or_string(&cfg["ollamaModel"], "deepseek-r1:14b");
        Some(
            [
                "- id: llm-pi-ai",
                "  config:",
                "    providers:",
                "      ollama:",
                "        displayName: 本机 Ollama",
                "        apiKeyEnv: DAFY_OLLAMA_KEY",
                "        api: openai-completions",
                &format!("        baseURL: {base}"),
                "        models:",
                &format!("          - id: \"{model}\""),
                &format!("            name: \"{model}\""),
                "            contextWindow: 4096",
                "            maxTokens: 400",
                "- id: agent-default-model",
                "  config:",
                "    provider: ollama",
                &format!("    model: \"{model}\""),
            ]
            .join("\n"),
        )
    } else if util::truthy(&cfg["deepseekApiKey"]) {
        let base = util::js_or_string(&cfg["deepseekBaseUrl"], "https://api.deepseek.com");
        let model = util::js_or_string(&cfg["deepseekModel"], "deepseek-chat");
        Some(
            [
                "- id: llm-deepseek-account",
                "  disabled: true",
                "- id: llm-deepseek",
                "  config:",
                "    apiKeyEnv: DEEPSEEK_API_KEY",
                &format!("    baseURL: {base}"),
                "- id: web-search-deepseek",
                "  disabled: true",
                "- id: web-deepseek-api",
                "  disabled: true",
                "- id: agent-default-model",
                "  config:",
                "    provider: deepseek-official",
                &format!("    model: {model}"),
            ]
            .join("\n"),
        )
    } else if util::truthy(&cfg["useLocalGateway"]) {
        let model = util::js_or_string(&cfg["gatewayModel"], "deepseek-v4.1-flash-free");
        let base = util::js_or_string(&cfg["gatewayBase"], "http://127.0.0.1:18899/v1");
        Some(
            [
                "- id: llm-pi-ai",
                "  config:",
                "    providers:",
                "      dafy:",
                "        displayName: 大肥鱼本地网关",
                "        apiKeyEnv: DAFY_GW_KEY",
                "        api: openai-completions",
                &format!("        baseURL: {base}"),
                "        models:",
                &format!("          - id: {model}"),
                &format!("            name: {model}"),
                "            contextWindow: 32768",
                "            maxTokens: 4096",
                "- id: agent-default-model",
                "  config:",
                "    provider: dafy",
                &format!("    model: {model}"),
            ]
            .join("\n"),
        )
    } else {
        None
    };

    if let Some(text) = text {
        let _ = util::write_text(&path, &format!("{text}\n"));
        Some(path)
    } else {
        None
    }
}

pub struct Prep {
    pub envs: Vec<(String, String)>,
    pub mem_count: usize,
    pub mem_dir: String,
}


pub fn prepare_patches(p: &Paths, cfg: &Value) -> anyhow::Result<Prep> {
    let dir = p.patch_dir();
    fs::create_dir_all(&dir)?;

    let (app_id, app_secret) = profile_credentials(p);
    let c2c = util::string_array(&cfg["c2cAllow"]);
    let group = util::string_array(&cfg["groupAllow"]);
    let mode = if util::truthy(&cfg["discover"]) { "open" } else { "allowlist" };
    let gmode = if util::truthy(&cfg["groupOpen"]) || util::truthy(&cfg["discover"]) {
        "open"
    } else {
        "allowlist"
    };

    let mut wl: Vec<String> = vec!["- id: im-qqbot".into(), "  config:".into()];
    if !app_id.is_empty() && !app_secret.is_empty() {
        wl.push(format!("    appId: \"{app_id}\""));
        wl.push(format!("    appSecret: \"{app_secret}\""));
    }
    let require_mention = cfg.get("requireMention").map(|v| v != &Value::Bool(false)).unwrap_or(true);
    wl.push(format!("    requireMention: {require_mention}"));

    let mem = load_shared_memory(p);
    let mem_dir = p.conv.display().to_string();
    let _ = util::write_text(&dir.join("debug.txt"), &format!("mem={} dir={}", mem.len(), mem_dir));

    let mut prompt_parts: Vec<String> = Vec::new();
    if util::truthy(&cfg["persona"]) {
        prompt_parts.push(util::js_string(&cfg["persona"]));
    }
    if !mem.is_empty() {
        let facts = mem
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n");
        prompt_parts.push(format!("【关于用户的记忆】\n{facts}"));
    }
    prompt_parts.push("【媒体理解】消息里出现 \"Image: 路径\" 时，必须先调用 qqbot_look_image 工具真正看过图片再回答；出现 \"Video: 路径\" 时，必须先调用 qqbot_analyze_video 工具分析视频再回答。不要凭文件名猜测内容。".into());
    let prompt = prompt_parts.join("\n\n");

    if !prompt.is_empty() {
        wl.push("    directPrompt: |".into());
        for line in prompt.split('\n') {
            wl.push(format!("      {}", line.trim_end_matches('\r')));
        }
        wl.push("    groupPrompt: |".into());
        for line in prompt.split('\n') {
            wl.push(format!("      {}", line.trim_end_matches('\r')));
        }
    }

    wl.push("    access:".into());
    wl.push(format!("      c2cMode: {mode}"));
    wl.push(list_yaml("c2cAllow", &c2c, "      "));
    wl.push(format!("      groupMode: {gmode}"));
    wl.push(list_yaml("groupAllow", &group, "      "));
    wl.push("    media:".into());
    wl.push("      enabled: true".into());
    wl.push("      maxMB: 200".into());
    wl.push("      ttlHours: 24".into());
    wl.push("    vision:".into());
    wl.push("      enabled: true".into());
    wl.push("      provider: \"\"".into());
    wl.push("      model: \"\"".into());
    wl.push("      defaultPrompt: \"请详细描述这张图片里有什么。\"".into());

    let wl_path = dir.join("whitelist.patch.yml");
    util::write_text(&wl_path, &format!("{}\n", wl.join("\n")))?;

    // ── 子进程环境 ──
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let npm_dir = Path::new(&appdata).join("npm");
    let old_path = std::env::var("PATH").unwrap_or_default();
    let mut path = format!("{};{}", npm_dir.display(), old_path);

    let mut envs: Vec<(String, String)> = vec![
        ("DSH_HOME".into(), p.qb.join(".dsh").display().to_string()),
        ("PATH".into(), path.clone()),
        ("QQBOT_BIND_OPEN_BROWSER".into(), "0".into()),
    ];

    // FFmpeg：winget 安装的默认不在 PATH
    let local_appdata = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let winget_base = Path::new(&local_appdata).join("Microsoft").join("WinGet").join("Packages");
    if let Ok(entries) = fs::read_dir(&winget_base) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !name.starts_with("gyan.ffmpeg") {
                continue;
            }
            if let Some(bin_dir) = util::find_ffmpeg_bin(&e.path(), 0) {
                path = format!("{};{}", bin_dir.display(), path);
                envs.push(("PATH".into(), path.clone()));
                envs.push(("FFMPEG_PATH".into(), bin_dir.join("ffmpeg.exe").display().to_string()));
                break;
            }
        }
    }

    write_llm_patch(p, cfg);

    let llm_mode = util::js_string(&cfg["llmMode"]);
    if llm_mode == "ollama" {
        let base = util::js_or_string(&cfg["ollamaBase"], "http://127.0.0.1:11434/v1");
        let model = util::js_or_string(&cfg["ollamaModel"], "deepseek-r1:14b");
        envs.push(("DAFY_OLLAMA_KEY".into(), "ollama".into()));
        envs.push(("DSH_VISION_BASE_URL".into(), base));
        envs.push(("DSH_VISION_API_KEY".into(), "ollama".into()));
        envs.push(("DSH_VISION_MODEL".into(), model));
    } else if util::truthy(&cfg["deepseekApiKey"]) {
        let key = util::js_string(&cfg["deepseekApiKey"]);
        let base = util::js_or_string(&cfg["deepseekBaseUrl"], "https://api.deepseek.com");
        let model = util::js_or_string(&cfg["visionModel"], "mimo-v2.6-flash-free");
        envs.push(("DEEPSEEK_API_KEY".into(), key.clone()));
        envs.push(("DSH_VISION_BASE_URL".into(), base));
        envs.push(("DSH_VISION_API_KEY".into(), key));
        envs.push(("DSH_VISION_MODEL".into(), model));
    } else if util::truthy(&cfg["useLocalGateway"]) {
        envs.push(("DAFY_GW_KEY".into(), util::js_string(&cfg["gatewayKey"])));
    }

    Ok(Prep {
        envs,
        mem_count: mem.len(),
        mem_dir,
    })
}
