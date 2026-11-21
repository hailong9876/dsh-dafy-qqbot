//! 本地探活：TCP 连通 + 极简 HTTP GET。
//!
//! 不用重型 HTTP 客户端，避免为了探活引入额外依赖；只处理本机服务
//! 会返回的 Content-Length / chunked / 连接关闭三种情况。

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// 端口是否可建立 TCP 连接。
pub async fn tcp_open(port: u16, dur: Duration) -> bool {
    matches!(timeout(dur, TcpStream::connect(("127.0.0.1", port))).await, Ok(Ok(_)))
}

/// 发一个最小 GET 请求。任何完整的 HTTP 响应（含 4xx/5xx）都算成功。
pub async fn http_get(port: u16, path: &str, dur: Duration) -> Option<HttpResponse> {
    let fut = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.ok()?;
        parse_response(&buf)
    };
    timeout(dur, fut).await.ok().flatten()
}

/// 健康接口返回 `{"ok":true}` 才算健康；解析失败视为不健康。
pub async fn json_ok(port: u16, path: &str, dur: Duration) -> bool {
    match http_get(port, path, dur).await {
        Some(r) if r.status < 500 => json_bool_field(&r.body, "ok").unwrap_or(false),
        _ => false,
    }
}

fn parse_response(buf: &[u8]) -> Option<HttpResponse> {
    let sep = find_subslice(buf, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..sep]).ok()?;
    let body_raw = &buf[sep + 4..];
    let mut lines = head.lines();
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("transfer-encoding:") {
            chunked = v.contains("chunked");
        } else if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        }
    }

    let body_bytes = if chunked {
        decode_chunked(body_raw)
    } else if let Some(n) = content_length {
        body_raw.get(..n.min(body_raw.len())).unwrap_or(body_raw).to_vec()
    } else {
        body_raw.to_vec()
    };

    Some(HttpResponse {
        status,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    })
}

fn decode_chunked(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(pos) = find_subslice(data, b"\r\n") else { break };
        let size_line = String::from_utf8_lossy(&data[..pos]);
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else { break };
        data = &data[pos + 2..];
        if size == 0 {
            break;
        }
        let end = size.min(data.len());
        out.extend_from_slice(&data[..end]);
        data = data.get(end + 2..).unwrap_or(&[]);
        if data.is_empty() {
            break;
        }
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 简易 JSON 字段判断：用于不引入 serde 的探活场景。
pub fn json_bool_field(body: &str, field: &str) -> Option<bool> {
    let needle = format!("\"{field}\"");
    let i = body.find(&needle)?;
    let rest = &body[i + needle.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}
