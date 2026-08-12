//! omp 会话 jsonl 解析(`~/.omp/agent/sessions/<路径编码>/<时间戳>_<uuid>.jsonl`)。
//!
//! 实测 type 词汇表(2026-08-11,251 个真实文件 / 192 MB,omp 17.x):
//!   `message` / `custom` / `model_change` / `title` / `session` /
//!   `custom_message` / `credential_pin` / `session_init` /
//!   `thinking_level_change` / `title_change` / `service_tier_change` /
//!   `compaction` / `ttsr_injection`
//!
//! 白名单取舍:
//! - 只有 `type == "message"` 记录才可能成为轮次;其余全是控制记录
//!   (`session` 带 cwd、`title` / `title_change` 带标题、`custom` 是工具
//!   执行事件流、`custom_message` 是注入提醒、`session_init` 带 systemPrompt、
//!   `compaction` 带归档摘要……),直接跳过。
//! - `message.role`:`user` → [`Role::User`]、`assistant` → [`Role::Assistant`]、
//!   `toolResult` → [`Role::Tool`](工具输出,进索引不进 FTS);
//!   `developer` 是注入的系统指令,同 Codex 适配器的取舍,排除。
//! - `message.content` 为字符串时直取;为数组时只拼接 `type == "text"` 段
//!   (`thinking` 是思维链、`toolCall` 是工具调用回显、`image` 是图片,
//!   都不算对话正文)。拼出来为空的记录(纯思考/纯工具调用)不是对话轮次,跳过。
//! - `cwd`:取 `type == "session"` 记录的 `cwd`(message 记录实测不带)。
//! - `title`:初始 `title` 记录可能为空,`title_change` 后续更新
//!   (实测全为 `source == "auto"`);取最后一条非空标题。
//!
//! 技能调用事件(prune 判定 skill 陈旧的口径,见 `duster_core::plan`):
//! 证据来自**工具调用参数**:`type == "custom"`(实测 `tool_execution_start`)
//! 的 `data.args`,以及 `type == "message"` 的 `message.content[].toolCall.arguments`
//! ——两处的 JSON 值里带 `skill://<名>` 即一次调用。刻意忽略:
//! `session_init.systemPrompt`(逐条列出全部技能名的目录,是目录不是调用)、
//! `custom_message`/`skill-prompt`(注入的技能正文)、以及任何 role 的
//! 正文文本——`skill://<名>` 出现在散文里只说明被提及。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use duster_model::{Role, SessionMeta, SkillInvocation, TurnBody, TurnRecord};
use serde_json::Value;

use super::jsonl_common::iso8601_to_ms;

/// 流式解析一个 omp 会话文件。坏行(非 JSON / 结构不符)跳过不中断。
///
/// `byte_off` / `byte_len` 是该轮次**整行 JSON** 在文件中的区间
/// (不含行尾 `\n`/`\r\n`),用偏移回读该区间可重新解析出同一行。
///
/// 第三个返回值是技能调用事件(见模块文档):证据行随手提取,不二次遍历。
pub fn parse(path: &Path) -> anyhow::Result<(SessionMeta, Vec<TurnRecord>, Vec<SkillInvocation>)> {
    let file = File::open(path)
        .with_context(|| format!("failed to open omp session file: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut events: Vec<SkillInvocation> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
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

        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if cwd.is_none()
                    && let Some(c) = v.get("cwd").and_then(Value::as_str)
                {
                    cwd = Some(c.to_string());
                }
            }
            // 初始 `title` 记录可能为空;`title_change` 之后更新。都取非空值,
            // 靠「后写覆盖」自然让最新标题生效。
            Some("title" | "title_change") => {
                if let Some(t) = v
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    title = Some(t.to_string());
                }
            }
            Some("message") => {
                // 行级抽取规则只在 body_of_line 里有一份;这里再写一遍就等于
                // 允许两处慢慢分叉,而分叉的那天就是展示层重新漏正文的那天。
                let Some(body) = body_of_line(std::str::from_utf8(line).unwrap_or_default()) else {
                    continue;
                };
                let role = match v.pointer("/message/role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    Some("toolResult") => Role::Tool,
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
            _ => {} // 其余控制记录跳过
        }
    }

    let meta = SessionMeta {
        cwd,
        title,
        turn_count: turns.len() as u32,
    };
    Ok((meta, turns, events))
}

