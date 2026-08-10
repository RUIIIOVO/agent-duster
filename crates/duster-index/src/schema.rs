//! SQLite schema 定义与迁移。表：agent / resource / turn / fts_turn（contentless）。
//!
//! 迁移由 `PRAGMA user_version` 驱动：每个版本一段 DDL，包在事务里执行，
//! 执行成功后把 `user_version` 提升到对应版本号。重复调用天然幂等。

use anyhow::{Context, Result};
use rusqlite::Connection;

/// 当前 schema 版本。新增迁移时递增，并在 [`migrate`] 中追加对应分支。
pub const SCHEMA_VERSION: i64 = 1;

/// v1 全量 DDL。
///
/// - `resource` 上的 `UNIQUE(agent_id, kind, scope, key)` 与
///   `duster_model::ResourceId` 一一对应，是 upsert 的天然冲突键。
/// - `ix_res_hash` 是部分索引（hash 非空才进），供内容去重反查。
/// - `fts_turn` 用 FTS5 contentless（`content=''`）+ `trigram` 分词，
///   保证 CJK 子串可搜；正文不落库，rowid 与 `turn.tid` 对齐由写入方负责。
///   `contentless_delete=1`（SQLite ≥ 3.43，bundled 满足）使 contentless 表
///   支持 DELETE，供 upsert 层重建/清理时同步删除 FTS 行。
const V1_DDL: &str = "
CREATE TABLE agent(
  agent_id     TEXT PRIMARY KEY,
  display_name TEXT,
  root         TEXT,
  version      TEXT,
  last_scan_ms INTEGER
);

CREATE TABLE resource(
  rid          INTEGER PRIMARY KEY,
  agent_id     TEXT NOT NULL,
  kind         TEXT NOT NULL,
  scope        TEXT NOT NULL,
  key          TEXT NOT NULL,
  path         TEXT NOT NULL,
  size         INTEGER NOT NULL,
  mtime_ns     INTEGER NOT NULL,
  hash_content BLOB,
  cheap_print  BLOB,
  UNIQUE(agent_id, kind, scope, key)
);
CREATE INDEX ix_res_hash ON resource(hash_content) WHERE hash_content IS NOT NULL;
CREATE INDEX ix_res_kind_sz ON resource(kind, size DESC);

CREATE TABLE turn(
  tid      INTEGER PRIMARY KEY,
  rid      INTEGER REFERENCES resource(rid) ON DELETE CASCADE,
  seq      INTEGER,
  role     TEXT,
  ts_ms    INTEGER,
  byte_off INTEGER,
  byte_len INTEGER
);

CREATE VIRTUAL TABLE fts_turn USING fts5(
  body,
  tid UNINDEXED,
  content='',
  contentless_delete=1,
  tokenize='trigram'
);
";

/// 把 `conn` 上的 schema 迁移到 [`SCHEMA_VERSION`]。幂等，可放心重复调用。
pub fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("读取 user_version 失败")?;

    if current < 1 {
        conn.execute_batch(&format!("BEGIN;\n{V1_DDL}\nPRAGMA user_version = 1;\nCOMMIT;"))
            .context("应用 schema v1 失败")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟（第一优先级）：bundled SQLite 必须支持 FTS5 + trigram，
    /// 且中文**子串**能被 MATCH 命中——这是整个会话检索功能的前提。
    #[test]
    fn fts5_trigram_matches_cjk_substring() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute(
            "INSERT INTO fts_turn(rowid, body, tid) VALUES (1, ?1, 42)",
            ["这是一段用于验证全文检索的中文会话文本"],
        )
        .unwrap();

        // trigram 分词要求查询串 >= 3 个字符；取正文中间的子串验证。
        let hit: i64 = conn
            .query_row(
                "SELECT rowid FROM fts_turn WHERE fts_turn MATCH '全文检索'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hit, 1);

        // 反例：不存在的子串不得命中。
        use rusqlite::OptionalExtension;
        let miss: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM fts_turn WHERE fts_turn MATCH '不存在的词'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(miss.is_none());
    }

    /// migrate 幂等：连跑两次不报错，表结构完整可用。
    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // 四张表都在。
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE name IN ('agent', 'resource', 'turn', 'fts_turn')
                   AND type IN ('table')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 4);
    }
}
