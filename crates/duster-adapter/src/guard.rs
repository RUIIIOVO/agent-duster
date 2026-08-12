//! schema_guard：结构指纹对不上就**自动降级为只读**，并且说出来。
//!
//! # 它防的是什么
//!
//! duster 回写的是别人家的配置文件。`~/.claude.json` 里躺着二十条 MCP 声明、
//! 一堆 duster 不认识的键，还有用户手写的东西。我们的保守回写策略
//! （toml_edit 保注释、JSON 保序、只改目标键）成立的前提是**文件的形状
//! 还是我们上次见到的那个形状**。
//!
//! 上游 agent 一次升级就能推翻这个前提：`mcpServers` 变成
//! `mcp.servers`、条目从对象变成数组、多出一层 `version` 包装。这时候
//! 「只改目标键」会改到一个语义已经不同的位置，而所有单元测试都还是绿的——
//! 它们用的是我们自己造的 fixture，不是用户升级后的真文件。
//!
//! 所以需要一道运行时闸门：**记住上次成功解析时文件长什么样，形状变了就
//! 不许再写**。宁可让用户手动改一次配置，也不能让 duster 写坏它。
//!
//! # 指纹取什么
//!
//! 只取**结构**，不取值：键路径 + 每个叶子的类型。
//! - 用户新增一条 MCP 声明 → 结构不变（同一层多一个同形状的兄弟），指纹不变。
//!   这是必须的：否则每加一个 server 都要用户重新确认一次，闸门会被当噪声关掉。
//! - 上游把 `command` 从字符串改成数组 → 类型变了，指纹变，拒绝回写。
//! - 上游多包一层 → 路径变了，指纹变，拒绝回写。
//!
//! 具体做法见 [`fingerprint`]：数组按「元素形状的并集」折叠成一个代表元；
//! **值形状一律相同且条目多于一个**的对象（`mcpServers` 这类注册表）
//! 把键名整个折成 `*`。所以「三条 server」与「四条 server」指纹相同，
//! 而「server 从对象变数组」「多包一层 `version`」不同。
//!
//! # 期望值从哪来
//!
//! **自学习**，不写进清单。第一次成功解析时把指纹记进索引的 `meta` 表
//! （键 `guard:<resource-key>`），之后每次读都比一次。这样新增 agent、
//! 新增文件都不需要有人先去清单里填一串哈希——那种设计上线第一天就会腐坏。
//!
//! 代价是「第一次见到的形状」被当成基准，哪怕它本来就是坏的。可以接受：
//! guard 的职责是发现**变化**，不是判断对错。

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use crate::codec::Doc;

/// 一次守卫判定的结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 库里还没有基准，本次记录为基准。首次见到不算异常。
    Learned,
    /// 与基准一致，回写放行。
    Match,
    /// 与基准不一致，**降级只读**。`reason` 是给用户看的人话，
    /// 必须说清「哪一处结构变了」，不能只说"指纹不同"。
    Drifted {
        expected: String,
        actual: String,
        reason: String,
    },
}

impl Verdict {
    /// 是否允许回写。只有 [`Verdict::Match`] 与 [`Verdict::Learned`] 放行。
    pub fn allows_write(&self) -> bool {
        !matches!(self, Verdict::Drifted { .. })
    }
}

