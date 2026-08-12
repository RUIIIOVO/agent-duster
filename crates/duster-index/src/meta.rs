//! `meta` 键值表读写：跨扫描保留的全库级小状态。
//!
//! 目前只有一个键（[`PARSER_EPOCH`]）。刻意保持 API 极窄——这张表是给
//! 「一两个标量」用的，任何有结构的数据都应该建自己的表。

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// 解析规则指纹的键名。
///
/// 值由 duster-core 计算（内置/用户清单源文本 + 原生解析器纪元的哈希）。
/// 与库内不一致即说明「同一份源文件这次会解析出不同结果」，scan 据此
/// 自动转全量重解析。
pub const PARSER_EPOCH: &str = "parser_epoch";

/// 技能调用证据「已采集」标记的键名。
///
/// 值恒为 `"1"`，由 scan 在**有会话被（重）解析**的那一轮结尾写入。
/// 存在与否就是全部语义：旧索引升级而来、尚未重扫时 `skill_event` 是
/// 空表，空表会读成「从没调用过」，把整机的 skill 全扫进删除计划——
/// 「没查过」和「查过、没有」是两个事实，plan 层靠这个键区分（见
/// `duster_core::plan` 的 skill 分支）。
pub const SKILL_EVIDENCE_READY: &str = "skill_evidence_ready";

/// 读一个键；不存在返回 `None`。
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        r.get(0)
    })
    .optional()
    .with_context(|| format!("failed to read meta key `{key}`"))
}

/// 写一个键（存在即覆盖）。
pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .with_context(|| format!("failed to write meta key `{key}`"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    /// 缺失键读出 None；写入后可回读；再写同键为覆盖而非报错。
    #[test]
    fn meta_读写与覆盖() {
        let conn = open_db();
        assert_eq!(get(&conn, PARSER_EPOCH).unwrap(), None);

        set(&conn, PARSER_EPOCH, "abc").unwrap();
        assert_eq!(get(&conn, PARSER_EPOCH).unwrap().as_deref(), Some("abc"));

        set(&conn, PARSER_EPOCH, "def").unwrap();
        assert_eq!(get(&conn, PARSER_EPOCH).unwrap().as_deref(), Some("def"));

        // 只有一行:覆盖不是追加。
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
