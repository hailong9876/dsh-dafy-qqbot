//! OpenAI 兼容代理（127.0.0.1:11435）→ 本机 Ollama(11434)。
//!
//! 旧 bridge.js 用 Node http 手写的这段转换逻辑，这里逐行对齐：
//! 非 chat 请求原样直通；chat 请求转成 /api/chat，并把结果重新包装成
//! OpenAI 的 JSON 或 SSE 流。

use crate::App;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const PROXY_PORT: u16 = 11435;
const OLLAMA_BASE: &str = "http://127.0.0.1:11434";

pub fn start(app: Arc<App>) {
    tokio::spawn(async move {
        let router = Router::new().fallback(any(move |req: axum::http::Request<Body>| {
            let a = app.clone();
            async move { handle(a, req).await }
        }));
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", PROXY_PORT)).await {
            Ok(l) => l,
            Err(_) => return,
        };
        let _ = axum::serve(listener, router.into_make_service()).await;
    });
}

async fn handle(
    app: Arc<App>,
    req: axum::http::Request<Body>,
) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let headers = req.headers().clone();
    if method == Method::OPTIONS {
        return empty(204);
    }
    let body_bytes = axum::body::to_bytes(req.into_body(), 8_000_000)
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();

    if method == Method::POST && path == "/v1/chat/completions" {
        return chat(app, &headers, body_bytes).await;
    }
    passthrough(app, method, &format!("{path}{query}"), headers, body_bytes).await
}

async fn passthrough(
    app: Arc<App>,
    method: Method,
    path_query: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Response<Body> {
    let mut rb = app
        .http
        .request(method, format!("{OLLAMA_BASE}{path_query}"))
        .body(body);
    if let Some(ct) = headers.get("content-type") {
        rb = rb.header("content-type", ct.as_bytes());
    }
    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            let mut builder = Response::builder().status(status);
            if let Some(ct) = resp.headers().get("content-type") {
                builder = builder.header("content-type", ct.as_bytes());
            }
            builder
                .header("access-control-allow-origin", "*")
                .header("access-control-allow-headers", "*")
                .header("access-control-allow-methods", "GET,POST,OPTIONS")
                .body(Body::from_stream(resp.bytes_stream()))
                .unwrap_or_else(|_| empty(502))
        }
        Err(_) => json_response(
            502,
            json!({ "error": "ollama 不可用" }),
        ),
    }
}

async fn chat(app: Arc<App>, headers: &HeaderMap, body: Vec<u8>) -> Response<Body> {
    let _ = headers;
    let parsed: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let stream = parsed["stream"].as_bool().unwrap_or(false);
    let model = if parsed["model"].is_string() {
        parsed["model"].as_str().unwrap_or("").to_string()
    } else {
        "deepseek-r1:14b".to_string()
    };
    let messages: Vec<Value> = parsed["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| {
                    let content = match &m["content"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    json!({ "role": m["role"].clone(), "content": content })
                })
                .collect()
        })
        .unwrap_or_default();
    let payload = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "options": {
            "num_predict": parsed["max_tokens"].as_i64().unwrap_or(200),
            "temperature": parsed["temperature"].as_f64().unwrap_or(0.6),
        },
    });
    let resp = app
        .http
        .post(format!("{OLLAMA_BASE}/api/chat"))
        .header("content-type", "application/json")
        .body(payload.to_string())
        .timeout(Duration::from_secs(300))
        .send()
        .await;
    let Ok(resp) = resp else {
        return json_response(502, json!({ "error": "ollama 不可用" }));
    };
    let data: Value = resp.json().await.unwrap_or(Value::Null);
    let text = data["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let created = chrono::Utc::now().timestamp();

    if stream {
        let id = format!("chatcmpl-{}", crate::util::now_ms());
        let chunk = |delta: Value, finish: Value| -> String {
            let obj = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            });
            format!("data: {obj}\n\n")
        };
        let mut sse = String::new();
        sse.push_str(&chunk(json!({"role":"assistant","content":""}), Value::Null));
        let chars: Vec<char> = text.chars().collect();
        for part in chars.chunks(60) {
            sse.push_str(&chunk(
                json!({ "content": part.iter().collect::<String>() }),
                Value::Null,
            ));
        }
        sse.push_str(&chunk(json!({}), json!("stop")));
        sse.push_str("data: [DONE]\n\n");
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream; charset=utf-8")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .header("access-control-allow-origin", "*")
            .body(Body::from(sse))
            .unwrap_or_else(|_| empty(500));
    }

    json_response(
        200,
        json!({
            "id": format!("chatcmpl-{}", crate::util::now_ms()),
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 },
        }),
    )
}

fn empty(code: u16) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn json_response(code: u16, v: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .body(Body::from(v.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
