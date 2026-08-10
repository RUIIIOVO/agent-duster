//! UARM —— 统一资源模型(Unified Agent Resource Model)。
//!
//! 本 crate 是整个 workspace 的地基:只定义类型与错误,**零 I/O、零外部副作用**。
//! 依赖方向严格向下,此 crate 不依赖任何其他 duster crate。

pub mod agent;
pub mod mcp;
pub mod resource;
pub mod session;
pub mod skill;

pub use agent::AgentInfo;
pub use mcp::{McpServerSpec, McpTransport};
pub use resource::{AgentId, Resource, ResourceId, ResourceKind, Scope};
pub use session::{Role, SessionMeta, TurnRecord};
pub use skill::SkillMeta;

/// 全 workspace 共享的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported resource kind: {0}")]
    UnsupportedKind(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