/// 计算一份文档的**结构指纹**（BLAKE3 hex，前 16 字符）。
///
/// 规则见模块文档。实现要求：
/// - 对象：按键名字典序遍历，`键名:子形状` 拼接成 `{k1:s1,k2:s2}`；
///   若**所有值形状相同且条目多于一个**，整体折成 `{*:<形状>}`（见下）；
/// - 数组：把所有元素的形状**去重排序**后折叠成一个代表元 `[sA|sB]`，
///   空数组是一个独立形状（`[]`）；
/// - 叶子：只记类型标签（`str` / `num` / `bool` / `null`），不记值；
/// - JSON 与 TOML 走同一套标签，同构的两份文件指纹相同——
///   这是 `mcp list` 跨格式合并的前提；
/// - 递归深度封顶 [`MAX_DEPTH`]，超出的子树一律记成 [`TRUNCATED`]。
///
/// # 为什么要有 `{*:…}` 这条折叠
///
/// 「用户新增一条 MCP 声明不该改变指纹」这个要求，光靠「同层同名键归一」
/// 达不到：两条 server 是两个**不同的键名**（`mcpServers.a` 与
/// `mcpServers.b`），键名进了形状串，加一条就变。所以注册表这类
/// 「键名是自由文本、值形状一律相同」的映射必须把键名整个丢掉。
///
/// 判据只能是结构性的——我们不认识 `mcpServers` 这个名字，也不该认识：
/// **每个值的形状都一样，且条目多于一个**。两个已知代价，都可以接受：
/// - 误折：`{"host":"h","user":"u"}` 这种两个字符串字段的普通对象也会折成
///   `{*:str}`，于是改键名、加第三个字符串字段都察觉不到。折叠只会让闸门
///   **更宽松**（漏报），不会让它误伤（误报），所以宁可漏。
/// - 条目从 1 个变成 2 个时形状确实会变（`{a:S}` → `{*:S}`），用户会被要求
///   重新确认一次。不能靠「单条目也折」绕开：那样 `{"mcpServers": {…}}`
///   会折成 `{*:{…}}`，上游把 `mcpServers` 改名成 `mcp.servers` 就查不出来，
///   而这恰恰是本模块要防的第一号场景。
pub fn fingerprint(doc: &Doc) -> String {
    let shape = match doc {
        Doc::Json(v) => shape_json(v, 0),
        Doc::Toml(v) => shape_toml(v, 0),
    };
    let hex = blake3::hash(shape.as_bytes()).to_hex();
    // 截到 16 个 hex 字符（64 bit）：配置文件这种量级不可能撞上，
    // 又短到能原样塞进 meta 表和报错信息里给人看。
    hex[..16].to_string()
}

/// 人类可读的结构轮廓，用于 [`Verdict::Drifted`] 的 `reason`。
///
/// 指纹是给机器比对的，用户看到 `a1b2c3 != d4e5f6` 什么也做不了。
/// 这里给出「差在哪」：新增的键路径、消失的键路径，以及类型变了的
/// 键路径（`command: str → arr`）。
///
/// # 谁能调
///
/// 两个参数都是 `&Doc`，也就是**必须同时拿得到新旧两份文档**。
/// [`check`] 拿不到：它在 `meta` 表里只存了一串指纹，旧文档早没了，
/// 所以它的 `reason` 只能是一句泛泛的提示。调用方手里若有上一次的解析
/// 结果（例如刚跑完结构 diff 的 duster-core），应当改调本函数，把
/// `reason` 换成具体那几条——用户才知道去文件的哪一行看。
///
/// # 与指纹的口径差异
///
/// 这里**不做** `{*:…}` 折叠：折叠是为了让闸门别乱叫，而一旦叫了，
/// 用户要的是真实键名（`mcpServers.alpha.command`，不是 `mcpServers.*.command`）。
/// 数组下标则仍然折成 `[]`——`args[3]` 对人没有信息量。
///
/// 输出封顶 [`MAX_DRIFT_ITEMS`] 条，多出来的折成 `… and N more`：
/// 一屏铺满的告警等于没有告警，用户只会把闸门整个关掉。
pub fn describe_drift(expected: &Doc, actual: &Doc) -> String {
    let old = flatten(expected);
    let new = flatten(actual);

    // 顺序是有意的：类型变化排最前。它才是「保守回写会写坏东西」的那一类，
    // 新增/消失通常只是用户自己动过配置。
    let mut items: Vec<String> = Vec::new();
    for (path, old_tag) in &old {
        match new.get(path) {
            Some(new_tag) if new_tag != old_tag => {
                items.push(format!("{}: {old_tag} → {new_tag}", show(path)));
            }
            _ => {}
        }
    }
    for (path, tag) in &new {
        if !old.contains_key(path) {
            items.push(format!("added {} ({tag})", show(path)));
        }
    }
    for (path, tag) in &old {
        if !new.contains_key(path) {
            items.push(format!("removed {} ({tag})", show(path)));
        }
    }

    if items.is_empty() {
        // 指纹不同但展开后一模一样：只可能是折叠口径造成的（例如注册表从
        // 1 条变成 2 条）。照实说，别硬编一个不存在的差异出来。
        return "no per-key structural difference; only the shape folding changed \
                (for example a single-entry registry gained a second entry)"
            .to_string();
    }

    let extra = items.len().saturating_sub(MAX_DRIFT_ITEMS);
    items.truncate(MAX_DRIFT_ITEMS);
    let mut out = format!("structure changed: {}", items.join("; "));
    if extra > 0 {
        out.push_str(&format!("; … and {extra} more"));
    }
    out
}

