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
//!
//! 技能调用事件(prune 判定 skill 陈旧的口径,见 `duster_core::plan`):
//! 证据来自 `message.content[]` 中 `type == "tool_use"` 且 `name == "Skill"`
//! 的块的 `input.skill` 字段。刻意忽略同一数组里的 `text` / `thinking` /
//! `tool_result` 段——正文里提到技能名只说明被引用,不说明被调用。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use duster_model::{Role, SessionMeta, SkillInvocation, TurnBody, TurnRecord};
use serde_json::Value;

use super::jsonl_common::iso8601_to_ms;

/// 流式解析一个 Claude 会话文件。坏行(非 JSON / 结构不符)跳过不中断。
///
/// `byte_off` / `byte_len` 是该轮次**整行 JSON** 在文件中的区间
/// (不含行尾 `\n`/`\r\n`),用偏移回读该区间可重新解析出同一行。
///
/// 第三个返回值是技能调用事件(见模块文档):证据行随手提取,不二次遍历。
pub fn parse(path: &Path) -> anyhow::Result<(SessionMeta, Vec<TurnRecord>, Vec<SkillInvocation>)> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Claude session file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut events: Vec<SkillInvocation> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    // 事件时间戳兜底:会话内最后已知时间戳。绝不发明 `now`——一个编出来的
    // 新日期会把死技能永远救活(见 `duster_model::SkillInvocation`)。
    let mut last_ts_ms: Option<i64> = None;

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

        let ts_ms = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(iso8601_to_ms);
        if ts_ms.is_some() {
            last_ts_ms = ts_ms;
        }
        for name in skill_names_of_line(std::str::from_utf8(line).unwrap_or_default()) {
            if let Some(t) = ts_ms.or(last_ts_ms) {
                events.push(SkillInvocation {
                    skill: name,
                    ts_ms: t,
                });
            }
        }

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
                // 行级抽取规则只在 body_of_line 里有一份;这里再写一遍就等于
                // 允许两处慢慢分叉,而分叉的那天就是展示层重新漏正文的那天。
                let Some(body) = body_of_line(std::str::from_utf8(line).unwrap_or_default()) else {
                    continue;
                };
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
                    text: body.text,
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
    Ok((meta, turns, events))
}

