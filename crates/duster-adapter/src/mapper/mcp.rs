//! `mcp/standard-json` 与 `mcp/codex-toml` -> McpServerSpec。
//!
//! - standard-json：Claude / Gemini / Cursor 同构的 `mcpServers` 对象。
//! - codex-toml：Codex `config.toml` 里的 `[mcp_servers.<name>]` 表。
//!
//! 容错原则与 skill mapper 一致：宁可信息降级，不可解析中断。单个 server
//! 损坏时降级为携带 `extra._parse_error` 的占位条目（而非直接跳过）——签名
//! 只能返回 `Vec<McpServerSpec>`，静默丢弃会让迁移有损、用户也看不到问题；
//! 展示/迁移层据 `_parse_error` 跳过或提示，不参与去重合并。

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use duster_model::{McpServerSpec, McpTransport};

/// 解析 standard-json 方言。`v` 是 `mcpServers` 对象本体：
/// `{"name": {"command":..,"args":[..],"env":{..}} | {"type":"http"|"sse","url":..,"headers":{..}}}`。
///
/// 传输判定：显式 `type` 优先（http/streamable-http -> Http，sse -> Sse）；
/// 无 `type` 时有 `command` -> Stdio，只有 `url` -> Http。
pub fn from_standard_json(v: &serde_json::Value) -> anyhow::Result<Vec<McpServerSpec>> {
    let map = v.as_object().context("mcpServers 不是 JSON 对象")?;
    let mut out = Vec::with_capacity(map.len());
    for (name, entry) in map {
        match parse_std_entry(name, entry) {
            Ok(spec) => out.push(spec),
            // 单个 server 损坏不中断整体：降级为 _parse_error 占位条目。
            Err(e) => out.push(error_spec(name, &format!("{e:#}"), json_raw(entry))),
        }
    }
    Ok(out)
}

/// 解析 codex-toml 方言。`v` 是 `mcp_servers` 表本体：
/// `[mcp_servers.<name>]` 下 `command`/`args`/`env` + 可选 `url`、`http_headers` 子表。
///
/// Codex 无显式 type 字段：有 `command` -> Stdio，只有 `url` -> Http。
pub fn from_codex_toml(v: &toml::Value) -> anyhow::Result<Vec<McpServerSpec>> {
    let table = v.as_table().context("mcp_servers 不是 TOML 表")?;
    let mut out = Vec::with_capacity(table.len());
    for (name, entry) in table {
        match parse_codex_entry(name, entry) {
            Ok(spec) => out.push(spec),
            Err(e) => out.push(error_spec(name, &format!("{e:#}"), toml_raw(entry))),
        }
    }
    Ok(out)
}

// ---------- standard-json ----------

fn parse_std_entry(name: &str, v: &serde_json::Value) -> anyhow::Result<McpServerSpec> {
    let obj = v
        .as_object()
        .with_context(|| format!("server `{name}` 不是对象"))?;
    // clone 后取走已建模字段，剩余的整体进 extra（serde_json 开了
    // preserve_order，键序与原文一致）。
    let mut rest = obj.clone();

    let command = take_json_str(&mut rest, "command")?;
    let args = take_json_str_array(&mut rest, "args")?;
    let env = take_json_str_map(&mut rest, "env")?;
    let url = take_json_str(&mut rest, "url")?;
    let headers = take_json_str_map(&mut rest, "headers")?;
    let ty = take_json_str(&mut rest, "type")?;

    let transport = match (ty.as_deref(), &command, &url) {
        // streamable-http 是 http 传输的新拼法，归一到 Http。
        (Some("http" | "streamable-http" | "streamable_http"), _, _) => McpTransport::Http,
        (Some("sse"), _, _) => McpTransport::Sse,
        (Some("stdio") | None, Some(_), _) => McpTransport::Stdio,
        (None, None, Some(_)) => McpTransport::Http,
        (Some(other), _, _) => bail!("server `{name}` 的 type `{other}` 无法识别"),
        _ => bail!("server `{name}` 既无 command 也无 url，无法判定传输方式"),
    };

    let extra = if rest.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&rest).context("序列化 extra 失败")?)
    };

    Ok(McpServerSpec {
        name: name.to_string(),
        transport,
        command,
        args,
        env,
        url,
        headers,
        extra,
    })
}

