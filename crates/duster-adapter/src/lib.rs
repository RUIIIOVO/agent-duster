//! 适配层。
//!
//! 「支持一个 agent」拆成四个正交部件:
//! - **Probe**:装了吗?什么版本?
//! - **Locator**:资源文件在哪?
//! - **Codec**:文件怎么读写(纯格式,与 agent 无关;保守回写)。
//! - **Mapper**:格式树 <-> UARM。
//!
//! 主路径是声明式 TOML 清单(`adapters/*.toml`,运行时加载,加 agent 不改代码);
//! 清单表达不了的(SQLite 会话库等)走原生适配器逃生舱。
//! 本 crate 不认识用例层——依赖方向严格向下。

pub mod codec;
pub mod guard;
pub mod manifest;
pub mod mapper;
pub mod native;
pub mod probe;
pub mod registry;
