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
//!
//! 技能调用事件(prune 判定 skill 陈旧的口径,见 `duster_core::plan`):
//! 证据来自 `response_item` 的 `payload.type ∈ {custom_tool_call, function_call}`
//! 的参数文本(`payload.input` / `payload.arguments`)里出现的
//! `/skills/<名>/SKILL.md` 路径。刻意忽略:**任何** role 的 message 正文——
//! developer/system 注入的指令块在**每个**会话里都逐条列出全部技能的
//! `SKILL.md` 路径(实测 528 处/55 名),把整行当证据就是旧名字检索 bug
//! 换了个马甲;`session_meta.base_instructions` / `world_state` / `compacted`
//! 同理是目录不是调用。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use duster_model::{Role, SessionMeta, SkillInvocation, TurnBody, TurnRecord};
use serde_json::Value;

use super::jsonl_common::iso8601_to_ms;

/// 流式解析一个 Codex rollout 文件。坏行跳过不中断。
///
/// `byte_off` / `byte_len` 是该轮次**整行 JSON** 在文件中的区间
/// (不含行尾 `\n`/`\r\n`),用偏移回读该区间可重新解析出同一行。
///
/// 第三个返回值是技能调用事件(见模块文档):证据行随手提取,不二次遍历。
pub fn parse(path: &Path) -> anyhow::Result<(SessionMeta, Vec<TurnRecord>, Vec<SkillInvocation>)> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Codex session file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut events: Vec<SkillInvocation> = Vec::new();
    let mut cwd: Option<String> = None;
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

        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if cwd.is_none()
                    && let Some(c) = v.pointer("/payload/cwd").and_then(Value::as_str)
                {
                    cwd = Some(c.to_string());
                }
            }
            Some("response_item") => {
                // 行级抽取规则只在 body_of_line 里有一份;这里再写一遍就等于
                // 允许两处慢慢分叉,而分叉的那天就是展示层重新漏正文的那天。
                let Some(body) = body_of_line(std::str::from_utf8(line).unwrap_or_default()) else {
                    continue;
                };
                let role = match v.pointer("/payload/role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    // 冗余防御:body_of_line 已挡掉 developer;真走到这里说明
                    // 规则被改过,宁可漏一条也不能把一个不是人说的角色落库。
                    _ => continue,
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
            _ => {} // turn_context / event_msg / compacted 等控制记录跳过
        }
    }

    let meta = SessionMeta {
        cwd,
        title: None,
        turn_count: turns.len() as u32,
    };
    Ok((meta, turns, events))
}

/// 单行 → 可读正文。None = 这一行不是对话轮次(控制记录 / 注入指令 / 工具流水)。
///
/// 与 [`parse`] 共用同一套白名单:`type == "response_item"` 且
/// `payload.type == "message"` 且 `payload.role ∈ {user, assistant}` 且正文
/// 非空、不是 CLI 注入的控制载荷,才算轮次。`tool` 恒为 None——Codex 的
/// `function_call` 行不带可读正文,工具名无处安放,编一个出来是撒谎。
pub fn body_of_line(raw: &str) -> Option<TurnBody> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = v.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None; // reasoning / function_call / *_call_output 等工具与事件流水
    }
    let role = payload.get("role").and_then(Value::as_str)?;
    if role != "user" && role != "assistant" {
        return None; // developer 是注入的系统指令,排除
    }
    let content = payload.get("content")?;
    let text = extract_text(content);
    let trimmed = text.trim();
    if trimmed.is_empty() || is_injected_control(trimmed) {
        return None;
    }
    Some(TurnBody {
        text,
        tool: None,
        raw_fallback: false,
    })
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

/// 从一行里提取技能调用事件名。
///
/// 证据字段:`response_item` 的 `payload.type ∈ {custom_tool_call, function_call}`
/// 的参数文本(`payload.input` / `payload.arguments`)里的 `/skills/<名>/SKILL.md`
/// 路径。刻意忽略:任何 role 的 message 正文与一切非工具流水——developer 的
/// 指令块在**每个**会话都逐条列出全部技能的 `SKILL.md` 路径,把整行当证据
/// 就是旧名字检索 bug 换了个马甲(实测 528 处/55 名,全是指令目录)。
fn skill_names_of_line(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    if v.get("type").and_then(Value::as_str) != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = v.get("payload") else {
        return Vec::new();
    };
    let kind = payload.get("type").and_then(Value::as_str);
    if kind != Some("custom_tool_call") && kind != Some("function_call") {
        return Vec::new();
    }
    let mut names = Vec::new();
    for field in ["input", "arguments"] {
        if let Some(s) = payload.get(field).and_then(Value::as_str) {
            extract_skill_md_names(s, &mut names);
        }
    }
    names
}

