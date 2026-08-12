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

/// 一次技能调用事件。由三个会话解析器在行走文件时顺手提取(绝不二次遍历),
/// 索引层落库成 `skill_event` 表,prune 据此判定技能陈旧。
///
/// 与 [`TurnRecord`] 的区别就是本类型的全部意义:轮次是**对话正文**,事件是
/// **工具调用记录**——只有后者能证明「人真的调用了这个技能」,正文里提到
/// 技能名只说明被引用,不说明被调用(旧实现的名字全文检索就是死在这上面)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInvocation {
    /// 调用时记录的名字:claude 取 `input.skill`,omp 取 `skill://<名>`,
    /// codex 取 `/skills/<名>/SKILL.md`。可能是目录名而不是声明名
    /// (7 份真实副本两者不同),查询层两个都要匹配。
    pub skill: String,
    /// 事件时间戳(Unix 毫秒)。记录自身没有时间戳时退回会话内最后已知
    /// 时间戳,绝不发明 `now`——一个编出来的新日期会把死技能永远救活。
    pub ts_ms: i64,
}

/// 回读时抽取出的单轮正文。与 [`TurnRecord`] 不同,这是**读路径**的产物:
/// 索引只存字节区间,正文永远从源文件回读,回读之后还得再抽一遍文本——
/// 否则 `session show` / `open` / 搜索摘要 / 导出都会把原始 JSONL 行印给用户。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnBody {
    /// 对话正文。抽取失败时是原始行(见 raw_fallback)——宁可丑,不能假装这轮没内容。
    pub text: String,
    /// 工具轮的工具名;行里带就给,不带是 None(Claude 的 tool_result 不带名字)。
    pub tool: Option<String>,
    /// true = text 是原始行兜底。
    pub raw_fallback: bool,
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
