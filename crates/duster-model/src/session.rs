//! 会话与轮次。索引只存字节偏移,正文永远从源文件流式回读。

use serde::{Deserialize, Serialize};

/// 轮次角色。上游千奇百怪的 type 值由各原生适配器白名单归一到这三类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// 工具调用/结果等非对话正文(默认不入全文索引)。
    Tool,
}

/// 会话文件中的一个真实对话轮次。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    /// 会话内序号,0 起。
    pub seq: u32,
    pub role: Role,
    /// Unix 毫秒;上游缺失时为 None。
    #[serde(default)]
    pub ts_ms: Option<i64>,
    /// 该轮次**可检索正文**在源文件中的字节区间(不是整行 JSON 的区间)。
    pub byte_off: u64,
    pub byte_len: u64,
    /// 提取出的正文文本(入 FTS 用;超长截断由索引层负责)。
    pub text: String,
}

/// 会话级元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// 会话发生的项目目录(还原自 jsonl 内 cwd 字段,路径编码不可逆不靠猜)。
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub turn_count: u32,
}