/// 单行 → 可读正文。None = 这一行不是对话轮次(控制记录 / 纯思考 / 纯工具调用)。
///
/// 与 [`parse`] 共用同一套白名单:`type == "message"` 且
/// `message.role ∈ {user, assistant, toolResult}` 且 `message.content` 抽得出
/// 非空文本,才算轮次。`tool` 取自行里真的带的名字(`message.toolName`,
/// 实测 toolResult 行有;user/assistant 行没有就是 None),不编。
pub fn body_of_line(raw: &str) -> Option<TurnBody> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("message") {
        return None; // session / title / custom / compaction 等控制记录
    }
    let m = v.get("message")?;
    let role = m.get("role").and_then(Value::as_str)?;
    if !matches!(role, "user" | "assistant" | "toolResult") {
        return None; // developer 是注入的系统指令,排除
    }
    let content = m.get("content")?;
    let text = extract_text(content);
    if text.trim().is_empty() {
        return None; // 纯思考/纯工具调用记录不是对话轮次
    }
    let tool = m
        .get("toolName")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(TurnBody {
        text,
        tool,
        raw_fallback: false,
    })
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
/// 证据字段(二选一,都只认**工具调用参数**):
/// - `type == "custom"` 的 `data.args`(实测 `tool_execution_start`,如
///   `{"path": "skill://brainstorming"}`);
/// - `type == "message"` 的 `message.content[].toolCall.arguments`(assistant
///   声明的工具调用参数,如 `{"path": "skill://plan-doc", "i": "..."}`)。
///
/// 刻意忽略:`session_init.systemPrompt`(逐条列出全部技能名的目录,是目录
/// 不是调用)、`custom_message`/`skill-prompt`(注入的技能正文)、以及任何
/// role 的 `text` / `thinking` / `toolResult`——`skill://<名>` 出现在散文里
/// 只说明被提及,不说明被调用。
fn skill_names_of_line(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let args: Option<&Value> = match v.get("type").and_then(Value::as_str) {
        Some("custom") => v.pointer("/data/args"),
        Some("message") => {
            let Some(items) = v.pointer("/message/content").and_then(Value::as_array) else {
                return Vec::new();
            };
            let mut found = None;
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("toolCall") {
                    found = item.get("arguments");
                    break;
                }
            }
            found
        }
        _ => None,
    };
    let Some(args) = args else {
        return Vec::new();
    };
    let text = match args {
        Value::String(s) => s.clone(),
        // 参数是对象(如 `{"path": "skill://brainstorming"}`):序列化后照搜。
        other => other.to_string(),
    };
    extract_skill_uri_names(&text)
}