/// 取走字符串字段；类型不符视为该 server 损坏。
fn take_json_str(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    // preserve_order 下 remove 是 swap_remove，会打乱剩余键序；shift_remove 保序。
    match m.shift_remove(key) {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(other) => bail!("字段 `{key}` 应为字符串，实际为 {other}"),
    }
}

/// 取走字符串数组字段。
fn take_json_str_array(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<Vec<String>> {
    let Some(v) = m.shift_remove(key) else {
        return Ok(Vec::new());
    };
    let arr = match v {
        serde_json::Value::Array(a) => a,
        other => bail!("字段 `{key}` 应为数组，实际为 {other}"),
    };
    arr.into_iter()
        .map(|item| match item {
            serde_json::Value::String(s) => Ok(s),
            other => bail!("字段 `{key}` 的元素应为字符串，实际为 {other}"),
        })
        .collect()
}

/// 取走字符串映射字段（env/headers）。标量值（数字/布尔）宽容地转成字符串
/// ——现实配置里 `"PORT": 8080` 很常见；这也保证与 TOML 方言哈希一致。
fn take_json_str_map(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let Some(v) = m.shift_remove(key) else {
        return Ok(BTreeMap::new());
    };
    let obj = match v {
        serde_json::Value::Object(o) => o,
        other => bail!("字段 `{key}` 应为对象，实际为 {other}"),
    };
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        let s = match val {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => bail!("字段 `{key}.{k}` 应为标量，实际为 {other}"),
        };
        out.insert(k, s);
    }
    Ok(out)
}

// ---------- codex-toml ----------

fn parse_codex_entry(name: &str, v: &toml::Value) -> anyhow::Result<McpServerSpec> {
    let table = v
        .as_table()
        .with_context(|| format!("server `{name}` 不是 TOML 表"))?;
    let mut rest = table.clone();

    let command = take_toml_str(&mut rest, "command")?;
    let args = take_toml_str_array(&mut rest, "args")?;
    let env = take_toml_str_map(&mut rest, "env")?;
    let url = take_toml_str(&mut rest, "url")?;
    // Codex 的请求头键名是 http_headers，归一到 headers。
    let headers = take_toml_str_map(&mut rest, "http_headers")?;

    let transport = match (&command, &url) {
        (Some(_), _) => McpTransport::Stdio,
        (None, Some(_)) => McpTransport::Http,
        (None, None) => bail!("server `{name}` 既无 command 也无 url，无法判定传输方式"),
    };

    let extra = if rest.is_empty() {
        None
    } else {
        // TOML 表本身保序，转 JSON（preserve_order）后键序不变。
        let json = serde_json::to_value(&rest).context("extra 转 JSON 失败")?;
        Some(serde_json::to_string(&json).context("序列化 extra 失败")?)
    };

    Ok(McpServerSpec {
        name: name.to_string(),
        transport,
        command,
        args,
        env,
        url,
        headers,
        extra,
    })
}

fn take_toml_str(
    m: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    match m.remove(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s)),
        Some(other) => bail!("字段 `{key}` 应为字符串，实际为 {other}"),
    }
}

