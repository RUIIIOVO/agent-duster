//! `mcp/standard-json` / `mcp/codex-toml` / `mcp/opencode-json` / `mcp/gemini-json`
//! -> McpServerSpec。
//!
//! - standard-json：Claude Code 等大多数 agent 的 `mcpServers` 对象。
//! - codex-toml：Codex `config.toml` 里的 `[mcp_servers.<name>]` 表。
//! - opencode-json：opencode `opencode.jsonc` 里的 `.mcp` 对象（第三种方言）。
//! - gemini-json：Gemini CLI `settings.json` 里的 `mcpServers` 对象。键名看着
//!   与 standard-json 同构，传输判定规则却是它自己的一套（第四种方言）。
//!
//! 本机实测 2026-08-11：Cursor 曾被当作 standard-json 同构，实际上它**没有**
//! 全局 `~/.cursor/mcp.json`，MCP 是项目级的（`~/.cursor/projects/<路径编码>/mcps/`），
//! 所以今天没有任何内置清单把 standard-json 指向 Cursor。
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
/// 无 `type` 时有 `command` -> Stdio，只有端点 -> Http。
///
/// 端点有两个拼法：通用的 `url`，以及 Gemini CLI 起头的 `httpUrl`（见
/// [`parse_std_entry`]）。Gemini 自己已经不走这条 mapper 了，见
/// [`from_gemini_json`]。
pub fn from_standard_json(v: &serde_json::Value) -> anyhow::Result<Vec<McpServerSpec>> {
    let map = v.as_object().context("mcpServers is not a JSON object")?;
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
    let table = v.as_table().context("mcp_servers is not a TOML table")?;
    let mut out = Vec::with_capacity(table.len());
    for (name, entry) in table {
        match parse_codex_entry(name, entry) {
            Ok(spec) => out.push(spec),
            Err(e) => out.push(error_spec(name, &format!("{e:#}"), toml_raw(entry))),
        }
    }
    Ok(out)
}

/// 解析 opencode-json 方言。`v` 是 `.mcp` 对象本体：
/// `{"<name>": {"type":"local","command":[argv…],"environment":{..},"enabled":bool}
///           | {"type":"remote","url":"…","enabled":bool}}`。
///
/// 与 standard-json 的三处关键差异（本机实测 2026-08-11）：
/// 1. `command` 是**单个 argv 数组**，不是 `command` + `args` 两个字段。元素 0
///    是程序名，其余是参数——拆开存才能与另两种方言 content_hash 对得上，
///    否则同一个 server 在 opencode 与 Claude 下会被算成两行。
/// 2. 环境变量键名是 `environment`，不是 `env`。
/// 3. 传输靠 `type: "local" | "remote"` 二选一。方言里没有区分 http 与 sse 的
///    办法，`remote` 一律归 [`McpTransport::Http`]。
///
/// `enabled: false` 照样产出一条 spec：声明存在，用户就必须看得见。
/// [`McpServerSpec`] 没有「是否启用」字段，按本文件既有约定——未建模字段原样
/// 留在 `extra`——保留，不新增字段。它因此不进 content_hash，这是对的：
/// 禁用与否不改变 server 的语义身份，跨 agent 去重仍应把它和启用态合并成一行。
pub fn from_opencode_json(v: &serde_json::Value) -> anyhow::Result<Vec<McpServerSpec>> {
    let map = v.as_object().context("mcp is not a JSON object")?;
    let mut out = Vec::with_capacity(map.len());
    for (name, entry) in map {
        match parse_opencode_entry(name, entry) {
            Ok(spec) => out.push(spec),
            Err(e) => out.push(error_spec(name, &format!("{e:#}"), json_raw(entry))),
        }
    }
    Ok(out)
}

