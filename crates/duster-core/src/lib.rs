//! 用例层:把下层能力编排为面向用户的动作。
//!
//! CLI 与未来的 GUI(UniFFI)都只调用本 crate,自身零业务逻辑。

pub mod clean;
pub mod diff;
pub mod doctor;
pub mod mcp;
pub mod memory;
pub mod plan;
pub mod prune;
pub mod scan;
pub mod search;
pub mod session;
pub mod skill_ops;
pub mod status;
pub mod uninstall;