/// 比对并（首次时）记录基准。
///
/// `slot` 是这份文件在 `meta` 表里的键，调用方用资源的稳定标识拼
/// （建议 `guard:<agent>:<kind>:<key>`）。`load` / `store` 由调用方注入，
/// 让本模块不依赖 duster-index —— 适配层不认识索引层，方向是向下的。
pub fn check(
    doc: &Doc,
    load: impl FnOnce() -> Result<Option<String>>,
    store: impl FnOnce(&str) -> Result<()>,
) -> Result<Verdict> {
    let actual = fingerprint(doc);
    match load()? {
        None => {
            store(&actual)?;
            Ok(Verdict::Learned)
        }
        Some(expected) if expected == actual => Ok(Verdict::Match),
        Some(expected) => Ok(Verdict::Drifted {
            expected,
            actual,
            // 只有指纹时给不出更多；调用方拿得到旧文档时应改用
            // describe_drift 填这一栏。
            reason: "the file's structure changed since duster last read it; \
                     refusing to write. Inspect the file and re-run `duster scan`"
                .to_string(),
        }),
    }
}

/// 递归深度上限。
///
/// 正常配置文件深度是个位数；能到 64 层的只有构造出来的畸形输入
/// （或者某个上游把树写成了自引用形状的展开）。栈溢出在 Rust 里是
/// abort，不是 `Err`，闸门自己把进程干掉就太荒唐了，所以硬性封顶。
const MAX_DEPTH: usize = 64;

/// 深度耗尽时代替子树的占位形状。
const TRUNCATED: &str = "…";

/// [`describe_drift`] 最多列几条。
const MAX_DRIFT_ITEMS: usize = 10;

/// JSON 子树的形状串。
fn shape_json(v: &serde_json::Value, depth: usize) -> String {
    if depth >= MAX_DEPTH {
        return TRUNCATED.to_string();
    }
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "bool".to_string(),
        // 整数与浮点同归 num：TOML 分这两类而 JSON 不分，若在这里分开，
        // 同一份配置写成 .json 与 .toml 会得到两个指纹，`mcp list` 就合不上了。
        serde_json::Value::Number(_) => "num".to_string(),
        serde_json::Value::String(_) => "str".to_string(),
        serde_json::Value::Array(items) => {
            fold_array(items.iter().map(|e| shape_json(e, depth + 1)).collect())
        }
        serde_json::Value::Object(map) => fold_object(
            map.iter()
                .map(|(k, e)| (k.as_str(), shape_json(e, depth + 1)))
                .collect(),
        ),
    }
}

