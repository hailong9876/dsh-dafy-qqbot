//! 与旧 Node 脚本语义对齐的小工具。
//!
//! 这里集中放「JS 行为模拟」：Number()/String()/truthy、Date.toISOString、
//! encodeURIComponent、JSON 读写、日志解码等，避免各模块各写一套导致行为漂移。

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 等价 `new Date().toISOString()`。
pub fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn iso_from_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 等价 `new Date(s).getTime()`，解析失败返回 None。
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

/// 等价 JS 的 String(v)：null/缺失视为空串。
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                n.as_f64().map(|f| format!("{f}")).unwrap_or_default()
            }
        }
        other => other.to_string(),
    }
}

/// 等价 JS 的 truthiness（对象/数组恒为真）。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// 等价 JS 的 Number()：数字直接取，字符串去空白后解析，布尔转 0/1。
pub fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok()
            }
        }
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}

/// 等价 parseInt()（只取开头整数部分）。
pub fn js_parse_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
        Value::String(s) => {
            let t = s.trim_start();
            let mut end = 0;
            let bytes = t.as_bytes();
            if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
                end += 1;
            }
            let digits_start = end;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == digits_start {
                None
            } else {
                t[..end].parse::<i64>().ok()
            }
        }
        _ => None,
    }
}

/// clampNum(v, min, max, dflt)：非数字回落默认值，四舍五入并夹紧。
pub fn clamp_num(v: Option<&Value>, min: i64, max: i64, dflt: i64) -> i64 {
    let Some(v) = v else { return dflt };
    let Some(n) = js_number(v) else { return dflt };
    if !n.is_finite() {
        return dflt;
    }
    (n.round() as i64).clamp(min, max)
}

/// `v || dflt` 后转字符串。
pub fn js_or_string(v: &Value, dflt: &str) -> String {
    if truthy(v) {
        js_string(v)
    } else {
        dflt.to_string()
    }
}

pub fn as_array(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

pub fn string_array(v: &Value) -> Vec<String> {
    as_array(v)
        .iter()
        .map(|x| js_string(x).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn read_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(text.trim_start_matches('\u{feff}').to_string())
}

pub fn read_json_or(path: &Path, dflt: Value) -> Value {
    match read_text(path).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(v) => v,
        None => dflt,
    }
}

pub fn write_json(path: &Path, v: &Value, trailing_newline: bool) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut s = serde_json::to_string_pretty(v)?;
    if trailing_newline {
        s.push('\n');
    }
    let mut f = fs::File::create(path)?;
    f.write_all(s.as_bytes())?;
    Ok(())
}

pub fn write_text(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text.as_bytes())?;
    Ok(())
}

/// 进程是否存活（等价 process.kill(pid, 0)）。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .creation_flags(0x0800_0000)
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        unsafe { libc_kill(pid) }
    }
}

#[cfg(not(windows))]
fn libc_kill(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// encodeURIComponent 等价实现。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// decodeURIComponent 等价实现（非法百分号原样保留）。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn base36(mut n: u64) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(CHARS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// 读取文件尾部若干行（最多 max_bytes 字节）。
pub fn tail_lines(path: &Path, max_bytes: u64, n: usize) -> Vec<String> {
    let Ok(meta) = fs::metadata(path) else {
        return Vec::new();
    };
    let len = meta.len().min(max_bytes);
    let Ok(mut f) = fs::File::open(path) else {
        return Vec::new();
    };
    use std::io::{Read, Seek, SeekFrom};
    if f.seek(SeekFrom::Start(meta.len() - len)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(len as usize);
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// 旧脚本同时尝试 UTF-16LE / UTF-8，按关键特征打分取高者。
pub fn decode_log_bytes(bytes: &[u8]) -> String {
    let u8s = String::from_utf8_lossy(bytes).into_owned();
    let u16s = decode_utf16le(bytes);
    let score = |s: &str| -> i32 {
        (if s.contains("http") { 2 } else { 0 })
            + (if s.contains("Bot ready") { 2 } else { 0 })
            + (if s.contains("二维码") { 1 } else { 0 })
    };
    if score(&u16s) >= score(&u8s) {
        u16s
    } else {
        u8s
    }
}

const BIND_PREFIX: &str = "https://q.qq.com/qqbot/openclaw/connect.html?";

/// 提取日志里最后一个扫码绑定链接。
pub fn last_bind_url(text: &str) -> Option<String> {
    let mut last = None;
    let mut start = 0usize;
    while let Some(pos) = text[start..].find(BIND_PREFIX) {
        let idx = start + pos;
        let tail = &text[idx + BIND_PREFIX.len()..];
        let end = tail
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || matches!(c, '\u{1b}' | '"' | '\'' | '<' | '>'))
            .map(|(j, _)| j)
            .unwrap_or(tail.len());
        last = Some(format!("{BIND_PREFIX}{}", &tail[..end]));
        start = idx + BIND_PREFIX.len() + end.max(1);
        if start >= text.len() {
            break;
        }
    }
    last
}

/// 读取文件 mtime（毫秒时间戳）。
pub fn mtime_ms(path: &Path) -> Option<i64> {
    let t = fs::metadata(path).ok()?.modified().ok()?;
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// 递归找 `bin/ffmpeg.exe`（最多 4 层），返回 bin 目录。
pub fn find_ffmpeg_bin(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let meta = fs::metadata(dir).ok()?;
    if !meta.is_dir() {
        return None;
    }
    if dir.join("bin").join("ffmpeg.exe").is_file() {
        return Some(dir.join("bin"));
    }
    let entries = fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        if let Some(found) = find_ffmpeg_bin(&e.path(), depth + 1) {
            return Some(found);
        }
    }
    None
}
