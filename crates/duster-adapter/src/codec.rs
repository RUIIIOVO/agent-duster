//! Codec:纯格式读写,与 agent 无关。M0 只做读方向(json / toml)。
//!
//! 职责边界:只认识文件格式,不认识任何 agent 的目录约定。
//! 读取一律经 [`read_to_string_capped`] 防御超大文件。

use std::fs;
use std::path::Path;

use anyhow::{Context, bail};

/// 解析后的配置文档。按格式分列,不做 trait object 抹平——
/// 调用方(mapper)本来就要区分格式做不同下钻。
#[derive(Debug, Clone)]
pub enum Doc {
    /// JSON 文档(serde_json 已开 preserve_order,键序天然保留)。
    Json(serde_json::Value),
    /// TOML 文档。
    Toml(toml::Value),
}

/// 无扩展名嗅探时的默认大小上限(8 MiB),足够覆盖任何正常配置文件。
const DEFAULT_CAP: u64 = 8 * 1024 * 1024;

/// 读取并解析一个配置文件。
///
/// 按扩展名分派:`.json` -> JSON,`.toml` -> TOML;
/// 无扩展名或不认识的扩展名 -> 先试 JSON 再试 TOML,
/// 两者都失败时把两个解析错误合并成一条人话报错。
pub fn read_file(path: &Path) -> anyhow::Result<Doc> {
    let text = read_to_string_capped(path, DEFAULT_CAP)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("json") => {
            let v = serde_json::from_str(&text)
                .with_context(|| format!("failed to parse JSON: {}", path.display()))?;
            Ok(Doc::Json(v))
        }
        Some("toml") => {
            let v = toml::from_str(&text)
                .with_context(|| format!("failed to parse TOML: {}", path.display()))?;
            Ok(Doc::Toml(v))
        }
        _ => {
            // 嗅探:先 JSON 后 TOML。
            let json_err = match serde_json::from_str(&text) {
                Ok(v) => return Ok(Doc::Json(v)),
                Err(e) => e,
            };
            let toml_err = match toml::from_str(&text) {
                Ok(v) => return Ok(Doc::Toml(v)),
                Err(e) => e,
            };
            bail!(
                "unrecognized format for {}: JSON parse failed ({json_err}); TOML parse failed ({toml_err})",
                path.display()
            );
        }
    }
}

/// JSON Pointer 下钻(RFC 6901),直接复用 `Value::pointer`。
///
/// 例:`/mcpServers/foo/command`;空指针 `""` 返回整棵树。
pub fn json_pointer<'a>(v: &'a serde_json::Value, ptr: &str) -> Option<&'a serde_json::Value> {
    v.pointer(ptr)
}

/// TOML 点路径下钻,例 `mcp.servers.foo`。
///
/// 限制:按 `.` 简单切分,**不支持**带引号的段(如 `a."b.c".d`)——
/// 键名本身含 `.` 时无法表达。数组段按十进制下标解释(如 `deps.0.name`)。
/// 空路径返回整棵树。
pub fn toml_path<'a>(v: &'a toml::Value, dotted: &str) -> Option<&'a toml::Value> {
    if dotted.is_empty() {
        return Some(v);
    }
    let mut cur = v;
    for seg in dotted.split('.') {
        cur = match cur {
            toml::Value::Table(t) => t.get(seg)?,
            toml::Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// 带大小上限的文本读取。超限直接报错并说明实际大小,防止把
/// 误认成配置的巨型文件整个吸进内存。
pub fn read_to_string_capped(path: &Path, cap: u64) -> anyhow::Result<String> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read metadata: {}", path.display()))?;
    if meta.len() > cap {
        bail!(
            "file too large, refusing to read: {} is {} bytes, cap is {} bytes",
            path.display(),
            meta.len(),
            cap
        );
    }
    fs::read_to_string(path).with_context(|| format!("failed to read file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const JSON_FIXTURE: &str = r#"{"mcpServers":{"foo":{"command":"npx","args":["-y","foo"]}}}"#;
    const TOML_FIXTURE: &str = "[mcp.servers.foo]\ncommand = \"npx\"\nargs = [\"-y\", \"foo\"]\n";

    fn write_tmp(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn read_file_dispatches_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let j = write_tmp(&dir, "cfg.json", JSON_FIXTURE);
        let t = write_tmp(&dir, "cfg.toml", TOML_FIXTURE);
        assert!(matches!(read_file(&j).unwrap(), Doc::Json(_)));
        assert!(matches!(read_file(&t).unwrap(), Doc::Toml(_)));
    }

    #[test]
    fn read_file_sniffs_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        // 无扩展名的 JSON 内容 -> 嗅探成 JSON。
        let j = write_tmp(&dir, "config", JSON_FIXTURE);
        assert!(matches!(read_file(&j).unwrap(), Doc::Json(_)));
        // 无扩展名的 TOML 内容(非法 JSON)-> 落到 TOML。
        let t = write_tmp(&dir, "settings", TOML_FIXTURE);
        assert!(matches!(read_file(&t).unwrap(), Doc::Toml(_)));
        // 两者都不是 -> 报错里同时提到两种格式的失败。
        let bad = write_tmp(&dir, "garbage", "{{{ not = valid [");
        let msg = read_file(&bad).unwrap_err().to_string();
        assert!(msg.contains("JSON"), "报错应包含 JSON 失败信息: {msg}");
        assert!(msg.contains("TOML"), "报错应包含 TOML 失败信息: {msg}");
    }

    #[test]
    fn json_pointer_hit_and_miss() {
        let v: serde_json::Value = serde_json::from_str(JSON_FIXTURE).unwrap();
        assert_eq!(
            json_pointer(&v, "/mcpServers/foo/command").and_then(|x| x.as_str()),
            Some("npx")
        );
        assert!(json_pointer(&v, "/mcpServers/bar").is_none());
        // 空指针返回整棵树。
        assert!(json_pointer(&v, "").is_some());
    }

    #[test]
    fn toml_path_hit_and_miss() {
        let v: toml::Value = toml::from_str(TOML_FIXTURE).unwrap();
        assert_eq!(
            toml_path(&v, "mcp.servers.foo.command").and_then(|x| x.as_str()),
            Some("npx")
        );
        // 数组下标下钻。
        assert_eq!(
            toml_path(&v, "mcp.servers.foo.args.1").and_then(|x| x.as_str()),
            Some("foo")
        );
        assert!(toml_path(&v, "mcp.servers.bar").is_none());
        assert!(toml_path(&v, "mcp.servers.foo.args.9").is_none());
        // 空路径返回整棵树。
        assert!(toml_path(&v, "").is_some());
    }

    #[test]
    fn capped_read_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "big.json", "0123456789");
        let msg = read_to_string_capped(&p, 4).unwrap_err().to_string();
        assert!(msg.contains("10"), "报错应说明实际大小: {msg}");
        // 上限内正常读取。
        assert_eq!(read_to_string_capped(&p, 10).unwrap(), "0123456789");
    }
}
