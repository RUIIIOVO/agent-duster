//! 索引层:`~/.agent-duster/index.db`(SQLite,WAL 模式)。
//!
//! 索引是**可丢弃的派生物**——随时可删,重扫即重建,绝不是唯一数据源。
//! 全文检索使用 FTS5 `trigram` 分词器以保证 CJK 子串可搜。

pub mod db;
pub mod meta;
pub mod schema;
pub mod search;
pub mod sqlite_probe;
pub mod upsert;
