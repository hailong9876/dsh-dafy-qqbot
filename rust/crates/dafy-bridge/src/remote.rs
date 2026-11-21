//! 外部 HTTP：模型 chat/completions 与 QQ 机器人开放平台。

use crate::util;
use crate::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub async fn request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> anyhow::Result<(u16, String)> {
    let mut rb = client.request(method, url).timeout(timeout);
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        rb = rb.body(b);
    }
    let resp = rb.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

/// 等价 bridge.js 的 chatComplete()。
pub async fn chat_complete(
    app: &Arc<App>,
    messages: &Value,
    model: Option<&str>,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    timeout: Option<Duration>,
) -> anyhow::Result<String> {
    let cfg = crate::config::read_config(&app.paths);
    let ollama = util::js_string(&cfg["llmMode"]) == "ollama";
    let gw = !ollama && util::truthy(&cfg["useLocalGateway"]);
    let base = if ollama {
        util::js_or_string(&cfg["ollamaBase"], "http://127.0.0.1:11434/v1")
    } else if gw {
        util::js_or_string(&cfg["gatewayBase"], "http://127.0.0.1:18899/v1")
    } else {
        util::js_or_string(&cfg["deepseekBaseUrl"], "https://api.deepseek.com")
    };
    let model = match model {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            if ollama {
                util::js_string(&cfg["ollamaModel"])
            } else if gw {
                util::js_string(&cfg["gatewayModel"])
            } else {
                util::js_or_string(&cfg["deepseekModel"], "deepseek-chat")
            }
        }
    };
    if model.is_empty() {
        anyhow::bail!("未配置模型（先在大屏 ⚙ AI 接口设置里选一个）");
    }
    let key = if ollama {
        String::new()
    } else if gw {
        util::js_string(&cfg["gatewayKey"])
    } else {
        util::js_string(&cfg["deepseekApiKey"])
    };

    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if !key.is_empty() {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    let payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens.unwrap_or(200),
        "temperature": temperature.unwrap_or(0.95),
        "stream": false,
    });
    let url = format!(
        "{}/chat/completions",
        base.trim_end_matches('/')
    );
    let (status, body) = request(
        &app.http,
        reqwest::Method::POST,
        &url,
        &headers,
        Some(payload.to_string().into_bytes()),
        timeout.unwrap_or(Duration::from_secs(90)),
    )
    .await?;
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "模型 HTTP {status}: {}",
            body.chars().take(160).collect::<String>()
        );
    }
    let obj: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let content = obj["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(content)
}

fn qq_api_base() -> String {
    std::env::var("QQBOT_BASE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.bot.qq.com".to_string())
}

async fn get_access_token(app: &Arc<App>) -> anyhow::Result<String> {
    let now = util::now_ms();
    {
        let cache = app.qq_token.lock().unwrap();
        if !cache.0.is_empty() && now < cache.1 - 60_000 {
            return Ok(cache.0.clone());
        }
    }
    let (app_id, app_secret) = crate::config::profile_credentials(&app.paths);
    if app_id.is_empty() || app_secret.is_empty() {
        anyhow::bail!("未配置 QQ 机器人 appId/appSecret");
    }
    let payload = json!({ "appId": app_id, "clientSecret": app_secret });
    let (_, body) = request(
        &app.http,
        reqwest::Method::POST,
        &format!("{}/app/getAppAccessToken", qq_api_base()),
        &[("Content-Type".into(), "application/json".into())],
        Some(payload.to_string().into_bytes()),
        Duration::from_secs(120),
    )
    .await?;
    let obj: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let token = obj["access_token"].as_str().unwrap_or("");
    if token.is_empty() {
        anyhow::bail!("获取 access_token 失败: {body}");
    }
    let expires = obj["expires_in"].as_i64().unwrap_or(7200);
    let mut cache = app.qq_token.lock().unwrap();
    *cache = (token.to_string(), now + expires * 1000);
    Ok(cache.0.clone())
}

/// 等价 sendQQMessage()：主动消息走开放平台 REST API。
pub async fn send_qq_message(
    app: &Arc<App>,
    target_type: &str,
    target_id: &str,
    content: &str,
) -> anyhow::Result<()> {
    if target_id.is_empty() || content.is_empty() {
        anyhow::bail!("targetId 和 content 不能为空");
    }
    let token = get_access_token(app).await?;
    let path = if target_type == "group" {
        format!("/v2/groups/{}/messages", util::percent_encode(target_id))
    } else {
        format!("/v2/users/{}/messages", util::percent_encode(target_id))
    };
    let payload = json!({ "content": content });
    let (status, body) = request(
        &app.http,
        reqwest::Method::POST,
        &format!("{}{}", qq_api_base(), path),
        &[
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), format!("QQBot {token}")),
        ],
        Some(payload.to_string().into_bytes()),
        Duration::from_secs(120),
    )
    .await?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        anyhow::bail!(
            "发送失败 ({status}): {}",
            body.chars().take(200).collect::<String>()
        )
    }
}