/// TOML 子树的形状串。标签必须与 [`shape_json`] 完全一致。
fn shape_toml(v: &toml::Value, depth: usize) -> String {
    if depth >= MAX_DEPTH {
        return TRUNCATED.to_string();
    }
    match v {
        toml::Value::String(_) => "str".to_string(),
        // 同 shape_json：整数与浮点都是 num。
        toml::Value::Integer(_) | toml::Value::Float(_) => "num".to_string(),
        toml::Value::Boolean(_) => "bool".to_string(),
        // TOML 有原生日期时间，JSON 没有——同一个时间戳搬到 JSON 里只能是
        // 字符串。记成 str 才能让两种格式同构，否则格式转换本身就算漂移。
        toml::Value::Datetime(_) => "str".to_string(),
        toml::Value::Array(items) => {
            fold_array(items.iter().map(|e| shape_toml(e, depth + 1)).collect())
        }
        // TOML 没有 null，所以 "null" 这个标签只会从 JSON 侧出现；
        // 这不影响同构判定——JSON 里写了 null 的键在 TOML 里根本无法表达。
        toml::Value::Table(map) => fold_object(
            map.iter()
                .map(|(k, e)| (k.as_str(), shape_toml(e, depth + 1)))
                .collect(),
        ),
    }
}

/// 数组：元素形状去重排序后折成一个代表元 `[a|b]`；空数组是独立形状 `[]`。
///
/// 折叠是必须的——往 `args` 里多塞一个字符串不是结构变化。
fn fold_array(mut shapes: Vec<String>) -> String {
    shapes.sort_unstable();
    shapes.dedup();
    format!("[{}]", shapes.join("|"))
}

/// 对象：同形状注册表折成 `{*:S}`，否则按键名字典序列出 `{k1:s1,k2:s2}`。
/// 判据与代价见 [`fingerprint`] 的文档。
fn fold_object(mut entries: Vec<(&str, String)>) -> String {
    if entries.len() > 1 {
        let first = &entries[0].1;
        if entries.iter().all(|(_, s)| s == first) {
            return format!("{{*:{first}}}");
        }
    }
    // serde_json 开了 preserve_order，键序是文件里的书写顺序而非字典序，
    // 必须自己排，否则同一份配置换个键序就换个指纹。
    entries.sort_unstable_by_key(|(k, _)| *k);
    let body: Vec<String> = entries.iter().map(|(k, s)| format!("{k}:{s}")).collect();
    format!("{{{}}}", body.join(","))
}

/// 把文档摊平成「键路径 → 类型标签」，供 [`describe_drift`] 做集合差。
///
/// 容器自己也占一条（`obj` / `arr`），这样「对象整个变成数组」能被说出来。
/// 根路径是空串，展示时由 [`show`] 换成 `(root)`。
fn flatten(doc: &Doc) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    match doc {
        Doc::Json(v) => flatten_json(v, "", 0, &mut out),
        Doc::Toml(v) => flatten_toml(v, "", 0, &mut out),
    }
    out
}

fn flatten_json(
    v: &serde_json::Value,
    path: &str,
    depth: usize,
    out: &mut BTreeMap<String, String>,
) {
    if depth >= MAX_DEPTH {
        merge_tag(out, path, TRUNCATED);
        return;
    }
    match v {
        serde_json::Value::Object(map) => {
            merge_tag(out, path, "obj");
            for (k, child) in map {
                flatten_json(child, &join(path, k), depth + 1, out);
            }
        }
        serde_json::Value::Array(items) => {
            merge_tag(out, path, "arr");
            // 下标折成 `[]`：`args[3]` 对人没有信息量，而且多一个元素
            // 就多一条差异，会把封顶名额全占掉。
            let child_path = format!("{path}[]");
            for item in items {
                flatten_json(item, &child_path, depth + 1, out);
            }
        }
        leaf => merge_tag(out, path, &shape_json(leaf, MAX_DEPTH - 1)),
    }
}

