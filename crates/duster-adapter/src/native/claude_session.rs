//! Claude Code 会话 jsonl 解析(`~/.claude/projects/<路径编码>/<uuid>.jsonl`)。
//!
//! 实测 type 词汇表(2026-08 采样 3 个真实会话文件,version 2.1.220):
//!   `user` / `assistant` / `system` / `attachment` / `last-prompt` / `mode` /
//!   `ai-title` / `custom-title` / `queue-operation` / `file-history-delta` /
//!   `file-history-snapshot`
//!
//! 白名单取舍:
//! - 只有 `user` / `assistant` 且带 `message.content` 的记录才可能成为轮次;
//!   其余全是 UI/历史快照类控制记录,直接跳过。
//! - `message.content` 为字符串时直取;为数组时只拼接 `type == "text"` 段
//!   (`thinking` / `tool_use` / `tool_result` 段不算对话正文)。
//!   拼出来为空的记录(纯工具调用/纯工具结果/纯思考)不是真实对话轮次,跳过。
//! - `ai-title` / `custom-title` 顺手喂给 `SessionMeta::title`,用户手改的
//!   `custom-title` 优先于 `ai-title`。
//! - `cwd`:取首条带 `cwd` 字符串字段的记录(实测 user/assistant 每行都带)。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use duster_model::{Role, SessionMeta, TurnRecord};
use serde_json::Value;

/// 流式解析一个 Claude 会话文件。坏行(非 JSON / 结构不符)跳过不中断。
///
/// `byte_off` / `byte_len` 是该轮次**整行 JSON** 在文件中的区间
/// (不含行尾 `\n`/`\r\n`),用偏移回读该区间可重新解析出同一行。
pub fn parse(path: &Path) -> anyhow::Result<(SessionMeta, Vec<TurnRecord>)> {
    let file = File::open(path)
        .with_context(|| format!("打开 Claude 会话文件失败: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;

    let mut buf: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        if n == 0 {
            break;
        }
        let line_off = offset;
        offset += n as u64;

        // 行尾去掉 \n / \r\n;byte_len 不含换行。
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

        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
        {
            cwd = Some(c.to_string());
        }

        match v.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    ai_title = Some(t.to_string());
                }
                continue;
            }
            Some("custom-title") => {
                if let Some(t) = v.get("customTitle").and_then(Value::as_str) {
                    custom_title = Some(t.to_string());
                }
                continue;
            }
            Some(t @ ("user" | "assistant")) => {
                let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
                    continue;
                };
                let text = extract_text(content);
                if text.trim().is_empty() {
                    continue; // 纯工具/纯思考记录不是对话轮次
                }
                let role = if t == "user" {
                    Role::User
                } else {
                    Role::Assistant
                };
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
            _ => {} // 其余控制记录跳过
        }
    }

    let meta = SessionMeta {
        cwd,
        title: custom_title.or(ai_title),
        turn_count: turns.len() as u32,
    };
    Ok((meta, turns))
}

/// 从 `message.content` 抽取可检索正文:字符串直取,数组只拼 `type=="text"` 段。
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut parts: Vec<&str> = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = item.get("text").and_then(Value::as_str)
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
/// 只认 Z 结尾的 ISO-8601;解析不动就 None,不报错。
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
    // 小数秒截到毫秒,不足三位右补零。
    let mut ms = 0i64;
    for i in 0..3 {
        let digit = frac.as_bytes().get(i).copied();
        match digit {
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
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sessions/claude-basic.jsonl")
    }

    #[test]
    fn claude_session_meta_and_turns() {
        let (meta, turns) = parse(&fixture()).unwrap();

        assert_eq!(meta.cwd.as_deref(), Some("/Users/tester/projects/demo"));
        // custom-title 优先于 ai-title
        assert_eq!(meta.title.as_deref(), Some("演示会话"));
        assert_eq!(meta.turn_count, 4);
        assert_eq!(turns.len(), 4);

        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::User, Role::Assistant]
        );

        assert_eq!(turns[0].text, "帮我看看这个报错");
        assert_eq!(turns[1].text, "先看日志,再定位配置。");
        assert_eq!(turns[2].text, "Lorem ipsum dolor sit amet");
        assert_eq!(turns[3].text, "收到,已修复。");

        // 2026-08-01T10:00:00.000Z
        assert_eq!(turns[0].ts_ms, Some(1_785_578_400_000));
        assert_eq!(turns[1].ts_ms, Some(1_785_578_400_500));

        // seq 连续从 0 起
        for (i, t) in turns.iter().enumerate() {
            assert_eq!(t.seq, i as u32);
        }
    }

    #[test]
    fn claude_session_byte_offsets_roundtrip() {
        let path = fixture();
        let bytes = std::fs::read(&path).unwrap();
        let (_, turns) = parse(&path).unwrap();
        assert!(!turns.is_empty());
        // 首行就是首个轮次
        assert_eq!(turns[0].byte_off, 0);

        for turn in &turns {
            let start = turn.byte_off as usize;
            let end = start + turn.byte_len as usize;
            let slice = &bytes[start..end];
            // 区间恰好是一整行:不含换行,且紧随其后是 \n(或文件结束)
            assert!(!slice.contains(&b'\n'));
            assert!(end == bytes.len() || bytes[end] == b'\n' || bytes[end] == b'\r');
            assert!(start == 0 || bytes[start - 1] == b'\n');
            // 回读重切能解析出同一轮次的正文
            let v: Value = serde_json::from_slice(slice).unwrap();
            let text = extract_text(v.get("message").and_then(|m| m.get("content")).unwrap());
            assert_eq!(text, turn.text);
        }
    }

    #[test]
    fn claude_session_ts_parse() {
        assert_eq!(iso8601_to_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            iso8601_to_ms("2026-08-01T10:00:00Z"),
            Some(1_785_578_400_000)
        );
        assert_eq!(
            iso8601_to_ms("2026-08-01T10:00:00.5Z"),
            Some(1_785_578_400_500)
        );
        assert_eq!(iso8601_to_ms("垃圾"), None);
    }
}