/// 解析 gemini-json 方言。`v` 是 `~/.gemini/settings.json` 里 `mcpServers`
/// 对象本体。键名与 standard-json 大面积重合，**判定规则却不是同一套**——
/// 拿 standard-json 去读 Gemini 的文件，读出来的传输方式会是错的。
///
/// 事实来源不是文档，是本机装着的那份实现（2026-08-11 实测，
/// `mise` 装的 `npm:@google/gemini-cli@0.54.4`，包内
/// `bundle/chunk-QXLHAGLO.js` 与 `bundle/chunk-BS6BSLZD.js` 里的
/// `createUrlTransport()` / `createTransport()`，两份 bundle 逐字一致）。
/// 分支顺序照抄如下：
///
/// 1. 有 `httpUrl` -> streamable HTTP。**连 `type` 都不看**：这一支在源码里
///    直接 return，所以 `{"httpUrl":…,"type":"sse"}` 在 Gemini 眼里是 HTTP。
/// 2. 有 `url` 且 `type` 是 `"http"` / `"sse"` -> 各按其字面。
/// 3. 只有光秃秃的 `url` -> streamable HTTP。
/// 4. 都没有，只有 `command` -> stdio。
/// 5. 一个都没有 -> Gemini 自己也连不上（它抛
///    `Invalid configuration: missing httpUrl…, url…, and command`），
///    我们同样降级为 `_parse_error` 占位条目。
///
/// 第 3 条与 Gemini 自己的 `docs/reference/configuration.md` 相矛盾：文档说
/// 「`url` 是 SSE 端点」。文档是旧时代的遗留——它通篇没提 `type` 这个键，而
/// `type` 既写在 settings 的 JSON Schema 里（`enum: ["stdio","sse","http"]`），
/// 又是 `gemini mcp add --transport …` 实际写出来的东西（包内
/// `packages/cli/src/commands/mcp/add.ts`）。早期 Gemini 确实只有
/// `url`(SSE) + `httpUrl`(HTTP) 两个键；`type` 落地后 `url` 的默认含义改成了
/// HTTP，`httpUrl` 转为「已弃用但优先级最高」的兼容键。**以实现为准。**
///
/// 与 [`from_standard_json`] 的实质差异，也就是必须另立一个方言的理由：
/// - `httpUrl` 与 `url` 同时出现时，Gemini 用 `httpUrl`，standard-json 用 `url`。
/// - `type` 是 Gemini 不认的值（含 `"streamable-http"`）时，它不报错，退回按
///   端点推断；standard-json 会把整条降级成 `_parse_error`。
/// - Gemini 的 `type` 词汇表只有 `stdio` / `sse` / `http` 三个。
///
/// 落选的那个键（被压掉的 `url`、没被建模的 `type`）一律**不取走**，随 extra
/// 原样保留——静默丢字段是这份 mapper 的红线。`tcp` 同理：它写在 settings 的
/// JSON Schema 里（注释是「TCP address for websocket transport」），但
/// `createTransport()` 里根本没有 websocket 分支，0.54.4 认不了它，所以它不是
/// 一种可建模的传输，只能待在 extra 里等某个未来版本把它接上。
pub fn from_gemini_json(v: &serde_json::Value) -> anyhow::Result<Vec<McpServerSpec>> {
    let map = v.as_object().context("mcpServers is not a JSON object")?;
    let mut out = Vec::with_capacity(map.len());
    for (name, entry) in map {
        match parse_gemini_entry(name, entry) {
            Ok(spec) => out.push(spec),
            Err(e) => out.push(error_spec(name, &format!("{e:#}"), json_raw(entry))),
        }
    }
    Ok(out)
}

// ---------- standard-json ----------