fn flatten_toml(v: &toml::Value, path: &str, depth: usize, out: &mut BTreeMap<String, String>) {
    if depth >= MAX_DEPTH {
        merge_tag(out, path, TRUNCATED);
        return;
    }
    match v {
        toml::Value::Table(map) => {
            merge_tag(out, path, "obj");
            for (k, child) in map {
                flatten_toml(child, &join(path, k), depth + 1, out);
            }
        }
        toml::Value::Array(items) => {
            merge_tag(out, path, "arr");
            let child_path = format!("{path}[]");
            for item in items {
                flatten_toml(item, &child_path, depth + 1, out);
            }
        }
        leaf => merge_tag(out, path, &shape_toml(leaf, MAX_DEPTH - 1)),
    }
}

/// 记录一条「路径 → 标签」。同一路径被数组的多个元素命中时取并集
/// （`args[]` 里既有字符串又有对象 → `obj|str`），排序保证确定性。
fn merge_tag(out: &mut BTreeMap<String, String>, path: &str, tag: &str) {
    let merged = match out.get(path) {
        None => tag.to_string(),
        Some(existing) => {
            if existing.split('|').any(|p| p == tag) {
                return;
            }
            let mut parts: Vec<&str> = existing.split('|').chain(std::iter::once(tag)).collect();
            parts.sort_unstable();
            parts.join("|")
        }
    };
    out.insert(path.to_string(), merged);
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// 空路径就是文档根，给用户看要有个名字。
fn show(path: &str) -> &str {
    if path.is_empty() { "(root)" } else { path }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn json(src: &str) -> Doc {
        Doc::Json(serde_json::from_str(src).expect("测试 JSON 必须可解析"))
    }

    fn toml_doc(src: &str) -> Doc {
        Doc::Toml(src.parse().expect("测试 TOML 必须可解析"))
    }

    /// 两条同形状 server 的注册表；`{n}` 处塞第三条。
    const REGISTRY_2: &str = r#"{
      "mcpServers": {
        "alpha": { "command": "npx", "args": ["-y", "a"] },
        "beta":  { "command": "uvx", "args": ["b"] }
      }
    }"#;

    const REGISTRY_3: &str = r#"{
      "mcpServers": {
        "alpha": { "command": "npx", "args": ["-y", "a"] },
        "beta":  { "command": "uvx", "args": ["b"] },
        "gamma": { "command": "deno", "args": ["run", "c"] }
      }
    }"#;

    /// alpha 的 command 从字符串变成数组。
    const REGISTRY_3_ARR_COMMAND: &str = r#"{
      "mcpServers": {
        "alpha": { "command": ["npx", "-y"], "args": ["-y", "a"] },
        "beta":  { "command": "uvx", "args": ["b"] },
        "gamma": { "command": "deno", "args": ["run", "c"] }
      }
    }"#;

    #[test]
    fn 注册表多一条同形状条目指纹不变_条目类型变了才变() {
        let two = fingerprint(&json(REGISTRY_2));
        let three = fingerprint(&json(REGISTRY_3));
        assert_eq!(
            two, three,
            "加一条同形状的 server 不是结构变化，闸门不许因此叫唤"
        );

        let drifted = fingerprint(&json(REGISTRY_3_ARR_COMMAND));
        assert_ne!(
            three, drifted,
            "command 从 str 变 arr 是真漂移，必须被指纹抓到"
        );
    }

    #[test]
    fn 多包一层指纹就变() {
        let plain = fingerprint(&json(REGISTRY_2));
        let wrapped = fingerprint(&json(&format!(
            r#"{{ "version": 2, "config": {REGISTRY_2} }}"#
        )));
        assert_ne!(plain, wrapped, "整棵树被包进 config 里，路径全变了");
    }

    #[test]
    fn json_与_toml_同构则指纹相同() {
        let j = json(
            r#"{
              "mcp": {
                "servers": {
                  "a": { "command": "x", "args": ["1"], "enabled": true,  "timeout": 30 },
                  "b": { "command": "y", "args": ["2"], "enabled": false, "timeout": 60 }
                }
              }
            }"#,
        );
        let t = toml_doc(
            r#"
[mcp.servers.a]
command = "x"
args = ["1"]
enabled = true
timeout = 30

[mcp.servers.b]
command = "y"
args = ["2"]
enabled = false
timeout = 60
"#,
        );
        assert_eq!(
            fingerprint(&j),
            fingerprint(&t),
            "跨格式同构必须同指纹，否则 mcp list 合并不了同一台 server"
        );

        // TOML 的浮点也要归到 num，不能和 JSON 的整数分家。
        let t_float = toml_doc(
            r#"
[mcp.servers.a]
command = "x"
args = ["1"]
enabled = true
timeout = 30.5

[mcp.servers.b]
command = "y"
args = ["2"]
enabled = false
timeout = 60.0
"#,
        );
        assert_eq!(
            fingerprint(&j),
            fingerprint(&t_float),
            "float 与 int 同为 num"
        );

        // TOML 原生 datetime 折成 str，等价于 JSON 里的字符串时间戳。
        let dt = toml_doc("created = 1979-05-27T07:32:00Z\n");
        let s = json(r#"{ "created": "1979-05-27T07:32:00Z" }"#);
        assert_eq!(fingerprint(&dt), fingerprint(&s), "datetime 折成 str");
    }

    #[test]
    fn 只改值不改指纹() {
        let a = json(
            r#"{ "mcpServers": {
                   "alpha": { "command": "npx",  "args": ["-y", "a"], "port": 8080 },
                   "beta":  { "command": "uvx",  "args": ["b"],       "port": 9090 } } }"#,
        );
        let b = json(
            r#"{ "mcpServers": {
                   "alpha": { "command": "deno", "args": ["run"],     "port": 1234 },
                   "beta":  { "command": "bunx", "args": ["z"],       "port": 4321 } } }"#,
        );
        assert_eq!(
            fingerprint(&a),
            fingerprint(&b),
            "指纹只看形状；换命令、换端口都不该触发只读降级"
        );
    }

    #[test]
    fn 空数组是独立形状() {
        let empty = fingerprint(&json(r#"{ "args": [] }"#));
        let one = fingerprint(&json(r#"{ "args": ["x"] }"#));
        assert_ne!(empty, one, "`[]` 与 `[str]` 是两种形状");
    }

    #[test]
    fn check_依次给出_learned_match_drifted() {
        // 用 RefCell 冒充 meta 表：不碰真索引，也不碰真 $HOME。
        let slot: RefCell<Option<String>> = RefCell::new(None);
        let run = |doc: &Doc| {
            check(
                doc,
                || Ok(slot.borrow().clone()),
                |fp| {
                    *slot.borrow_mut() = Some(fp.to_string());
                    Ok(())
                },
            )
            .expect("测试用的闭包不会失败")
        };

        let learned = run(&json(REGISTRY_2));
        assert_eq!(learned, Verdict::Learned, "第一次见到，记基准");
        assert!(learned.allows_write());
        assert!(slot.borrow().is_some(), "基准必须真的写进了槽位");

        // 加一条同形状 server：形状没变，应当是 Match。
        let matched = run(&json(REGISTRY_3));
        assert_eq!(matched, Verdict::Match, "同形状，放行");
        assert!(matched.allows_write());

        let drifted = run(&json(REGISTRY_3_ARR_COMMAND));
        assert!(!drifted.allows_write(), "漂移必须降级只读");
        match drifted {
            Verdict::Drifted {
                expected,
                actual,
                reason,
            } => {
                assert_ne!(expected, actual);
                assert!(
                    reason.contains("refusing to write"),
                    "reason 要是人话，实际为 {reason}"
                );
            }
            other => panic!("期望 Drifted，实际 {other:?}"),
        }
    }

    #[test]
    fn describe_drift_说出键路径与类型变化方向() {
        let expected = json(
            r#"{ "mcpServers": {
                   "alpha": { "command": "npx", "args": ["-y"] },
                   "beta":  { "command": "uvx", "args": ["b"]  } } }"#,
        );
        let actual = json(
            r#"{ "mcpServers": {
                   "alpha": { "command": ["npx", "-y"], "args": ["-y"] },
                   "gamma": { "command": "deno", "args": ["c"] } } }"#,
        );
        let msg = describe_drift(&expected, &actual);

        assert!(
            msg.contains("mcpServers.alpha.command: str → arr"),
            "要说清哪条路径、从什么变成什么，实际为 {msg}"
        );
        assert!(
            msg.contains("added mcpServers.gamma"),
            "新增的键路径要点名，实际为 {msg}"
        );
        assert!(
            msg.contains("removed mcpServers.beta"),
            "消失的键路径要点名，实际为 {msg}"
        );
        assert!(!msg.contains("[3]"), "数组下标应折成 []，实际为 {msg}");
    }

    #[test]
    fn describe_drift_封顶十条() {
        let mut old = String::from("{");
        let mut new = String::from("{");
        for i in 0..25 {
            // 每个键的值形状都不同，避免注册表折叠把差异吃掉。
            old.push_str(&format!(r#""k{i}": {{ "a": "s", "b{i}": 1 }},"#));
            new.push_str(&format!(r#""k{i}": {{ "a": 1,   "b{i}": 1 }},"#));
        }
        old.push_str(r#""tail": true }"#);
        new.push_str(r#""tail": true }"#);

        let msg = describe_drift(&json(&old), &json(&new));
        assert!(
            msg.contains("… and 15 more"),
            "25 条差异只列 10 条，实际为 {msg}"
        );
        assert_eq!(msg.matches("; ").count(), 10, "10 条正文 + 1 条省略提示");
    }

    #[test]
    fn describe_drift_无逐键差异时照实说() {
        // 注册表从 1 条变 2 条：指纹会变（`{a:S}` → `{*:S}`），
        // 但摊平后每条路径的类型都对得上。
        let one = json(r#"{ "mcpServers": { "alpha": { "command": "npx" } } }"#);
        let same = json(r#"{ "mcpServers": { "alpha": { "command": "npx" } } }"#);
        let msg = describe_drift(&one, &same);
        assert!(
            msg.starts_with("no per-key structural difference"),
            "实际为 {msg}"
        );
    }

    #[test]
    fn 两百层嵌套不爆栈且在深度上限处截断() {
        fn nest(levels: usize, leaf: serde_json::Value) -> serde_json::Value {
            let mut v = leaf;
            for _ in 0..levels {
                v = serde_json::json!({ "n": v });
            }
            v
        }

        let deep = fingerprint(&Doc::Json(nest(200, serde_json::json!("leaf"))));
        assert_eq!(deep.len(), 16, "无论多深都要正常返回一个指纹");

        // 只在上限以下有差别的两棵树指纹相同 —— 证明确实截断了，
        // 而不是碰巧没递归到那么深。
        let other = fingerprint(&Doc::Json(nest(200, serde_json::json!(42))));
        assert_eq!(deep, other, "第 200 层的叶子类型早被 MAX_DEPTH 挡住了");

        // 上限以内的差别照样要抓到。
        let shallow_str = fingerprint(&Doc::Json(nest(3, serde_json::json!("leaf"))));
        let shallow_num = fingerprint(&Doc::Json(nest(3, serde_json::json!(42))));
        assert_ne!(shallow_str, shallow_num, "浅层的类型变化不许漏");

        // describe_drift 走的是另一条递归，同样不许爆栈。
        let msg = describe_drift(
            &Doc::Json(nest(200, serde_json::json!("leaf"))),
            &Doc::Json(nest(200, serde_json::json!(42))),
        );
        assert!(!msg.is_empty());
    }
}
