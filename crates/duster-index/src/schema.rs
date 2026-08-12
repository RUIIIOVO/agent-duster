//! SQLite schema 定义与迁移。表：agent / resource / turn / fts_turn（contentless）。
//!
//! 迁移由 `PRAGMA user_version` 驱动：每个版本一段 DDL，包在事务里执行，
//! 执行成功后把 `user_version` 提升到对应版本号。重复调用天然幂等。

use anyhow::{Context, Result};
use rusqlite::Connection;

/// 当前 schema 版本。新增迁移时递增，并在 [`migrate`] 中追加对应分支。
pub const SCHEMA_VERSION: i64 = 6;

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

/// v2：`meta` 键值表。
///
/// 存跨扫描保留的小状态，目前只有 `parser_epoch`（解析规则指纹，见
/// duster-core 的 scan）。刻意做成 key/value 而不是加列：这类状态是全库级的、
/// 数量少、schema 会随功能长，键值表免掉后续每加一项就来一次迁移。
const V2_DDL: &str = "
CREATE TABLE meta(
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

/// v3：`resource.clean_level` + `resource.reclaimable`。
///
/// - `clean_level`：清理级别是清单声明的属性，但 clean 的计划生成是纯读
///   索引的（不重跑 probe、不重解析清单），所以级别必须随资源行一起落库。
///   可空：只有 `kind = 'artifact'` 的行有值，其余 kind 恒为 NULL——
///   "有没有值"本身就是"能不能清"的判据，不需要再回查清单。
/// - `reclaimable`：**清掉这一项真正能拿回多少字节**，与 `size`（这一项
///   占了多少）是两个数。l1/l2 两者相等（整个删掉）；l0 只回收空洞，
///   例如一个 784 MB 的 SQLite 日志库里可能只有 774 MB 是空闲页，
///   VACUUM 之后剩下的 10 MB 真实数据还在。把 `size` 当回收量报给用户
///   就是虚报。同样只有 artifact 行有值。
///
/// 刻意不建索引：唯一的读方（status 的分级汇总）带 `agent_id` 过滤，
/// 规划器实测选的是 `UNIQUE(agent_id, kind, scope, key)` 自动索引。
/// 等 M1 的全局 clean 查询真的出现，再连同它的实测一起加。
const V3_DDL: &str = "
ALTER TABLE resource ADD COLUMN clean_level TEXT;
ALTER TABLE resource ADD COLUMN reclaimable INTEGER;
";

/// v4：`resource.install_bytes`。
///
/// artifact/install 的二分法在**资源内部**是漏的：`~/.claude/skills` 整体声明为
/// `kind = skill`，但 1.1 GB 里 721 MB 是 `node_modules`、301 MB 是 `dist/` `bin/`
/// 编译产物——它们符合 install 的判据（删了要重装），却因为长在 skill 目录里
/// 而被算进了用户内容。这一列把那部分字节从 `size` 里剥出来单独记账：
/// `size` 只剩真·用户内容，`install_bytes` 归入 install 桶。
///
/// 只有 `kind = 'skill'` 且清单声明了 `install_paths` 的行有值，其余恒为 NULL。
/// 归档（打包时排除）与副本检测（树哈希时剪枝）也读同一份声明，三处口径一致。
const V4_DDL: &str = "
ALTER TABLE resource ADD COLUMN install_bytes INTEGER;
";

/// v5：`resource.keep_generations`。
///
/// 清单声明的「保留最新 N 份代际」（数据库滚动备份等）是生成 prune 计划的
/// 依据，而 prune 的计划生成是纯读索引的（不重跑 probe、不重解析清单，
/// 见 v3 注释里同一条铁律），所以 N 必须随资源行一起落库。
///
/// 只有带 `glob` 的 stats-only 行有值（每行一份代际，N 由声明它的那条
/// `[[resource]]` 决定），其余行恒为 NULL——"有没有值"本身就是"这份资源
/// 是不是代际资源"的判据。写入走 `upsert::set_keep_generations`（独立
/// UPDATE 而不是加进 `ResourceRow`：32 个构造点只有一个字段需要这个值，
/// 为一个字段给整条行加字段是让全库为局部需求买单）。
const V5_DDL: &str = "
ALTER TABLE resource ADD COLUMN keep_generations INTEGER;
";

/// v6：`skill_event` 调用事件表。
///
/// prune 判定 skill 是否陈旧的**唯一**使用证据：三个会话解析器在行走文件时
/// 顺手提取真实工具调用（claude 的 `tool_use` / omp 的 `skill://` 参数 /
/// codex 的 `/skills/<名>/SKILL.md` 路径），按资源行落在这里。正文提到技能名
/// 不算——那是引用不是调用。
///
/// 写路径按 rid 整体替换（重解析一个会话就重建该会话的事件，见
/// `upsert::replace_skill_events`），所以不需要唯一约束；`rid` 外键级联保证
/// 会话行被 stale 清理删掉时事件跟着消失。`skill` 索引服务 `MAX(ts_ms)`
/// 查询（prune 判定「最后一次调用」）。
const V6_DDL: &str = "
CREATE TABLE skill_event(
  eid   INTEGER PRIMARY KEY,
  rid   INTEGER REFERENCES resource(rid) ON DELETE CASCADE,
  skill TEXT NOT NULL,
  ts_ms INTEGER NOT NULL
);
CREATE INDEX ix_skill_event_name ON skill_event(skill);
";

/// 把 `conn` 上的 schema 迁移到 [`SCHEMA_VERSION`]。幂等，可放心重复调用。
pub fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("failed to read user_version")?;

    if current < 1 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V1_DDL}\nPRAGMA user_version = 1;\nCOMMIT;"
        ))
        .context("failed to apply schema v1")?;
    }

    if current < 2 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V2_DDL}\nPRAGMA user_version = 2;\nCOMMIT;"
        ))
        .context("failed to apply schema v2")?;
    }

    if current < 3 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V3_DDL}\nPRAGMA user_version = 3;\nCOMMIT;"
        ))
        .context("failed to apply schema v3")?;
    }

    if current < 4 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V4_DDL}\nPRAGMA user_version = 4;\nCOMMIT;"
        ))
        .context("failed to apply schema v4")?;
    }

    if current < 5 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V5_DDL}\nPRAGMA user_version = 5;\nCOMMIT;"
        ))
        .context("failed to apply schema v5")?;
    }

    if current < 6 {
        conn.execute_batch(&format!(
            "BEGIN;\n{V6_DDL}\nPRAGMA user_version = 6;\nCOMMIT;"
        ))
        .context("failed to apply schema v6")?;
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

        // 五张表都在（skill_event 是 v6 加的调用事件表）。
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE name IN ('agent', 'resource', 'turn', 'fts_turn', 'skill_event')
                   AND type IN ('table')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 5);
    }

    /// 老库(v2)必须能就地升到最新版并保住既有数据。
    /// 新装用户走的是全量 DDL 路径，只有这条能守住升级路径。
    #[test]
    fn v2_database_upgrades_in_place_to_latest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "BEGIN;\n{V1_DDL}\n{V2_DDL}\nPRAGMA user_version = 2;\nCOMMIT;"
        ))
        .unwrap();
        conn.execute(
            "INSERT INTO resource(agent_id, kind, scope, key, path, size, mtime_ns)
             VALUES ('codex', 'artifact', 'global', '~/.codex/cache', '/x', 42, 0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // 既有行还在，v3/v4 新增列都存在且为 NULL（下次 scan 才会填上）。
        let (size, level, install): (i64, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT size, clean_level, install_bytes FROM resource WHERE key = '~/.codex/cache'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(size, 42);
        assert_eq!(level, None);
        assert_eq!(install, None);
    }
}