fn parse_std_entry(name: &str, v: &serde_json::Value) -> anyhow::Result<McpServerSpec> {
    let obj = v
        .as_object()
        .with_context(|| format!("server `{name}` is not an object"))?;
    // clone 后取走已建模字段，剩余的整体进 extra（serde_json 开了
    // preserve_order，键序与原文一致）。
    let mut rest = obj.clone();

    let command = take_json_str(&mut rest, "command")?;
    let args = take_json_str_array(&mut rest, "args")?;
    let env = take_json_str_map(&mut rest, "env")?;
    let mut url = take_json_str(&mut rest, "url")?;
    if url.is_none() {
        // `httpUrl` 是 Gemini CLI 起的头，但拼法已经外溢：任何抄了 Gemini
        // 写法的 standard-json 文件都可能这么写。Gemini 本身现在走
        // [`from_gemini_json`]，这里留的是对**其他** agent 的容忍。
        //
        // 不认这个键的后果不是少一个字段：整条声明会因为「既没有 command 也
        // 没有 url」降级成 `_parse_error` 占位条目，于是同一个 server 在
        // `duster mcp list` 里裂成两行、headers 也一并丢掉。
        //
        // `url` 仍是首选而不是同义词：它是 MCP 配置的通用拼法，两个键都在时
        // 让通用的那个赢。输掉的 `httpUrl` **不被取走**，随 extra 原样保留——
        // 静默丢字段是这份 mapper 的红线。（Gemini 自己的优先级相反，
        // httpUrl 先于 url；那条口径归 [`from_gemini_json`]，不能拿它改所有
        // 其他 agent 的读法。）
        url = take_json_str(&mut rest, "httpUrl")?;
    }
    let headers = take_json_str_map(&mut rest, "headers")?;
    let ty = take_json_str(&mut rest, "type")?;

    let transport = match (ty.as_deref(), &command, &url) {
        // streamable-http 是 http 传输的新拼法，归一到 Http。
        (Some("http" | "streamable-http" | "streamable_http"), _, _) => McpTransport::Http,
        (Some("sse"), _, _) => McpTransport::Sse,
        (Some("stdio") | None, Some(_), _) => McpTransport::Stdio,
        (None, None, Some(_)) => McpTransport::Http,
        (Some(other), _, _) => bail!("server `{name}` has unrecognized type `{other}`"),
        _ => bail!("server `{name}` has neither command nor url; cannot determine transport"),
    };

    let extra = if rest.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&rest).context("failed to serialize extra")?)
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
        Some(other) => bail!("field `{key}` should be a string, got {other}"),
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
        other => bail!("field `{key}` should be an array, got {other}"),
    };
    arr.into_iter()
        .map(|item| match item {
            serde_json::Value::String(s) => Ok(s),
            other => bail!("element of field `{key}` should be a string, got {other}"),
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
        other => bail!("field `{key}` should be an object, got {other}"),
    };
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        let s = match val {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => bail!("field `{key}.{k}` should be a scalar, got {other}"),
        };
        out.insert(k, s);
    }
    Ok(out)
}

// ---------- codex-toml ----------

fn parse_codex_entry(name: &str, v: &toml::Value) -> anyhow::Result<McpServerSpec> {
    let table = v
        .as_table()
        .with_context(|| format!("server `{name}` is not a TOML table"))?;
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
        (None, None) => {
            bail!("server `{name}` has neither command nor url; cannot determine transport")
        }
    };

    let extra = if rest.is_empty() {
        None
    } else {
        // TOML 表本身保序，转 JSON（preserve_order）后键序不变。
        let json = serde_json::to_value(&rest).context("failed to convert extra to JSON")?;
        Some(serde_json::to_string(&json).context("failed to serialize extra")?)
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
        Some(other) => bail!("field `{key}` should be a string, got {other}"),
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
        other => bail!("field `{key}` should be an array, got {other}"),
    };
    arr.into_iter()
        .map(|item| match item {
            toml::Value::String(s) => Ok(s),
            other => bail!("element of field `{key}` should be a string, got {other}"),
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
        other => bail!("field `{key}` should be a table, got {other}"),
    };
    let mut out = BTreeMap::new();
    for (k, val) in table {
        let s = match val {
            toml::Value::String(s) => s,
            toml::Value::Integer(n) => n.to_string(),
            toml::Value::Float(f) => f.to_string(),
            toml::Value::Boolean(b) => b.to_string(),
            other => bail!("field `{key}.{k}` should be a scalar, got {other}"),
        };
        out.insert(k, s);
    }
    Ok(out)
}

// ---------- opencode-json ----------

fn parse_opencode_entry(name: &str, v: &serde_json::Value) -> anyhow::Result<McpServerSpec> {
    let obj = v
        .as_object()
        .with_context(|| format!("server `{name}` is not an object"))?;
    let mut rest = obj.clone();

    // `command` 在这个方言里是 argv 数组，复用数组取值器而不是字符串取值器。
    let argv = take_json_str_array(&mut rest, "command")?;
    let url = take_json_str(&mut rest, "url")?;
    let env = take_json_str_map(&mut rest, "environment")?;
    let ty = take_json_str(&mut rest, "type")?;
    // `enabled` 刻意**不**取走：模型里没有对应字段，留在 rest 里随 extra 一起
    // 带走，「已声明但被禁用」这件事才不会在归一化中丢失。

    let mut argv = argv.into_iter();
    let command = argv.next();
    let args: Vec<String> = argv.collect();

    let transport = match (ty.as_deref(), &command, &url) {
        (Some("local"), Some(_), _) => McpTransport::Stdio,
        // 方言不区分 http / sse，remote 一律归 Http。
        (Some("remote"), _, Some(_)) => McpTransport::Http,
        // 无 type 时退回与 standard-json 相同的推断规则，两方言保持一致。
        (None, Some(_), _) => McpTransport::Stdio,
        (None, None, Some(_)) => McpTransport::Http,
        (Some("local"), None, _) => bail!("server `{name}` has type `local` but no command"),
        (Some("remote"), _, None) => bail!("server `{name}` has type `remote` but no url"),
        (Some(other), _, _) => bail!("server `{name}` has unrecognized type `{other}`"),
        // 与 standard-json 同一句话、同一种降级，不另立第二套约定。
        _ => bail!("server `{name}` has neither command nor url; cannot determine transport"),
    };

    let extra = if rest.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&rest).context("failed to serialize extra")?)
    };

    Ok(McpServerSpec {
        name: name.to_string(),
        transport,
        command,
        args,
        env,
        url,
        // 方言里没有请求头字段；真出现了会随未知字段落进 extra。
        headers: BTreeMap::new(),
        extra,
    })
}

