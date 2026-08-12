//! 原生适配器逃生舱:声明式清单表达不了的部分(session jsonl / SQLite)。

use duster_model::TurnBody;

pub mod claude_session;
pub mod codex_session;
pub mod jsonl_common;
pub mod omp_jsonl_session;
pub mod omp_session;
pub mod opencode_session;

/// 按 agent_id 分派单行正文抽取。名单外的 agent → None,调用方兜底回原始行。
///
/// 用 agent_id 而不是 mapper 名分派,与 core 侧 `read_db_turn` 同一个原因:
/// `resource` 表不存 mapper(那是清单的属性,不是资源的),回读路径上只有
/// agent_id 能决定这一行该按哪家的语法解析。「哪个 agent 的会话长什么样」
/// 是一份封闭的短名单,名单外的 agent 由调用方把原始区间当正文兜底。
pub fn body_of_line(agent_id: &str, raw: &str) -> Option<TurnBody> {
    match agent_id {
        "claude-code" => claude_session::body_of_line(raw),
        "codex" => codex_session::body_of_line(raw),
        "omp" => omp_jsonl_session::body_of_line(raw),
        _ => None,
    }
}
