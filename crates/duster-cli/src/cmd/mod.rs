//! M2 起新增的命令组。
//!
//! `main.rs` 已经两千行、装着全部渲染器，再往里塞五个命令组就没人读得动了。
//! 所以新组一组一个文件：clap 子命令枚举、`run`、渲染器都在自己的文件里，
//! `main.rs` 只留三处接线（`mod cmd;` / `enum Command` 一条 / 分发一条）。
//!
//! 已有命令（`scan`/`status`/`search`/`open`/`clean`/`prune`/`uninstall`/
//! `skill`/`doctor`）留在 `main.rs` 不动——为了排版一致去搬一遍它们，
//! 改的是历史而不是这次的需求。

pub mod diff;
pub mod mcp;
pub mod memory;
pub mod session;