fn take_toml_str_array(
    m: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> anyhow::Result<Vec<String>> {
    let Some(v) = m.remove(key) else {
        return Ok(Vec::new());
    };
    let arr = match v {
        toml::Value::Array(a) => a,
        other => bail!("字段 `{key}` 应为数组，实际为 {other}"),
    };
    arr.into_iter()
        .map(|item| match item {
            toml::Value::String(s) => Ok(s),
            other => bail!("字段 `{key}` 的元素应为字符串，实际为 {other}"),
        })
        .collect()
}

/// 与 [`take_json_str_map`] 同样的标量宽容规则，保证双方言哈希一致。
fn take_toml_str_map(
    m: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let Some(v) = m.remove(key) else {
        return Ok(BTreeMap::new());
    };
    let table = match v {
        toml::Value::Table(t) => t,
        other => bail!("字段 `{key}` 应为表，实际为 {other}"),
    };
    let mut out = BTreeMap::new();
    for (k, val) in table {
        let s = match val {
            toml::Value::String(s) => s,
            toml::Value::Integer(n) => n.to_string(),
            toml::Value::Float(f) => f.to_string(),
            toml::Value::Boolean(b) => b.to_string(),
            other => bail!("字段 `{key}.{k}` 应为标量，实际为 {other}"),
        };
        out.insert(k, s);
    }
    Ok(out)
}

// ---------- 降级 ----------

/// 坏 server 的占位条目：name + `extra._parse_error`（含原始内容，避免有损）。
/// transport 兜底为 Stdio 且各字段为空——content_hash 会与其他空条目撞车，
/// 但 `_parse_error` 条目本就不该参与去重，由消费方过滤。
fn error_spec(name: &str, err: &str, raw: serde_json::Value) -> McpServerSpec {
    McpServerSpec {
        name: name.to_string(),
        transport: McpTransport::Stdio,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        extra: Some(serde_json::json!({ "_parse_error": err, "_raw": raw }).to_string()),
    }
}

fn json_raw(v: &serde_json::Value) -> serde_json::Value {
    v.clone()
}

/// TOML 原文转 JSON；极端情况下（如 datetime 序列化失败）退化为字符串表示。
fn toml_raw(v: &toml::Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or_else(|_| serde_json::Value::String(v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn std_json_stdio_形态() {
        let v = json(
            r#"{
                "fs": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                    "env": { "ROOT": "/tmp", "PORT": 8080 }
                }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.name, "fs");
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(s.args, ["-y", "@modelcontextprotocol/server-filesystem"]);
        assert_eq!(s.env.get("ROOT").unwrap(), "/tmp");
        // 数字标量宽容转字符串。
        assert_eq!(s.env.get("PORT").unwrap(), "8080");
        assert!(s.extra.is_none());
    }

    #[test]
    fn std_json_http_与_sse_形态() {
        let v = json(
            r#"{
                "remote": {
                    "type": "http",
                    "url": "https://mcp.example.com",
                    "headers": { "Authorization": "Bearer x" }
                },
                "events": { "type": "sse", "url": "https://sse.example.com" },
                "bare_url": { "url": "https://bare.example.com" }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].transport, McpTransport::Http);
        assert_eq!(specs[0].url.as_deref(), Some("https://mcp.example.com"));
        assert_eq!(specs[0].headers.get("Authorization").unwrap(), "Bearer x");
        assert_eq!(specs[1].transport, McpTransport::Sse);
        // 无 type、只有 url -> 默认 Http。
        assert_eq!(specs[2].transport, McpTransport::Http);
    }

    #[test]
    fn std_json_extra_保留未知字段且保持键序() {
        let v = json(
            r#"{
                "srv": {
                    "command": "run",
                    "zeta": 1,
                    "alpha": { "b": 2 },
                    "disabled": true
                }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        let extra = specs[0].extra.as_deref().unwrap();
        // preserve_order：extra 里键序与原文一致（zeta 在 alpha 前面）。
        assert_eq!(extra, r#"{"zeta":1,"alpha":{"b":2},"disabled":true}"#);
    }

    #[test]
    fn std_json_坏_server_降级不中断() {
        let v = json(
            r#"{
                "good": { "command": "ok" },
                "no_target": { "note": "既无 command 也无 url" },
                "bad_type": { "command": 42 }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].transport, McpTransport::Stdio);
        assert!(specs[0].extra.is_none());
        for bad in &specs[1..] {
            let extra: serde_json::Value =
                serde_json::from_str(bad.extra.as_deref().unwrap()).unwrap();
            assert!(extra.get("_parse_error").is_some(), "{}", bad.name);
            assert!(extra.get("_raw").is_some());
        }
    }

    #[test]
    fn codex_toml_stdio_与_http_形态() {
        let doc: toml::Value = toml::from_str(
            r#"
                [fs]
                command = "npx"
                args = ["-y", "server-fs"]
                startup_timeout_sec = 20
                [fs.env]
                ROOT = "/tmp"

                [remote]
                url = "https://mcp.example.com"
                [remote.http_headers]
                Authorization = "Bearer x"
            "#,
        )
        .unwrap();
        let specs = from_codex_toml(&doc).unwrap();
        assert_eq!(specs.len(), 2);
        let fs = specs.iter().find(|s| s.name == "fs").unwrap();
        assert_eq!(fs.transport, McpTransport::Stdio);
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.env.get("ROOT").unwrap(), "/tmp");
        // 未建模字段进 extra。
        assert_eq!(fs.extra.as_deref(), Some(r#"{"startup_timeout_sec":20}"#));
        let remote = specs.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport, McpTransport::Http);
        assert_eq!(remote.headers.get("Authorization").unwrap(), "Bearer x");
        assert!(remote.extra.is_none());
    }

    #[test]
    fn codex_toml_坏_server_降级不中断() {
        let doc: toml::Value = toml::from_str(
            r#"
                [good]
                command = "ok"

                [broken]
                note = "没有 command 也没有 url"
            "#,
        )
        .unwrap();
        let specs = from_codex_toml(&doc).unwrap();
        assert_eq!(specs.len(), 2);
        let broken = specs.iter().find(|s| s.name == "broken").unwrap();
        let extra: serde_json::Value =
            serde_json::from_str(broken.extra.as_deref().unwrap()).unwrap();
        assert!(extra.get("_parse_error").is_some());
    }

    /// 同一 server 在两种方言下声明（名字都不同），content_hash 必须相等。
    #[test]
    fn 双方言_content_hash_一致() {
        // stdio 形态：extra 各不相同（方言残留），但不参与哈希。
        let j = json(
            r#"{
                "fs-claude": {
                    "command": "npx",
                    "args": ["-y", "server-fs"],
                    "env": { "ROOT": "/tmp", "PORT": 8080 },
                    "claude_only": true
                }
            }"#,
        );
        let t: toml::Value = toml::from_str(
            r#"
                [fs-codex]
                command = "npx"
                args = ["-y", "server-fs"]
                startup_timeout_sec = 20
                [fs-codex.env]
                ROOT = "/tmp"
                PORT = 8080
            "#,
        )
        .unwrap();
        let js = from_standard_json(&j).unwrap();
        let ts = from_codex_toml(&t).unwrap();
        assert_ne!(js[0].name, ts[0].name);
        assert_ne!(js[0].extra, ts[0].extra);
        assert_eq!(js[0].content_hash(), ts[0].content_hash());

        // http 形态：headers 键名不同（headers vs http_headers），归一后哈希一致。
        let j2 = json(
            r#"{
                "remote-a": {
                    "type": "http",
                    "url": "https://mcp.example.com",
                    "headers": { "Authorization": "Bearer x" }
                }
            }"#,
        );
        let t2: toml::Value = toml::from_str(
            r#"
                [remote-b]
                url = "https://mcp.example.com"
                [remote-b.http_headers]
                Authorization = "Bearer x"
            "#,
        )
        .unwrap();
        let js2 = from_standard_json(&j2).unwrap();
        let ts2 = from_codex_toml(&t2).unwrap();
        assert_eq!(js2[0].content_hash(), ts2[0].content_hash());
        // 不同 server 之间哈希不同（防止 feed 编码退化）。
        assert_ne!(js[0].content_hash(), js2[0].content_hash());
    }
}