// ---------- gemini-json ----------

fn parse_gemini_entry(name: &str, v: &serde_json::Value) -> anyhow::Result<McpServerSpec> {
    let obj = v
        .as_object()
        .with_context(|| format!("server `{name}` is not an object"))?;
    let mut rest = obj.clone();

    let command = take_json_str(&mut rest, "command")?;
    let args = take_json_str_array(&mut rest, "args")?;
    let env = take_json_str_map(&mut rest, "env")?;
    let headers = take_json_str_map(&mut rest, "headers")?;

    // 先看清楚哪个端点键会赢，再动手取——取错了，输的那个就被从 rest 里
    // 摘走、也就进不了 extra 了。Gemini 的优先级是 httpUrl 压 url。
    let via_http_url = rest.contains_key("httpUrl");
    let url = if via_http_url {
        take_json_str(&mut rest, "httpUrl")?
    } else {
        take_json_str(&mut rest, "url")?
    };
    // `type` 同样只是先看一眼：Gemini 不认的值它自己都不当回事（不报错，
    // 退回按端点推断），我们更没有理由把它吃掉。够格才在下面取走。
    let ty = rest.get("type").and_then(serde_json::Value::as_str);

    // 分支顺序 = `createUrlTransport()` + `createTransport()` 的分支顺序。
    // 第二个分量是「`type` 这次有没有被 transport 完整表达」——只有表达了
    // 才取走它，否则留在 extra 里，免得把「文件里写着 sse、实际跑 http」
    // 这种矛盾抹平。
    let (transport, ty_modelled) = match (&url, via_http_url, ty, &command) {
        // 1. httpUrl 赢者通吃，`type` 在这一支里是死字段。
        (Some(_), true, _, _) => (McpTransport::Http, false),
        // 2. url + 显式 type。Gemini 的词汇表里只有这两个值是活的。
        (Some(_), false, Some("sse"), _) => (McpTransport::Sse, true),
        (Some(_), false, Some("http"), _) => (McpTransport::Http, true),
        // 3. 光秃秃的 url，或 type 是 Gemini 不认的值 —— 都落到 streamable
        //    HTTP。这里刻意**不** bail：Gemini 自己就是这么兜底的，报错只会
        //    把一条它跑得起来的声明打成 `_parse_error`。
        (Some(_), false, _, _) => (McpTransport::Http, false),
        // 4. 没有端点才轮到 command。
        (None, _, t, Some(_)) => (McpTransport::Stdio, t == Some("stdio")),
        (None, _, _, None) => bail!(
            "server `{name}` has none of `command`, `url`, `httpUrl`; \
             cannot determine transport"
        ),
    };
    if ty_modelled {
        // shift_remove 保序，与 take_json_str 同一个理由。
        rest.shift_remove("type");
    }

    let extra = if rest.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&rest).context("failed to serialize extra")?)
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

    /// Gemini CLI 的真实形态（本机实测 2026-08-11 的 `~/.gemini/settings.json`）：
    /// 端点写在 `httpUrl`、没有 `type`、`headers` 带凭据、`timeout` 是数字。
    ///
    /// 这一条曾经整条降级成 `_parse_error` 占位条目：同一个 server 在
    /// `duster mcp list` 里裂成两行，headers 直接丢掉。
    #[test]
    fn std_json_gemini_的_httpurl_是端点而不是未知字段() {
        let v = json(
            r#"{
                "stitch": {
                    "httpUrl": "https://stitch.googleapis.com/mcp",
                    "headers": { "X-Goog-Api-Key": "AQ.secret" },
                    "timeout": 60000
                }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        // 无 type + 有端点 -> Http，与 `url` 那条路径同一个结论。
        assert_eq!(s.transport, McpTransport::Http);
        assert_eq!(s.url.as_deref(), Some("https://stitch.googleapis.com/mcp"));
        assert_eq!(s.headers.get("X-Goog-Api-Key").unwrap(), "AQ.secret");
        assert!(s.command.is_none());
        // 我们没建模的 `timeout` 进 extra，一个字段都不丢。
        assert_eq!(s.extra.as_deref(), Some(r#"{"timeout":60000}"#));

        // 与同一个 server 的 Claude 形态（type + url）语义哈希相同 ->
        // `duster mcp list` 合并成一行。这才是这个修复的意义。
        let claude = json(
            r#"{
                "stitch": {
                    "type": "http",
                    "url": "https://stitch.googleapis.com/mcp",
                    "headers": { "X-Goog-Api-Key": "AQ.secret" }
                }
            }"#,
        );
        let other = from_standard_json(&claude).unwrap();
        assert_eq!(
            s.content_hash(),
            other[0].content_hash(),
            "两种拼法同一个 server，语义哈希必须相同"
        );
    }

    /// 两个拼法都在时 `url` 赢——它是通用拼法；输掉的 `httpUrl` 不许被吞掉，
    /// 必须随 extra 原样留着，否则用户看不出这份配置里有两个端点在打架。
    #[test]
    fn std_json_url_压过_httpurl_且落选者进_extra() {
        let v = json(
            r#"{
                "both": {
                    "url": "https://standard.example.com/mcp",
                    "httpUrl": "https://gemini.example.com/mcp"
                }
            }"#,
        );
        let specs = from_standard_json(&v).unwrap();
        assert_eq!(
            specs[0].url.as_deref(),
            Some("https://standard.example.com/mcp")
        );
        assert_eq!(
            specs[0].extra.as_deref(),
            Some(r#"{"httpUrl":"https://gemini.example.com/mcp"}"#)
        );
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

    /// 本机实测形态（2026-08-11）：local + argv 数组、remote + url、
    /// enabled=false、既无 command 也无 url 的垃圾条目，四种一次覆盖。
    #[test]
    fn opencode_json_四种形态() {
        let v = json(
            r#"{
                "fs": {
                    "type": "local",
                    "command": ["npx", "-y", "server-fs", "--root", "/tmp"],
                    "environment": { "ROOT": "/tmp", "PORT": 8080 },
                    "enabled": true
                },
                "remote": {
                    "type": "remote",
                    "url": "https://mcp.example.com",
                    "enabled": true
                },
                "off": {
                    "type": "local",
                    "command": ["node", "off.js"],
                    "enabled": false
                },
                "junk": { "type": "local", "note": "既无 command 也无 url" }
            }"#,
        );
        let specs = from_opencode_json(&v).unwrap();
        assert_eq!(specs.len(), 4);

        // local：argv 数组拆成 command + args。
        let fs = &specs[0];
        assert_eq!(fs.transport, McpTransport::Stdio);
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, ["-y", "server-fs", "--root", "/tmp"]);
        assert_eq!(fs.env.get("ROOT").unwrap(), "/tmp");
        // environment 里的数字标量同样宽容转字符串。
        assert_eq!(fs.env.get("PORT").unwrap(), "8080");
        // enabled 不是建模字段，留在 extra 里。
        assert_eq!(fs.extra.as_deref(), Some(r#"{"enabled":true}"#));

        // remote：url 定传输，方言不区分 http/sse，归 Http。
        let remote = &specs[1];
        assert_eq!(remote.transport, McpTransport::Http);
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example.com"));
        assert!(remote.command.is_none());

        // enabled=false 仍然产出 spec，禁用状态记在 extra 而不是被丢弃。
        let off = &specs[2];
        assert_eq!(off.name, "off");
        assert_eq!(off.transport, McpTransport::Stdio);
        assert_eq!(off.command.as_deref(), Some("node"));
        assert_eq!(off.extra.as_deref(), Some(r#"{"enabled":false}"#));

        // 垃圾条目：与 standard-json 同一种降级，不中断、不静默丢弃。
        let junk = &specs[3];
        let extra: serde_json::Value =
            serde_json::from_str(junk.extra.as_deref().unwrap()).unwrap();
        assert!(extra.get("_parse_error").is_some());
        assert!(extra.get("_raw").is_some());
    }

    /// argv 数组拆 command + args 的意义全在这里：同一个 server 在 opencode
    /// 与 Claude 下声明，content_hash 必须相等，否则跨 agent 去重会漏。
    #[test]
    fn opencode_与_standard_json_content_hash_一致() {
        let o = json(
            r#"{
                "fs-opencode": {
                    "type": "local",
                    "command": ["npx", "-y", "server-fs"],
                    "environment": { "ROOT": "/tmp" },
                    "enabled": false
                }
            }"#,
        );
        let s = json(
            r#"{
                "fs-claude": {
                    "command": "npx",
                    "args": ["-y", "server-fs"],
                    "env": { "ROOT": "/tmp" }
                }
            }"#,
        );
        let os = from_opencode_json(&o).unwrap();
        let ss = from_standard_json(&s).unwrap();
        assert_ne!(os[0].extra, ss[0].extra);
        // 禁用状态只在 extra 里，不影响语义身份。
        assert_eq!(os[0].content_hash(), ss[0].content_hash());
    }

    /// 这个 mapper 只吃 JSON 节点：scan_mcp 用 `json_pointer` 定位，写成
    /// `toml_key` 会喂进一棵 TOML 树，运行时才炸。清单层面先钉死。
    #[test]
    fn 内置清单里的_opencode_json_必须用_json_pointer() {
        let manifests = crate::manifest::load_builtin();
        let mut seen = 0usize;
        for m in &manifests {
            for r in &m.resources {
                if r.mapper != crate::manifest::MapperName::McpOpencodeJson {
                    continue;
                }
                seen += 1;
                assert!(
                    r.json_pointer.is_some(),
                    "{} 的 {} 用了 mcp/opencode-json 却没有 json_pointer",
                    m.agent.id,
                    r.path
                );
                assert!(
                    r.toml_key.is_none(),
                    "{} 的 {} 用了 mcp/opencode-json 却写了 toml_key",
                    m.agent.id,
                    r.path
                );
            }
        }
        assert!(
            seen > 0,
            "内置清单里应至少有一处 mcp/opencode-json（opencode）"
        );
    }

    // ---------------------- gemini-json ----------------------

    /// 本机 `~/.gemini/settings.json` 里那条 stitch 的原样形态（实测
    /// 2026-08-11）：端点在 `httpUrl`、没有 `type`、`headers` 带凭据、
    /// `timeout` 是数字。
    ///
    /// 附带钉死 M2 的验收条件：它必须与 Claude、Codex 那两份声明合并成
    /// **一行**，也就是三者 content_hash 相等。
    #[test]
    fn gemini_json_真实的_stitch_条目() {
        let v = json(
            r#"{
                "stitch": {
                    "httpUrl": "https://stitch.googleapis.com/mcp",
                    "headers": { "X-Goog-Api-Key": "AQ.secret" },
                    "timeout": 60000
                }
            }"#,
        );
        let specs = from_gemini_json(&v).unwrap();
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.name, "stitch");
        assert_eq!(s.transport, McpTransport::Http);
        assert_eq!(s.url.as_deref(), Some("https://stitch.googleapis.com/mcp"));
        assert_eq!(s.headers.get("X-Goog-Api-Key").unwrap(), "AQ.secret");
        // 没建模的 timeout 一个字节都不丢。
        assert_eq!(s.extra.as_deref(), Some(r#"{"timeout":60000}"#));

        let claude = from_standard_json(&json(
            r#"{
                "stitch": {
                    "type": "http",
                    "url": "https://stitch.googleapis.com/mcp",
                    "headers": { "X-Goog-Api-Key": "AQ.secret" }
                }
            }"#,
        ))
        .unwrap();
        let codex = from_codex_toml(
            &r#"
[stitch]
url = "https://stitch.googleapis.com/mcp"
[stitch.http_headers]
X-Goog-Api-Key = "AQ.secret"
"#
            .parse::<toml::Value>()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            s.content_hash(),
            claude[0].content_hash(),
            "gemini 与 claude 的 stitch 必须合并成一行"
        );
        assert_eq!(
            s.content_hash(),
            codex[0].content_hash(),
            "gemini 与 codex 的 stitch 必须合并成一行"
        );
    }

    /// 传输判定逐条对齐 0.54.4 的 `createUrlTransport()`。
    ///
    /// 尤其是 `bare`：Gemini 的 `docs/reference/configuration.md` 说光秃秃的
    /// `url` 是 SSE，**实现说不是**——那一支 return 的是
    /// `StreamableHTTPClientTransport`。文档停留在 `type` 这个键出现之前的
    /// 年代。以实现为准，这条断言就是那份文档的勘误。
    #[test]
    fn gemini_json_传输判定按实现而非文档() {
        let v = json(
            r#"{
                "bare":      { "url": "https://bare.example.com/mcp" },
                "typed_sse": { "url": "https://sse.example.com/mcp",  "type": "sse" },
                "typed_http":{ "url": "https://http.example.com/mcp", "type": "http" },
                "http_url":  { "httpUrl": "https://h.example.com/mcp" },
                "stdio":     { "command": "npx", "args": ["-y", "srv"],
                               "env": { "K": "V" } }
            }"#,
        );
        let specs = from_gemini_json(&v).unwrap();
        let by = |n: &str| specs.iter().find(|s| s.name == n).unwrap();

        assert_eq!(
            by("bare").transport,
            McpTransport::Http,
            "光秃秃的 url 走 StreamableHTTP，不是 SSE"
        );
        assert_eq!(by("typed_sse").transport, McpTransport::Sse);
        assert_eq!(by("typed_http").transport, McpTransport::Http);
        assert_eq!(by("http_url").transport, McpTransport::Http);
        assert_eq!(by("stdio").transport, McpTransport::Stdio);
        assert_eq!(by("stdio").command.as_deref(), Some("npx"));
        assert_eq!(by("stdio").args, ["-y", "srv"]);
        assert_eq!(by("stdio").env.get("K").unwrap(), "V");
        // 被 transport 完整表达的 type 取走了，谁也不剩。
        for n in ["bare", "typed_sse", "typed_http", "http_url", "stdio"] {
            assert!(by(n).extra.is_none(), "{n} 不该有 extra：{:?}", by(n).extra);
        }
    }

    /// 与 standard-json 分道扬镳的三处，逐条钉住。
    #[test]
    fn gemini_json_与_standard_json_的三处分歧() {
        // 1. httpUrl 压过 url —— standard-json 的结论正相反。
        let both = json(
            r#"{ "x": { "url": "https://sse.example.com/mcp",
                        "httpUrl": "https://http.example.com/mcp" } }"#,
        );
        let g = &from_gemini_json(&both).unwrap()[0];
        let s = &from_standard_json(&both).unwrap()[0];
        assert_eq!(g.url.as_deref(), Some("https://http.example.com/mcp"));
        assert_eq!(s.url.as_deref(), Some("https://sse.example.com/mcp"));
        // 落选的 url 留在 extra，两个端点在打架这件事看得见。
        assert_eq!(
            g.extra.as_deref(),
            Some(r#"{"url":"https://sse.example.com/mcp"}"#)
        );

        // 2. httpUrl 在场时连 type 都不看：源码里那一支直接 return。
        let contradictory =
            json(r#"{ "x": { "httpUrl": "https://h.example.com/mcp", "type": "sse" } }"#);
        let g = &from_gemini_json(&contradictory).unwrap()[0];
        assert_eq!(g.transport, McpTransport::Http);
        // 这个 type 没被表达，不许悄悄吃掉。
        assert_eq!(g.extra.as_deref(), Some(r#"{"type":"sse"}"#));

        // 3. type 词汇表不一样，且不认的时候反应也不一样。
        //    `streamable-http` 在 standard-json 里是 http 的别名，Gemini 的
        //    枚举里没有它：两边都得出 Http，但 Gemini 是靠 url 兜底得出的，
        //    那个它没看懂的 type 必须原样留着。
        let alias = json(
            r#"{ "x": { "url": "https://x.example.com/mcp",
                                     "type": "streamable-http" } }"#,
        );
        let g = &from_gemini_json(&alias).unwrap()[0];
        let s = &from_standard_json(&alias).unwrap()[0];
        assert_eq!(g.transport, McpTransport::Http);
        assert_eq!(s.transport, McpTransport::Http);
        assert_eq!(g.extra.as_deref(), Some(r#"{"type":"streamable-http"}"#));
        assert!(s.extra.is_none(), "standard-json 认得这个别名，会取走它");

        //    两边都不认的值上分歧才彻底：Gemini 退回按端点推断照样能跑，
        //    standard-json 把整条降级成 `_parse_error`。
        let odd = json(
            r#"{ "x": { "url": "https://x.example.com/mcp",
                                   "type": "grpc" } }"#,
        );
        let g = &from_gemini_json(&odd).unwrap()[0];
        assert_eq!(g.transport, McpTransport::Http);
        assert_eq!(g.url.as_deref(), Some("https://x.example.com/mcp"));
        assert_eq!(g.extra.as_deref(), Some(r#"{"type":"grpc"}"#));
        let s = &from_standard_json(&odd).unwrap()[0];
        assert!(
            s.extra.as_deref().unwrap().contains("_parse_error"),
            "standard-json 对不认的 type 是降级，不是兜底"
        );
    }

    /// `tcp` 写在 settings 的 JSON Schema 里（「TCP address for websocket
    /// transport」），但 0.54.4 的 `createTransport()` 根本没有 websocket
    /// 分支——它是死配置。所以：不建模、不发明、也不丢，待在 extra 里。
    ///
    /// 只有 `tcp` 的条目 Gemini 自己也连不上（抛 `Invalid configuration:
    /// missing httpUrl…, url…, and command`），我们同样降级。
    #[test]
    fn gemini_json_tcp_是死配置不是传输() {
        let with_url =
            json(r#"{ "x": { "url": "https://x.example.com/mcp", "tcp": "localhost:9000" } }"#);
        let g = &from_gemini_json(&with_url).unwrap()[0];
        assert_eq!(g.transport, McpTransport::Http);
        assert_eq!(g.extra.as_deref(), Some(r#"{"tcp":"localhost:9000"}"#));

        let only_tcp = json(r#"{ "x": { "tcp": "localhost:9000" } }"#);
        let g = &from_gemini_json(&only_tcp).unwrap()[0];
        let extra: serde_json::Value = serde_json::from_str(g.extra.as_deref().unwrap()).unwrap();
        assert!(extra.get("_parse_error").is_some());
        assert_eq!(extra["_raw"]["tcp"], "localhost:9000");
    }

    /// 剩下的字段是 Gemini 有、我们没建模的那一堆（trust / description /
    /// includeTools / excludeTools / oauth / cwd …）。一个都不许掉。
    #[test]
    fn gemini_json_未建模字段全数进_extra() {
        let v = json(
            r#"{
                "x": {
                    "command": "npx",
                    "cwd": "/srv",
                    "trust": true,
                    "description": "demo",
                    "includeTools": ["a"],
                    "excludeTools": ["b"],
                    "oauth": { "enabled": true },
                    "authProviderType": "dynamic_discovery",
                    "extension": { "name": "ext" }
                }
            }"#,
        );
        let g = &from_gemini_json(&v).unwrap()[0];
        assert_eq!(g.transport, McpTransport::Stdio);
        let extra: serde_json::Value = serde_json::from_str(g.extra.as_deref().unwrap()).unwrap();
        for k in [
            "cwd",
            "trust",
            "description",
            "includeTools",
            "excludeTools",
            "oauth",
            "authProviderType",
            "extension",
        ] {
            assert!(extra.get(k).is_some(), "`{k}` 丢了");
        }
    }

    /// 与 opencode 那条同样的道理：这个 mapper 只吃 JSON 节点，清单层面钉死。
    #[test]
    fn 内置清单里的_gemini_json_必须用_json_pointer() {
        let manifests = crate::manifest::load_builtin();
        let mut seen = 0usize;
        for m in &manifests {
            for r in &m.resources {
                if r.mapper != crate::manifest::MapperName::McpGeminiJson {
                    continue;
                }
                seen += 1;
                assert!(
                    r.json_pointer.is_some(),
                    "{} 的 {} 用了 mcp/gemini-json 却没有 json_pointer",
                    m.agent.id,
                    r.path
                );
                assert!(
                    r.toml_key.is_none(),
                    "{} 的 {} 用了 mcp/gemini-json 却写了 toml_key",
                    m.agent.id,
                    r.path
                );
            }
        }
        assert!(
            seen > 0,
            "内置清单里应至少有一处 mcp/gemini-json（gemini-cli）"
        );
    }
}