/// 从工具参数文本里提取所有 `skill://<名>` 名字。
///
/// 名字只取标识符字符(字母/数字/连字符/下划线):`skill://plan-doc"` 停在
/// 引号,`skill://foo/` 停在斜杠(URI 后续路径)。`.` 刻意不收——实测正文里
/// 有 `skill://weekly-report.` 这种句子收尾,收了会把句点带进名字。
fn extract_skill_uri_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let needle = b"skill://";
    let mut names = Vec::new();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let start = i + needle.len();
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b'_')
            {
                end += 1;
            }
            if end > start {
                names.push(text[start..end].to_string());
            }
            i = end.max(start);
        } else {
            i += 1;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sessions/omp-basic.jsonl")
    }

    #[test]
    fn omp_jsonl_session_meta_and_turns() {
        let (meta, turns, _events) = parse(&fixture()).unwrap();

        // cwd 取自 session 记录;标题取最后一条非空 title_change。
        assert_eq!(meta.cwd.as_deref(), Some("/Users/tester/projects/demo"));
        assert_eq!(meta.title.as_deref(), Some("演示会话"));
        assert_eq!(meta.turn_count, 4);
        assert_eq!(turns.len(), 4);

        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant,]
        );

        assert_eq!(turns[0].text, "帮我看看这个报错");
        assert_eq!(turns[1].text, "先看日志,再定位配置。");
        assert_eq!(turns[2].text, "error: demo.rs:12 no such file");
        assert_eq!(turns[3].text, "收到,已修复。");

        // 2026-08-01T10:00:00.200Z
        assert_eq!(turns[0].ts_ms, Some(1_785_578_400_200));

        for (i, t) in turns.iter().enumerate() {
            assert_eq!(t.seq, i as u32);
        }
    }

    #[test]
    fn omp_jsonl_session_byte_offsets_roundtrip() {
        let path = fixture();
        let bytes = std::fs::read(&path).unwrap();
        let (_, turns, _events) = parse(&path).unwrap();
        assert!(!turns.is_empty());
        // 首行是 title 控制记录,首个轮次一定不在偏移 0
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
            let text = extract_text(v.pointer("/message/content").unwrap());
            assert_eq!(text, turn.text);
        }
    }

    #[test]
    fn omp_jsonl_session_坏行_跳过不中断() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"title\",\"v\":1,\"title\":\"\"}\n");
        body.push_str("{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"cwd\":\"/tmp\"}\n");
        body.push_str("this is not json\n"); // 坏行
        body.push_str("{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"第一轮\"}]}}\n");
        body.push_str("{\"type\":\"message\",\"id\":\"m2\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"只思考不出口\"},{\"type\":\"text\",\"text\":\"第二轮\"}]}}\n");
        std::fs::write(&path, body).unwrap();

        let (meta, turns, _events) = parse(&path).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/tmp"));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "第一轮");
        // 坏行在中间,后面的轮次一个不少
        assert_eq!(turns[1].text, "第二轮");
        assert_eq!(turns[1].role, Role::Assistant);
    }

    #[test]
    fn omp_jsonl_session_纯工具与注入_不计轮次() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"cwd\":\"/tmp\"}\n");
        // assistant 纯 thinking + toolCall,无 text -> 跳过
        body.push_str("{\"type\":\"message\",\"id\":\"a1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"想\"},{\"type\":\"toolCall\",\"name\":\"read\"}]}}\n");
        // developer 注入指令 -> 跳过
        body.push_str("{\"type\":\"message\",\"id\":\"d1\",\"message\":{\"role\":\"developer\",\"content\":[{\"type\":\"text\",\"text\":\"system reminder\"}]}}\n");
        // 字符串 content 直取
        body.push_str("{\"type\":\"message\",\"id\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"直取字符串\"}}\n");
        std::fs::write(&path, body).unwrap();

        let (_, turns, _events) = parse(&path).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "直取字符串");
        assert_eq!(turns[0].role, Role::User);
    }

    /// 真实形状的轮次行抽得出散文(没有 `{` / `"role"` 泄漏),控制记录是 None;
    /// toolResult 行带上行里真的有的 `message.toolName`。
    #[test]
    fn omp_body_of_line_轮次行出散文_控制记录不出轮次() {
        let user_line = r#"{"type":"message","id":"m1","timestamp":"2026-08-01T10:00:00.200Z","message":{"role":"user","content":[{"type":"text","text":"帮我看看 docker 报错"}]}}"#;
        let body = body_of_line(user_line).unwrap();
        assert_eq!(body.text, "帮我看看 docker 报错");
        assert!(!body.text.contains('{'));
        assert!(!body.text.contains("\"role\""));
        assert_eq!(body.tool, None);
        assert!(!body.raw_fallback);

        // toolResult 行:正文拼 text 段,工具名来自行里的 toolName。
        let tool_line = r#"{"type":"message","id":"m3","message":{"role":"toolResult","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"error: demo.rs:12 no such file"}]}}"#;
        let body = body_of_line(tool_line).unwrap();
        assert_eq!(body.text, "error: demo.rs:12 no such file");
        assert_eq!(body.tool.as_deref(), Some("read"));

        // 控制记录 / 纯思考+toolCall / developer / 坏行一律 None。
        assert!(body_of_line(r#"{"type":"title","v":1,"title":""}"#).is_none());
        assert!(body_of_line(
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"想"},{"type":"toolCall","name":"read"}]}}"#
        )
        .is_none());
        assert!(body_of_line(
            r#"{"type":"message","message":{"role":"developer","content":[{"type":"text","text":"reminder"}]}}"#
        )
        .is_none());
        assert!(body_of_line("this is not json").is_none());
    }

    /// parse() 与 body_of_line 必须出自同一套规则:对夹具里每个轮次行,
    /// body_of_line 抽出的正文与 parse 落库的 text 逐字节相同。
    #[test]
    fn omp_parse_与_body_of_line_规则一致() {
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

    /// 真实形状的工具调用参数行出事件;systemPrompt 目录与正文提及都不算。
    #[test]
    fn omp_skill_工具调用出事件_系统提示词里的目录行不算() {
        // custom / tool_execution_start 的 data.args 带 skill:// 路径。
        let custom = r#"{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"toolu_01","toolName":"read","startedAt":"2026-08-06T03:36:55.622Z","args":{"path":"skill://brainstorming"},"intent":"Loading brainstorming skill"},"id":"c1","parentId":"p1","timestamp":"2026-08-06T03:36:55.622Z"}"#;
        assert_eq!(skill_names_of_line(custom), vec!["brainstorming"]);

        // message.content[].toolCall.arguments 带 skill:// 路径。
        let toolcall = r#"{"type":"message","id":"m1","parentId":"p1","timestamp":"2026-08-06T03:36:55.619Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"想"},{"type":"toolCall","id":"t1","name":"read","arguments":{"path":"skill://plan-doc","i":"Reading plan-doc skill"}}]}}"#;
        assert_eq!(skill_names_of_line(toolcall), vec!["plan-doc"]);

        // 反例:session_init.systemPrompt 逐条列出全部技能名——目录不是调用。
        let init = r#"{"type":"session_init","id":"s1","timestamp":"2026-08-05T05:52:51.907Z","systemPrompt":"Available skills:\n- brainstorming\n- plan-doc\n- weekly-report\n- writing-plans"}"#;
        assert!(skill_names_of_line(init).is_empty());

        // custom_message / skill-prompt 是注入的技能正文,也不算。
        let prompt = r#"{"type":"custom_message","customType":"skill-prompt","content":"[IMPORTANT: The user has invoked the \"plan-doc\" skill...]"}"#;
        assert!(skill_names_of_line(prompt).is_empty());

        // 正文(toolResult 的 text)里带 skill:// 只是回显,不算。
        let result = r#"{"type":"message","message":{"role":"toolResult","toolName":"read","content":[{"type":"text","text":"reading skill://brainstorming content"}]}}"#;
        assert!(skill_names_of_line(result).is_empty());

        // 名字停在标点:句点不进名字(实测有 `skill://weekly-report.` 这种句子收尾)。
        let dotted = r#"{"type":"custom","data":{"args":{"path":"skill://weekly-report."}}}"#;
        assert_eq!(skill_names_of_line(dotted), vec!["weekly-report"]);
    }

    /// parse() 顺手产出事件:名字与时间戳都来自行本身。
    #[test]
    fn omp_parse_产出调用事件() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"custom\",\"customType\":\"tool_execution_start\",\"data\":{\"toolCallId\":\"t1\",\"toolName\":\"read\",\"startedAt\":\"2026-08-06T03:36:55.622Z\",\"args\":{\"path\":\"skill://bark-notify\"}},\"id\":\"c1\",\"parentId\":\"p1\",\"timestamp\":\"2026-08-06T03:36:55.622Z\"}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "bark-notify");
        assert_eq!(
            events[0].ts_ms,
            iso8601_to_ms("2026-08-06T03:36:55.622Z").unwrap()
        );
    }

    /// 调用行没有时间戳时退回会话内最后已知时间戳,绝不发明 `now`。
    #[test]
    fn omp_调用行无时间戳_退回会话最后已知时间戳() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"message\",\"id\":\"m1\",\"timestamp\":\"2026-08-06T03:36:55.000Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"帮我写周报\"}]}}\n");
        body.push_str("{\"type\":\"custom\",\"customType\":\"tool_execution_start\",\"data\":{\"toolCallId\":\"t1\",\"toolName\":\"read\",\"startedAt\":\"2026-08-06T03:36:55.622Z\",\"args\":{\"path\":\"skill://weekly-report\"}},\"id\":\"c1\",\"parentId\":\"p1\"}\n");
        std::fs::write(&path, body).unwrap();

        let (_, _, events) = parse(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].skill, "weekly-report");
        assert_eq!(
            events[0].ts_ms,
            iso8601_to_ms("2026-08-06T03:36:55.000Z").unwrap()
        );
    }
}
