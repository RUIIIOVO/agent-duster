//! Codex CLI 会话 jsonl 解析(`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`)。
//!
//! 实测词汇表(2026-08 采样 3 个真实 rollout 文件,cli_version 0.146.0):
//! - 顶层 `type`:`session_meta` / `turn_context` / `response_item` /
//!   `event_msg` / `compacted` / `world_state` / `inter_agent_communication_metadata`
//! - `response_item.payload.type`:`message` / `reasoning` / `function_call` /
//!   `function_call_output` / `custom_tool_call` / `custom_tool_call_output` /
//!   `agent_message`
//! - `event_msg.payload.type`:`token_count` / `user_message` / `agent_message` /
//!   `task_started` / `task_complete`
//!
//! 白名单取舍:
//! - 只有 `type == "response_item"` 且 `payload.type == "message"` 且
//!   `payload.role ∈ {user, assistant}` 的记录才成为轮次。
//!   `developer` role 是注入的系统指令,排除;`reasoning` / `*_call` /
//!   `*_call_output` / `agent_message` 是工具与事件流水,排除;
//!   `event_msg` 与 response_item 内容重复(UI 事件回显),整类排除。
//! - user message 中正文形如 `<user_instructions>…` / `<environment_context>…`
//!   的是 CLI 每轮注入的控制载荷,不是人说的话,同样排除。
//! - `payload.content` 为字符串直取;数组拼 `input_text` / `output_text` /
//!   `text` 段(user 段是 input_text,assistant 段是 output_text)。
//! - `cwd` 取 `session_meta.payload.cwd`;`title` 上游没有,恒 None。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use duster_model::{Role, SessionMeta, TurnRecord};
use serde_json::Value;

/// 流式解析一个 Codex rollout 文件。坏行跳过不中断。
///
/// `byte_off` / `byte_len` 是该轮次**整行 JSON** 在文件中的区间
/// (不含行尾 `\n`/`\r\n`),用偏移回读该区间可重新解析出同一行。
pub fn parse(path: &Path) -> anyhow::Result<(SessionMeta, Vec<TurnRecord>)> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Codex session file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut cwd: Option<String> = None;

    let mut buf: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        let line_off = offset;
        offset += n as u64;

        let mut end = buf.len();
        while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
            end -= 1;
        }
        let line = &buf[..end];
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue; // 坏行跳过
        };

        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if cwd.is_none()
                    && let Some(c) = v.pointer("/payload/cwd").and_then(Value::as_str)
                {
                    cwd = Some(c.to_string());
                }
            }
            Some("response_item") => {
                let Some(payload) = v.get("payload") else {
                    continue;
                };
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                let role = match payload.get("role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue, // developer 等注入指令排除
                };
                let Some(content) = payload.get("content") else {
                    continue;
                };
                let text = extract_text(content);
                let trimmed = text.trim();
                if trimmed.is_empty() || is_injected_control(trimmed) {
                    continue;
                }
                turns.push(TurnRecord {
                    seq: turns.len() as u32,
                    role,
                    ts_ms: v
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(iso8601_to_ms),
                    byte_off: line_off,
                    byte_len: line.len() as u64,
                    text,
                });
            }
            _ => {} // turn_context / event_msg / compacted 等控制记录跳过
        }
    }

    let meta = SessionMeta {
        cwd,
        title: None,
        turn_count: turns.len() as u32,
    };
    Ok((meta, turns))
}

/// CLI 每轮以 user role 注入的控制载荷,不算对话轮次。
fn is_injected_control(text: &str) -> bool {
    text.starts_with("<user_instructions>") || text.starts_with("<environment_context>")
}

/// 从 `payload.content` 抽取正文:字符串直取,数组拼文本段。
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut parts: Vec<&str> = Vec::new();
            for item in items {
                if matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                ) && let Some(t) = item.get("text").and_then(Value::as_str)
                {
                    parts.push(t);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// 解析 `2026-08-07T07:04:38.708Z` 形式的 UTC 时间戳为 Unix 毫秒。
fn iso8601_to_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    if d.next().is_some() {
        return None;
    }
    let (hms, frac) = match time.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time, ""),
    };
    let mut t = hms.split(':');
    let (h, mi, sec) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if t.next().is_some() {
        return None;
    }
    let mut ms = 0i64;
    for i in 0..3 {
        match frac.as_bytes().get(i).copied() {
            Some(b @ b'0'..=b'9') => ms = ms * 10 + i64::from(b - b'0'),
            Some(_) => return None,
            None => ms *= 10,
        }
    }
    let days = days_from_civil(y, mo, day);
    Some((((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}

/// 公历日期 -> 距 1970-01-01 的天数(Howard Hinnant 的 days_from_civil)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sessions/codex-basic.jsonl")
    }

    #[test]
    fn codex_session_meta_and_turns() {
        let (meta, turns) = parse(&fixture()).unwrap();

        assert_eq!(meta.cwd.as_deref(), Some("/Users/tester/projects/demo"));
        assert_eq!(meta.title, None);
        assert_eq!(meta.turn_count, 2);
        assert_eq!(turns.len(), 2);

        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant]);

        assert_eq!(turns[0].text, "请修复构建脚本");
        assert_eq!(turns[1].text, "已修复,lorem ipsum。");

        // 2026-08-01T12:00:01.000Z / 2026-08-01T12:00:05.250Z
        assert_eq!(turns[0].ts_ms, Some(1_785_585_601_000));
        assert_eq!(turns[1].ts_ms, Some(1_785_585_605_250));

        for (i, t) in turns.iter().enumerate() {
            assert_eq!(t.seq, i as u32);
        }
    }

    #[test]
    fn codex_session_byte_offsets_roundtrip() {
        let path = fixture();
        let bytes = std::fs::read(&path).unwrap();
        let (_, turns) = parse(&path).unwrap();
        assert!(!turns.is_empty());
        // 首行是 session_meta,首个轮次一定不在偏移 0
        assert!(turns[0].byte_off > 0);

        for turn in &turns {
            let start = turn.byte_off as usize;
            let end = start + turn.byte_len as usize;
            let slice = &bytes[start..end];
            assert!(!slice.contains(&b'\n'));
            assert!(end == bytes.len() || bytes[end] == b'\n' || bytes[end] == b'\r');
            assert!(start == 0 || bytes[start - 1] == b'\n');
            // 回读重切能解析出同一轮次的正文
            let v: Value = serde_json::from_slice(slice).unwrap();
            let text = extract_text(v.pointer("/payload/content").unwrap());
            assert_eq!(text, turn.text);
        }
    }
}