/// 从参数文本里提取 `/skills/<名>/SKILL.md` 路径中的技能名。
///
/// 名称为 `/skills/` 后到下一个 `/` 之间的段,且**必须**紧跟 `/SKILL.md`:
/// `~/.codex/skills/.system/imagegen/SKILL.md` 这种内嵌路径中间隔了层目录,
/// 取出来的是目录而不是技能,不算。名字本身可以含连字符/点/`@`
/// (`control-in-app-browser` 就是真名)。
fn extract_skill_md_names(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let needle = b"/skills/";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let start = i + needle.len();
            let mut end = start;
            while end < bytes.len()
                && bytes[end] != b'/'
                && bytes[end] != b'"'
                && bytes[end] != b'\''
                && bytes[end] != b' '
            {
                end += 1;
            }
            if end > start && bytes[end..].starts_with(b"/SKILL.md") {
                out.push(text[start..end].to_string());
            }
            i = end.max(start);
        } else {
            i += 1;
        }
    }
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
        let (meta, turns, _events) = parse(&fixture()).unwrap();

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
        let (_, turns, _events) = parse(&path).unwrap();
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

    /// 真实形状的轮次行抽得出散文,控制记录 / 注入指令 / 工具流水是 None。
    #[test]
    fn codex_body_of_line_轮次行出散文_控制记录不出轮次() {
        let user_line = r#"{"timestamp":"2026-08-01T12:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"请修复 docker 构建脚本"}]}}"#;
        let body = body_of_line(user_line).unwrap();
        assert_eq!(body.text, "请修复 docker 构建脚本");
        assert!(!body.text.contains('{'));
        assert!(!body.text.contains("\"role\""));
        assert_eq!(body.tool, None);
        assert!(!body.raw_fallback);

        // assistant 段拼 output_text,reasoning / function_call 不算正文。
        let assistant_line = r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"已修复,lorem ipsum。"}]}}"#;
        assert_eq!(
            body_of_line(assistant_line).unwrap().text,
            "已修复,lorem ipsum。"
        );

        // 控制记录 / developer / 注入载荷 / 工具流水 / 坏行一律 None。
        assert!(body_of_line(
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"lorem"}]}}"#
        )
        .is_none());
        assert!(body_of_line(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_instructions>lorem</user_instructions>"}]}}"#
        )
        .is_none());
        assert!(body_of_line(r#"{"type":"event_msg","payload":{"type":"token_count"}}"#).is_none());
        assert!(
            body_of_line(
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell"}}"#
            )
            .is_none()
        );
        assert!(body_of_line("this is not json").is_none());
    }

    /// parse() 与 body_of_line 必须出自同一套规则:对夹具里每个轮次行,
    /// body_of_line 抽出的正文与 parse 落库的 text 逐字节相同。
    #[test]
    fn codex_parse_与_body_of_line_规则一致() {
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

    /// 真实形状的工具调用行出事件;developer 指令目录行是核心反例,一个都不许出。
    #[test]
    fn codex_skill_工具调用出事件_developer目录行不算() {
        // 真实形状:exec_command 的 cmd 参数里带着技能路径。
        let call = r#"{"type":"response_item","timestamp":"2026-07-20T06:59:50.831Z","payload":{"type":"custom_tool_call","id":"c1","name":"shell","input":"const r = await tools.exec_command({cmd:\"sed -n '1,240p' '/Users/laibu/.codex/skills/pdf/SKILL.md'\"});"}}"#;
        assert_eq!(skill_names_of_line(call), vec!["pdf"]);

        // function_call 形状:arguments 是 JSON 字符串,同样认。
        let fn_call = r#"{"type":"response_item","timestamp":"2026-07-20T06:59:50.831Z","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"sed -n '1,240p' '/Users/laibu/.codex/skills/webapp-testing/SKILL.md'\"}"}}"#;
        assert_eq!(skill_names_of_line(fn_call), vec!["webapp-testing"]);

        // 反例(本任务存在的理由):developer role 的 message 正文逐条列出
        // 全部技能的 SKILL.md 路径——这是指令目录不是调用,一个都不许出。
        let dev = r#"{"type":"response_item","timestamp":"2026-07-20T06:30:00.000Z","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"可用技能:\n- ~/.claude/skills/pdf/SKILL.md\n- ~/.claude/skills/docx/SKILL.md\n- ~/.claude/skills/frontend/SKILL.md"}]}}"#;
        assert!(skill_names_of_line(dev).is_empty());

        // session_meta 的 base_instructions 也是指令目录,不算。
        let meta = r#"{"type":"session_meta","timestamp":"2026-07-20T06:30:59.286Z","payload":{"base_instructions":"skills: /Users/laibu/.codex/skills/pdf/SKILL.md"}}"#;
        assert!(skill_names_of_line(meta).is_empty());

        // 嵌套路径(.system/imagegen)中间隔了层目录,取出来不是技能名,不算。
        let nested = r#"{"type":"response_item","timestamp":"2026-07-20T06:59:50.831Z","payload":{"type":"custom_tool_call","id":"c1","input":"sed -n '1,240p' '/Users/laibu/.codex/skills/.system/imagegen/SKILL.md'"}}"#;
        assert!(skill_names_of_line(nested).is_empty());
    }

    /// parse() 顺手产出事件:名字与时间戳都来自行本身。
    #[test]
    fn codex_parse_产出调用事件() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"response_item\",\"timestamp\":\"2026-07-20T06:59:50.831Z\",\"payload\":{\"type\":\"custom_tool_call\",\"id\":\"c1\",\"name\":\"shell\",\"input\":\"sed -n '1,240p' '/Users/laibu/.codex/skills/pdf/SKILL.md'\"}}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "pdf");
        assert_eq!(
            events[0].ts_ms,
            iso8601_to_ms("2026-07-20T06:59:50.831Z").unwrap()
        );
    }

    /// 调用行没有时间戳时退回会话内最后已知时间戳,绝不发明 `now`。
    #[test]
    fn codex_调用行无时间戳_退回会话最后已知时间戳() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"response_item\",\"timestamp\":\"2026-07-20T06:59:50.000Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"请修复构建脚本\"}]}}\n");
        body.push_str("{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"id\":\"c1\",\"name\":\"shell\",\"input\":\"sed -n '1,240p' '/Users/laibu/.codex/skills/pdf/SKILL.md'\"}}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "pdf");
        assert_eq!(
            events[0].ts_ms,
            iso8601_to_ms("2026-07-20T06:59:50.000Z").unwrap()
        );
    }
}
