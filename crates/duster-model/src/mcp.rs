//! MCP server 的归一化描述——所有方言（standard-json / codex-toml）都映射到这里。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 传输方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// 本地子进程,stdio 通信(command + args + env)。
    Stdio,
    /// HTTP / streamable-http。
    Http,
    /// Server-Sent Events。
    Sse,
}

/// 归一化 MCP server 声明。
///
/// 语义哈希([`Self::content_hash`])只覆盖影响行为的字段,用于跨 agent 去重:
/// 同一个 server 在 Claude 和 Codex 各声明一次,应合并为一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// 声明名(agent 配置里的 key)。不参与语义哈希——同一 server 可有不同别名。
    pub name: String,
    pub transport: McpTransport,
    /// Stdio: 启动命令。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// BTreeMap 保证哈希稳定(键序确定)。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Http/Sse: 端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Http/Sse: 请求头。值可能含凭据,展示层必须掩码。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// 未被归一化模型覆盖的原始字段(JSON 序列化),迁移时原样带走以避免有损。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

impl McpServerSpec {
    /// 语义哈希:transport/command/args/env/url/headers 参与,name 不参与。
    pub fn content_hash(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        let tag = match self.transport {
            McpTransport::Stdio => b"stdio",
            McpTransport::Http => b"http\0",
            McpTransport::Sse => b"sse\0\0",
        };
        h.update(tag);
        let mut feed = |s: &str| {
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        };
        feed(self.command.as_deref().unwrap_or(""));
        for a in &self.args {
            feed(a);
        }
        for (k, v) in &self.env {
            feed(k);
            feed(v);
        }
        feed(self.url.as_deref().unwrap_or(""));
        for (k, v) in &self.headers {
            feed(k);
            feed(v);
        }
        h.finalize()
    }
}