/// 单行 → 可读正文。None = 这一行不是对话轮次(控制记录 / 纯思考 / 纯工具调用)。
///
/// 与 [`parse`] 共用同一套白名单:只有 `type ∈ {user, assistant}` 且
/// `message.content` 抽得出非空文本的记录才算轮次,其余(`system` /
/// `ai-title` / `file-history-*` 等)一律 None。`tool` 恒为 None——Claude 的
/// `tool_result` 块不带工具名(名字在配对的 `tool_use` 块里,而那一块不是
/// 对话正文),编一个名字出来是撒谎。
pub fn body_of_line(raw: &str) -> Option<TurnBody> {
    let v: Value = serde_json::from_str(raw).ok()?;
    match v.get("type").and_then(Value::as_str) {
        Some("user" | "assistant") => {
            let content = v.get("message").and_then(|m| m.get("content"))?;
            let text = extract_text(content);
            if text.trim().is_empty() {
                return None; // 纯工具/纯思考记录不是对话轮次
            }
            Some(TurnBody {
                text,
                tool: None,
                raw_fallback: false,
            })
        }
        _ => None, // 控制记录
    }
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

/// 从一行里提取技能调用事件名。
///
/// 证据字段:`message.content[]` 中 `type == "tool_use"` 且 `name == "Skill"`
/// 的块的 `input.skill`。刻意忽略:同一数组里的 `text` / `thinking` /
/// `tool_result` 段、以及任何非 `Skill` 工具——正文提到技能名只说明被引用,
/// 不说明被调用;旧实现的名字全文检索就是死在引用与调用的混淆上。
fn skill_names_of_line(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(items) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("tool_use")
            && item.get("name").and_then(Value::as_str) == Some("Skill")
            && let Some(s) = item
                .get("input")
                .and_then(|i| i.get("skill"))
                .and_then(Value::as_str)
        {
            names.push(s.to_string());
        }
    }
    names
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
        let (meta, turns, _events) = parse(&fixture()).unwrap();

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
        let (_, turns, _events) = parse(&path).unwrap();
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

    /// 真实形状的轮次行抽得出散文(没有 `{` / `"role"` 泄漏),控制记录是 None。
    #[test]
    fn claude_body_of_line_轮次行出散文_控制记录不出轮次() {
        let user_line = r#"{"type":"user","message":{"role":"user","content":"帮我看看 docker 报错"},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        let body = body_of_line(user_line).unwrap();
        assert_eq!(body.text, "帮我看看 docker 报错");
        assert!(!body.text.contains('{'));
        assert!(!body.text.contains("\"role\""));
        assert_eq!(body.tool, None);
        assert!(!body.raw_fallback);

        // 数组 content 只拼 text 段,thinking / tool_use / tool_result 不算正文。
        let assistant_line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"lorem"},{"type":"text","text":"先看日志,再定位配置。"}]}}"#;
        let body = body_of_line(assistant_line).unwrap();
        assert_eq!(body.text, "先看日志,再定位配置。");

        // 控制记录 / 纯工具轮 / 坏行一律 None。
        assert!(body_of_line(r#"{"type":"ai-title","aiTitle":"标题"}"#).is_none());
        assert!(body_of_line(r#"{"type":"system","subtype":"info","content":"x"}"#).is_none());
        assert!(body_of_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#
        )
        .is_none());
        assert!(body_of_line("this is not json").is_none());
    }

    /// parse() 与 body_of_line 必须出自同一套规则:对夹具里每个轮次行,
    /// body_of_line 抽出的正文与 parse 落库的 text 逐字节相同。
    #[test]
    fn claude_parse_与_body_of_line_规则一致() {
        let path = fixture();
        let bytes = std::fs::read(&path).unwrap();
        let (_, turns, _events) = parse(&path).unwrap();
        assert!(!turns.is_empty());
        let body = String::from_utf8(bytes).unwrap();
        for turn in &turns {
            let line = &body[turn.byte_off as usize..(turn.byte_off + turn.byte_len) as usize];
            let b = body_of_line(line).unwrap();
            assert_eq!(b.text, turn.text);
        }
    }

    /// 真实形状的 Skill 工具调用行产生事件;正文提及、非 Skill 工具都不算。
    #[test]
    fn claude_skill_工具调用行出事件_正文提及不算() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我来处理。"},{"type":"tool_use","id":"toolu_01","name":"Skill","input":{"skill":"pdf"}}]},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        assert_eq!(skill_names_of_line(line), vec!["pdf"]);

        // 正文里提到技能名只是被引用:tool_use 块才是证据,text 段刻意忽略。
        let prose = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"帮我看一下 pdf 技能怎么用"}]},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        assert!(skill_names_of_line(prose).is_empty());
        // 别的工具碰巧带 input.skill 字段也不算调用。
        let other = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"WebSearch","input":{"skill":"pdf"}}]},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        assert!(skill_names_of_line(other).is_empty());
    }

    /// parse() 顺手产出事件:名字与时间戳都来自行本身,不二次遍历。
    #[test]
    fn claude_parse_产出调用事件() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Skill\",\"input\":{\"skill\":\"xlsx\"}}]},\"timestamp\":\"2026-08-01T10:00:00.000Z\"}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "xlsx");
        assert_eq!(events[0].ts_ms, 1_785_578_400_000);
    }

    /// 调用行没有时间戳时退回会话内最后已知时间戳,绝不发明 `now`——
    /// 一个编出来的新日期会把死技能永远救活。
    #[test]
    fn claude_调用行无时间戳_退回会话最后已知时间戳() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"先聊聊\"}]},\"timestamp\":\"2026-08-01T10:00:00.000Z\"}\n");
        body.push_str("{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Skill\",\"input\":{\"skill\":\"docx\"}}]}}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "docx");
        assert_eq!(events[0].ts_ms, 1_785_578_400_000);
    }
}
