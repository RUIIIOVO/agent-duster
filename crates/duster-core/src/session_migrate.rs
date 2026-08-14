//! `duster session migrate`:把一场会话的**纯文本**移植(plant)到另一个 agent。
//!
//! # 有损是契约,不是缺陷
//!
//! 只带走 user / assistant 的对话散文;thinking、tool_use、tool_result 一律丢弃,
//! **连续**被丢的一段压成一行 assistant 文本 [`TOOL_OMITTED`]。为什么不做全保真
//! 转换:三家的工具轮语义互不兼容——Claude 的 `tool_use`/`tool_result` 必须成对、
//! Codex 的 `function_call` 有自己的 call_id 空间、omp 的 `toolResult` 挂在
//! parentId 链上。硬翻等于在目标 agent 里伪造一段它自己从没执行过的工具历史,
//! 目标 agent 拿这种假历史续聊会去引用不存在的调用 id。文本是三家共同的
//! 最大公约数:诚实的损失好过体面的伪造。
//!
//! # 只支持 claude-code / codex / omp 互迁
//!
//! 写入端必须产出**目标 agent 自己读得回去**的文件,这只对三家有原生解析器的
//! agent 成立(见 [`duster_adapter::native`])。每次写完都用自家解析器
//! round-trip 对账,验不过就删掉半成品报错——绝不静默产出一个 scan 得到、
//! 读不出正文的文件(见 [`verify_roundtrip`])。名单外的 agent(无论作为
//! 来源还是目标)一律 `agent <x> sessions are stats-only, cannot migrate`。
//!
//! # 安全边界
//!
//! 只往目标 agent 的会话目录写**一个新文件**,别的一个字节不碰。特别是
//! claude-code:**不写 `~/.claude.json` 的 trust 条目**——「信任该目录」是
//! 用户与 Claude 之间的授权动作(首次在该项目启动时 Claude 会问一次),
//! duster 替用户签这个字等于绕过那次安全询问。代价只是移植过去的会话
//! 首次打开时多一次确认,值得。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;

use duster_model::Role;

use crate::session::{self, SessionDetail};

/// 压缩工具活动的占位行。逐字契约:README、TUI 文案、测试引用的都是这一句。
pub const TOOL_OMITTED: &str = "[tool activity omitted]";

/// 迁移目标的封闭词汇表。名单外的 agent 由 [`migrate`] 统一报错——
/// 来源端与目标端同一句话,用户不用学两种失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Claude,
    Codex,
    Omp,
}

impl Target {
    fn of(agent_id: &str) -> Option<Self> {
        match agent_id {
            "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "omp" => Some(Self::Omp),
            _ => None,
        }
    }
}

/// `duster session migrate` 的输入。
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// 会话 id(`duster session list` 的 ID 列)。
    pub rid: i64,
    /// 目标 agent id:claude-code / codex / omp。
    pub to: String,
    /// 只出报告不落盘。
    pub dry_run: bool,
    /// home 注入口,测试用;None = 真实用户主目录。
    pub home: Option<PathBuf>,
    /// 当前时间(Unix 毫秒)注入口,测试用;None = 系统时钟。
    /// 它落到三处:溯源行的日期、目标文件名的时间段、无时间戳轮次的兜底时间。
    pub now_ms: Option<i64>,
}

/// 迁移回执。`--json` 时整个对象进信封;dry-run 时 `target` 是「将要写到哪」。
#[derive(Debug, Clone, Serialize)]
pub struct MigrateReport {
    pub rid: i64,
    pub from: String,
    pub to: String,
    /// 目标文件绝对路径。
    pub target: String,
    /// 落进目标文件的对话记录数(user + assistant,含压缩出来的占位行)。
    pub turns_planted: usize,
    /// 被丢弃的源轮次数(工具轮 + 抽不出散文的 raw 兜底轮)。
    pub tool_turns_dropped: usize,
    pub dry_run: bool,
}

/// 计划里的一条:移植后只剩 user / assistant 两种角色的纯文本。
struct PlannedTurn {
    user: bool,
    text: String,
    ts_ms: Option<i64>,
}

/// 把一场会话移植到另一个 agent。读走 [`session::show`](与 `show`/`export`
/// 同一条回读路径),写走 `duster_fs::atomic::write_atomic`(半截会话比没有
/// 会话更糟,它看起来像一份完整记录)。
pub fn migrate(index_path: Option<&Path>, opts: &MigrateOptions) -> Result<MigrateReport> {
    let Some(target) = Target::of(&opts.to) else {
        bail!("agent {} sessions are stats-only, cannot migrate", opts.to);
    };

    let detail = session::show(index_path, opts.rid)?;
    if Target::of(&detail.row.agent_id).is_none() {
        bail!(
            "agent {} sessions are stats-only, cannot migrate",
            detail.row.agent_id
        );
    }
    if detail.turns.is_empty() {
        // 空会话移植出去只剩一行溯源,目标 agent 里多一个空壳。诚实报错,
        // 不制造「迁移成功了但什么都没带过去」的错觉。
        bail!(
            "session {} has no turns to migrate (nothing but control records)",
            opts.rid
        );
    }

    let home = session::resolve_home(opts.home.as_deref())?;
    let now_ms = opts.now_ms.unwrap_or_else(session::system_now_ms);

    let (mut plan, dropped) = plan_turns(&detail);
    merge_provenance(
        &mut plan,
        &detail.row.agent_id,
        &detail.row.key,
        &session::render_date(now_ms),
    );

    // cwd 取源会话的;读不到用 home(= $HOME)。绝不猜第三个值——cwd 决定
    // claude / omp 的目录编码,猜错等于把会话种进别人的项目。
    let cwd = detail
        .row
        .cwd
        .clone()
        .unwrap_or_else(|| home.display().to_string());

    let sid = uuid4();
    let (path, lines) = match target {
        Target::Claude => render_claude(&plan, &home, &cwd, &sid, now_ms),
        Target::Codex => render_codex(&plan, &home, &cwd, &sid, now_ms),
        Target::Omp => render_omp(&plan, &home, &cwd, &sid, now_ms),
    };

    let report = MigrateReport {
        rid: opts.rid,
        from: detail.row.agent_id.clone(),
        to: opts.to.clone(),
        target: path.display().to_string(),
        turns_planted: plan.len(),
        tool_turns_dropped: dropped,
        dry_run: opts.dry_run,
    };
    if opts.dry_run {
        return Ok(report);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    let mut body = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for line in &lines {
        body.push_str(line);
        body.push('\n');
    }
    duster_fs::atomic::write_atomic(&path, body.as_bytes())
        .with_context(|| format!("failed to write migrated session: {}", path.display()))?;

    if let Err(e) = verify_roundtrip(target, &path, &cwd, &plan) {
        // 验不过的文件不许留:它会被目标 agent 与下一次 scan 同时捡到,
        // 而两边都读不出完整正文。删除失败无所谓——报错里带着路径。
        let _ = std::fs::remove_file(&path);
        return Err(e.context(format!(
            "round-trip verification failed, removed the half-written file: {}",
            path.display()
        )));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// 计划:SessionDetail -> 纯文本轮次
// ---------------------------------------------------------------------------

/// 取舍(有损契约):
/// - user / assistant 的散文轮原样带走;
/// - 工具轮(`role == "tool"`)与 raw 兜底轮(抽取失败,text 是原始 JSONL 行)
///   一律丢弃——把原始 JSON 行塞进目标 agent 的会话是垃圾注入,不是移植;
/// - **连续**被丢的一段压成一行 assistant 文本 [`TOOL_OMITTED`],时间取该段
///   第一条的。段与段之间由散文轮隔开,几段就几行,不合并成全局一行——
///   「这里发生过工具活动」的位置本身是对话的一部分。
///
/// 返回 (计划, 被丢的源轮次数)。
fn plan_turns(detail: &SessionDetail) -> (Vec<PlannedTurn>, usize) {
    let mut plan: Vec<PlannedTurn> = Vec::with_capacity(detail.turns.len());
    let mut dropped = 0usize;
    // 当前被丢段:Some(第一条的时间) = 段开着。
    let mut run: Option<Option<i64>> = None;
    for t in &detail.turns {
        let carried = !t.raw_fallback && (t.role == "user" || t.role == "assistant");
        if carried {
            if let Some(ts) = run.take() {
                plan.push(PlannedTurn {
                    user: false,
                    text: TOOL_OMITTED.to_string(),
                    ts_ms: ts,
                });
            }
            plan.push(PlannedTurn {
                user: t.role == "user",
                text: t.text.clone(),
                ts_ms: t.ts_ms,
            });
        } else {
            dropped += 1;
            if run.is_none() {
                run = Some(t.ts_ms);
            }
        }
    }
    if let Some(ts) = run {
        plan.push(PlannedTurn {
            user: false,
            text: TOOL_OMITTED.to_string(),
            ts_ms: ts,
        });
    }
    (plan, dropped)
}

/// 溯源行(逐字契约),并入**第一条 user 文本**顶端。整场没有 user 轮时单独
/// 成为首条 user 记录——溯源不能因为源会话恰好没有用户发言而丢失。
///
/// 「tool activity omitted」恒在句里:即使这一场一条工具轮都没丢,源 agent
/// 的解析器在索引期就已经把 thinking / 工具流水挡在轮次表外了(见
/// `duster_adapter::native`),这句话对任何一场迁移都为真。
fn merge_provenance(plan: &mut Vec<PlannedTurn>, from: &str, key: &str, date: &str) {
    let line =
        format!("[migrated from {from} session {key} by duster on {date}; tool activity omitted]");
    match plan.iter_mut().find(|t| t.user) {
        Some(first_user) => first_user.text = format!("{line}\n{}", first_user.text),
        None => {
            let ts_ms = plan.first().and_then(|t| t.ts_ms);
            plan.insert(
                0,
                PlannedTurn {
                    user: true,
                    text: line,
                    ts_ms,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 三个写入器。每家只写 spike / 实测验证过的最小记录集,多余字段不写——
// 没验证过的字段是在别人的格式里即兴发挥。
// ---------------------------------------------------------------------------

/// claude-code:`~/.claude/projects/<cwd 编码>/<uuid>.jsonl`。
///
/// 目录名编码:cwd 的 `/` 全部换 `-`,保留前导 `-`(spike 实测 2.1.227 的
/// 目录就长这样,如 `-Users-me-Code-proj`)。编码不可逆没关系——真正的 cwd
/// 在每条记录里,目录名只是桶名(`duster session list` 的 cwd 还原同理
/// 从不反推目录名,见 [`session::list`])。
///
/// 信封与 message 的字段集是 spike 逐字段验证过的最小集:user 的 content
/// 是**字符串**,assistant 的 content 是 `[{type:"text",text}]` 且带
/// model / usage 零值。usage 写零而不是编一个像样的数:token 计数是
/// 计费口径,伪造它比缺省它危险。
fn render_claude(
    plan: &[PlannedTurn],
    home: &Path,
    cwd: &str,
    sid: &str,
    now_ms: i64,
) -> (PathBuf, Vec<String>) {
    let path = home
        .join(".claude/projects")
        .join(encode_cwd(cwd))
        .join(format!("{sid}.jsonl"));
    let mut lines = Vec::with_capacity(plan.len());
    let mut parent: Option<String> = None;
    for t in plan {
        let uuid = uuid4();
        let message = if t.user {
            json!({"role": "user", "content": t.text})
        } else {
            json!({
                "model": "claude-opus-5",
                "id": format!("msg_{}", hex_n(16)),
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": t.text}],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            })
        };
        let record = json!({
            "parentUuid": parent.as_deref(),
            "isSidechain": false,
            "uuid": uuid.as_str(),
            "timestamp": iso8601_ms(t.ts_ms.unwrap_or(now_ms)),
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": sid,
            "version": "2.1.227",
            "gitBranch": "",
            "type": if t.user { "user" } else { "assistant" },
            "message": message
        });
        lines.push(record.to_string());
        parent = Some(uuid);
    }
    (path, lines)
}

/// codex:`~/.codex/sessions/YYYY/MM/DD/rollout-<时间>-<uuid>.jsonl`。
///
/// 最小记录集来自本机真实 rollout(0.146/0.147)与 `fixtures/sessions/
/// codex-basic.jsonl` 的交集:首行 `session_meta`(cwd 只住这里),其后每轮
/// 一条 `response_item{payload:{type:"message",role,content:[…]}}`。段类型
/// user 是 `input_text`、assistant 是 `output_text`——写反了自家解析器
/// 照样读得出,codex 自己读不出,round-trip 挡不住这种错,别写反。
/// `originator` 写 `"duster"`:诚实署名;伪装成 `codex_exec` 反而可能改变
/// codex 端 resume 列表的过滤行为。
fn render_codex(
    plan: &[PlannedTurn],
    home: &Path,
    cwd: &str,
    sid: &str,
    now_ms: i64,
) -> (PathBuf, Vec<String>) {
    let date = session::render_date(now_ms);
    let path = home
        .join(".codex/sessions")
        .join(&date[0..4])
        .join(&date[5..7])
        .join(&date[8..10])
        .join(format!("rollout-{}-{sid}.jsonl", codex_stamp(now_ms)));
    let mut lines = Vec::with_capacity(plan.len() + 1);
    let now_iso = iso8601_ms(now_ms);
    lines.push(
        json!({
            "timestamp": now_iso,
            "type": "session_meta",
            "payload": {
                "id": sid,
                "timestamp": now_iso,
                "cwd": cwd,
                "originator": "duster",
                "cli_version": "0.146.0",
                "instructions": null
            }
        })
        .to_string(),
    );
    for t in plan {
        let (role, seg) = if t.user {
            ("user", "input_text")
        } else {
            ("assistant", "output_text")
        };
        lines.push(
            json!({
                "timestamp": iso8601_ms(t.ts_ms.unwrap_or(now_ms)),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": role,
                    "content": [{"type": seg, "text": t.text}]
                }
            })
            .to_string(),
        );
    }
    (path, lines)
}

/// omp:`~/.omp/agent/sessions/<cwd 编码>/<时间戳>_<uuid>.jsonl`。
///
/// 最小记录集来自本机 `~/.omp/agent/sessions` 真实文件(17.x)与
/// `fixtures/sessions/omp-basic.jsonl` 的交集:首行 `session`(version 3,
/// cwd 只住这里),其后每轮一条 `message{id,parentId,timestamp,message:{…}}`。
/// id 是短十六进制、parentId 链式(首条 null)——自家解析器不看链,
/// 但 omp 自己的会话树靠它排祖先,断链的文件在 omp 里是孤儿。
fn render_omp(
    plan: &[PlannedTurn],
    home: &Path,
    cwd: &str,
    sid: &str,
    now_ms: i64,
) -> (PathBuf, Vec<String>) {
    let path = home
        .join(".omp/agent/sessions")
        .join(encode_cwd(cwd))
        .join(format!("{}_{sid}.jsonl", omp_stamp(now_ms)));
    let mut lines = Vec::with_capacity(plan.len() + 1);
    lines.push(
        json!({
            "type": "session",
            "version": 3,
            "id": sid,
            "timestamp": iso8601_ms(now_ms),
            "cwd": cwd
        })
        .to_string(),
    );
    let mut parent: Option<String> = None;
    for t in plan {
        let id = hex_n(4);
        lines.push(
            json!({
                "type": "message",
                "id": id.as_str(),
                "parentId": parent.as_deref(),
                "timestamp": iso8601_ms(t.ts_ms.unwrap_or(now_ms)),
                "message": {
                    "role": if t.user { "user" } else { "assistant" },
                    "content": [{"type": "text", "text": t.text}]
                }
            })
            .to_string(),
        );
        parent = Some(id);
    }
    (path, lines)
}

/// cwd -> 目录名:`/` 全换 `-`,保留前导 `-`。claude 与 omp 同一套编码
/// (两家实测目录长一个样),codex 不分目录用不上。
fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

// ---------------------------------------------------------------------------
// round-trip 对账
// ---------------------------------------------------------------------------

/// 写完立刻用**自家原生解析器**(与 scan / show 同一份)读回来逐条对账:
/// cwd、轮数、角色、正文全等。
///
/// 这不是防御性编程,是这个动词的成立条件——「目标 agent 读得回去」在
/// duster 里唯一可机检的近似就是自家解析器。验不过说明写入器与解析器对
/// 格式的理解分叉了,调用方会删掉半成品再报错。
fn verify_roundtrip(target: Target, path: &Path, cwd: &str, plan: &[PlannedTurn]) -> Result<()> {
    use duster_adapter::native::{claude_session, codex_session, omp_jsonl_session};
    let (meta, turns, _events) = match target {
        Target::Claude => claude_session::parse(path)?,
        Target::Codex => codex_session::parse(path)?,
        Target::Omp => omp_jsonl_session::parse(path)?,
    };
    if meta.cwd.as_deref() != Some(cwd) {
        bail!(
            "cwd did not survive the round-trip: wrote {cwd}, read back {:?}",
            meta.cwd
        );
    }
    if turns.len() != plan.len() {
        bail!(
            "planted {} turns but the native parser read back {}",
            plan.len(),
            turns.len()
        );
    }
    for (i, (got, want)) in turns.iter().zip(plan).enumerate() {
        let want_role = if want.user { Role::User } else { Role::Assistant };
        if got.role != want_role {
            bail!("turn {i} came back as {:?}, expected {want_role:?}", got.role);
        }
        if got.text != want.text {
            bail!("turn {i} text changed in the round-trip");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// id 与时间戳
// ---------------------------------------------------------------------------

/// 进程内计数器:同一纳秒内连发的 id 靠它区分(部分平台的时钟粒度只有微秒)。
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 16 字节新鲜熵:blake3(纳秒时钟 ‖ pid ‖ 进程内计数器)。
///
/// 不引 `uuid` / `rand` 依赖:这些 id 只承担「文件名与记录 id 不撞车」的
/// 职责,不承载任何安全语义,为它们拖一个随机数栈进二进制不划算
/// (体积预算见工作区 Cargo.toml 的 release profile 注释);blake3 已经
/// 因内容哈希在依赖树里。
fn fresh16() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = blake3::Hasher::new();
    h.update(&nanos.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(&count.to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 前 n 字节的十六进制(n ≤ 16):assistant 的 `msg_<hex32>` 取 16,
/// omp 的短记录 id 取 4。
fn hex_n(n: usize) -> String {
    hex_of(&fresh16()[..n])
}

/// v4 形状的 UUID(8-4-4-4-12,version / variant 位按 RFC 4122 摆好)。
fn uuid4() -> String {
    let mut b = fresh16();
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h = hex_of(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Unix 毫秒 -> `YYYY-MM-DDTHH:MM:SS.mmmZ`(UTC)。三家的会话时间戳都是
/// 这个形状;秒级日历算术与 `session::stamp_of` 同源,这里只补毫秒段。
fn iso8601_ms(ms: i64) -> String {
    let ms = ms.max(0);
    let s = session::stamp_of(ms);
    format!(
        "{}-{}-{}T{}:{}:{}.{:03}Z",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15],
        ms % 1_000
    )
}

/// codex 文件名里的时间段:`YYYY-MM-DDTHH-MM-SS`(真实 rollout 同款,
/// 冒号换连字符——它要进文件名)。
fn codex_stamp(ms: i64) -> String {
    let s = session::stamp_of(ms.max(0));
    format!(
        "{}-{}-{}T{}-{}-{}",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    )
}

/// omp 文件名里的时间段:`YYYY-MM-DDTHH-MM-SS-mmmZ`(真实文件同款)。
fn omp_stamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{}-{:03}Z", codex_stamp(ms), ms % 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionFilter, SessionRow, TurnView};
    use std::fs;
    use tempfile::TempDir;

    /// 固定的「现在」:2023-11-14T22:13:20.000Z。溯源行日期、目标文件名、
    /// 无时间戳轮次的兜底时间全都相对它,测试不看真实时钟。
    const NOW_MS: i64 = 1_700_000_000_000;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/sessions")
            .join(name)
    }

    /// 假 home:三家的源会话各一场(fixtures 原样拷贝进各家的真实目录形状),
    /// 真扫描出索引。真实 `$HOME` 一个字节都不碰。
    fn seed() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();

        let cl = home.join(".claude/projects/-Users-tester-projects-demo");
        fs::create_dir_all(&cl).unwrap();
        fs::copy(
            fixture("claude-basic.jsonl"),
            cl.join("00000000-0000-4000-8000-000000000001.jsonl"),
        )
        .unwrap();

        let cx = home.join(".codex/sessions/2026/08/01");
        fs::create_dir_all(&cx).unwrap();
        fs::copy(
            fixture("codex-basic.jsonl"),
            cx.join("rollout-2026-08-01T12-00-00-0002.jsonl"),
        )
        .unwrap();

        let om = home.join(".omp/agent/sessions/-Users-tester-projects-demo");
        fs::create_dir_all(&om).unwrap();
        fs::copy(
            fixture("omp-basic.jsonl"),
            om.join("2026-08-01T10-00-00-000Z_019fd000-0000-7000-8000-000000000001.jsonl"),
        )
        .unwrap();

        let index = home.join(".agent-duster/index.db");
        crate::scan::scan(&crate::scan::ScanOptions {
            home: Some(home.clone()),
            index_path: Some(index.clone()),
            full: false,
        })
        .expect("scan 夹具");
        (tmp, home, index)
    }

    fn rid_of(index: &Path, agent: &str) -> i64 {
        let rows = session::list(Some(index), &SessionFilter::default())
            .unwrap()
            .rows;
        rows.iter()
            .find(|r| r.agent_id == agent)
            .map(|r| r.rid)
            .unwrap_or_else(|| panic!("夹具里没有 {agent} 的会话"))
    }

    fn opts(rid: i64, to: &str, home: &Path) -> MigrateOptions {
        MigrateOptions {
            rid,
            to: to.into(),
            dry_run: false,
            home: Some(home.to_path_buf()),
            now_ms: Some(NOW_MS),
        }
    }

    /// 验收主线:omp 源(带一条 toolResult 工具轮)→ claude 目标。
    /// 逐行可解、信封字段在位、parentUuid 链完整、溯源并入首条 user、
    /// 工具痕迹零残留、占位行恰好一行。
    #[test]
    fn 迁_omp_到_claude_逐行可解_链完整_溯源在首条() {
        let (_tmp, home, index) = seed();
        let rid = rid_of(&index, "omp");
        let report = migrate(Some(&index), &opts(rid, "claude-code", &home)).unwrap();

        let target = PathBuf::from(&report.target);
        assert!(
            target.starts_with(home.join(".claude/projects/-Users-tester-projects-demo")),
            "目标该落在 cwd 编码的项目目录下:{}",
            report.target
        );
        // omp 夹具的轮次:user / assistant / toolResult / assistant。
        // toolResult 压成一行占位 → 4 条落盘,1 条被丢。
        assert_eq!(report.turns_planted, 4);
        assert_eq!(report.tool_turns_dropped, 1);

        let body = fs::read_to_string(&target).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4);

        let stem = target.file_stem().unwrap().to_string_lossy().to_string();
        let mut prev: Option<String> = None;
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {i} 不是合法 JSON:{e}"));
            assert_eq!(v["isSidechain"], false);
            assert_eq!(v["userType"], "external");
            assert_eq!(v["entrypoint"], "cli");
            assert_eq!(v["version"], "2.1.227");
            assert_eq!(v["gitBranch"], "");
            assert_eq!(v["cwd"], "/Users/tester/projects/demo");
            // sessionId 全场一致且等于文件名主干:claude 按它归并会话。
            assert_eq!(v["sessionId"].as_str(), Some(stem.as_str()));
            match &prev {
                None => assert!(v["parentUuid"].is_null(), "首条 parentUuid 必须是 null"),
                Some(p) => assert_eq!(
                    v["parentUuid"].as_str(),
                    Some(p.as_str()),
                    "line {i} 的 parentUuid 链断了"
                ),
            }
            prev = Some(v["uuid"].as_str().expect("uuid 缺失").to_string());
        }

        // 溯源行并入首条 user 文本顶端,日期来自注入的 NOW_MS。
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["type"], "user");
        let text = first["message"]["content"].as_str().unwrap();
        assert!(text.starts_with("[migrated from omp session "), "{text}");
        assert!(
            text.contains(" by duster on 2023-11-14; tool activity omitted]\n帮我看看这个报错"),
            "{text}"
        );

        // 工具痕迹零残留:工具轮正文、调用 id、thinking 内容全不见。
        for needle in [
            "tool_use",
            "tool_result",
            "toolCall",
            "toolResult",
            "thinking",
            "toolu_placeholder",
            "no such file",
            "cogitatio",
        ] {
            assert!(!body.contains(needle), "目标文件不该有 {needle}");
        }
        assert_eq!(body.matches(TOOL_OMITTED).count(), 1, "占位行恰好一行");

        // 自家解析器把角色顺序钉死(migrate 内部已 round-trip,这里防它偷懒)。
        let (_m, turns, _e) =
            duster_adapter::native::claude_session::parse(&target).unwrap();
        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Assistant, Role::Assistant]
        );
        assert_eq!(turns[2].text, TOOL_OMITTED);
    }

    /// codex 目标 round-trip:自家解析器读回的 cwd / 角色 / 正文逐条相等,
    /// 文件按迁移时刻的 UTC 日期分桶。
    #[test]
    fn 迁_claude_到_codex_自家读取器_round_trip() {
        let (_tmp, home, index) = seed();
        let rid = rid_of(&index, "claude-code");
        let report = migrate(Some(&index), &opts(rid, "codex", &home)).unwrap();

        let target = PathBuf::from(&report.target);
        assert!(
            target.starts_with(home.join(".codex/sessions/2023/11/14")),
            "{}",
            report.target
        );
        let name = target.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("rollout-2023-11-14T22-13-20-") && name.ends_with(".jsonl"),
            "{name}"
        );
        // claude 源的工具/thinking 记录在索引期就没进轮次表,这里没有可丢的。
        assert_eq!(report.turns_planted, 4);
        assert_eq!(report.tool_turns_dropped, 0);

        let (meta, turns, _e) =
            duster_adapter::native::codex_session::parse(&target).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/Users/tester/projects/demo"));
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[0].role, Role::User);
        assert!(turns[0].text.starts_with("[migrated from claude-code session "));
        assert!(turns[0].text.ends_with("帮我看看这个报错"));
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "先看日志,再定位配置。");
        assert_eq!(turns[2].role, Role::User);
        assert_eq!(turns[2].text, "Lorem ipsum dolor sit amet");
        assert_eq!(turns[3].role, Role::Assistant);
        assert_eq!(turns[3].text, "收到,已修复。");
    }

    /// omp 目标 round-trip:message 记录的 parentId 链完整(omp 的会话树
    /// 靠它排祖先,断链即孤儿),自家解析器读回正文相等。
    #[test]
    fn 迁_codex_到_omp_自家读取器_round_trip() {
        let (_tmp, home, index) = seed();
        let rid = rid_of(&index, "codex");
        let report = migrate(Some(&index), &opts(rid, "omp", &home)).unwrap();

        let target = PathBuf::from(&report.target);
        assert!(
            target.starts_with(home.join(".omp/agent/sessions/-Users-tester-projects-demo")),
            "{}",
            report.target
        );
        let name = target.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("2023-11-14T22-13-20-000Z_"), "{name}");

        let (meta, turns, _e) =
            duster_adapter::native::omp_jsonl_session::parse(&target).unwrap();
        assert_eq!(meta.cwd.as_deref(), Some("/Users/tester/projects/demo"));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::User);
        assert!(turns[0].text.starts_with("[migrated from codex session "));
        assert!(turns[0].text.ends_with("请修复构建脚本"));
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "已修复,lorem ipsum。");

        // parentId 链:首条 message null,其后逐条衔接。
        let body = fs::read_to_string(&target).unwrap();
        let mut prev: Option<String> = None;
        for line in body.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["type"] != "message" {
                continue;
            }
            match &prev {
                None => assert!(v["parentId"].is_null(), "首条 message 的 parentId 必须是 null"),
                Some(p) => assert_eq!(v["parentId"].as_str(), Some(p.as_str())),
            }
            prev = Some(v["id"].as_str().unwrap().to_string());
        }
    }

    /// 目标名单外:错误文案逐字(与来源名单外同一句)。
    #[test]
    fn 目标_agent_名单外_直接报错() {
        let (_tmp, home, index) = seed();
        let rid = rid_of(&index, "codex");
        let err = migrate(Some(&index), &opts(rid, "gemini-cli", &home)).unwrap_err();
        assert!(
            err.to_string()
                .contains("agent gemini-cli sessions are stats-only, cannot migrate"),
            "{err:#}"
        );
    }

    /// 来源名单外:duster 索引得动它(cursor 的会话读得出原始行),
    /// 但迁移的成立条件是「目标读得回去」,名单外一律同一句报错。
    #[test]
    fn 源_agent_名单外_同一句报错() {
        let (_tmp, home, index) = seed();
        let src = home.join(".cursor/chat.jsonl");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        let line = r#"{"text":"hi"}"#;
        fs::write(&src, format!("{line}\n")).unwrap();

        let idx = duster_index::db::Index::open(&index).unwrap();
        let out = duster_index::upsert::upsert_resource(
            idx.conn(),
            &duster_index::upsert::ResourceRow {
                agent_id: "cursor".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "chat.jsonl".into(),
                path: src.to_string_lossy().into_owned(),
                size: (line.len() + 1) as u64,
                mtime_ns: NOW_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        duster_index::upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[duster_model::TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: Some(NOW_MS),
                byte_off: 0,
                byte_len: line.len() as u64,
                text: "hi".into(),
            }],
        )
        .unwrap();
        drop(idx);

        let err = migrate(Some(&index), &opts(out.rid, "claude-code", &home)).unwrap_err();
        assert!(
            err.to_string()
                .contains("agent cursor sessions are stats-only, cannot migrate"),
            "{err:#}"
        );
    }

    /// dry-run 只出报告不碰盘,计数与真跑一致——报告不是写完再数出来的。
    #[test]
    fn dry_run_只出报告不落盘() {
        let (_tmp, home, index) = seed();
        let rid = rid_of(&index, "omp");
        let report = migrate(
            Some(&index),
            &MigrateOptions {
                dry_run: true,
                ..opts(rid, "claude-code", &home)
            },
        )
        .unwrap();
        assert!(report.dry_run);
        assert!(
            !PathBuf::from(&report.target).exists(),
            "dry-run 不许碰盘:{}",
            report.target
        );
        assert_eq!(report.turns_planted, 4);
        assert_eq!(report.tool_turns_dropped, 1);
    }

    // -- 纯函数细节 ---------------------------------------------------------

    fn tv(role: &str, text: &str, raw: bool) -> TurnView {
        TurnView {
            seq: 0,
            role: role.into(),
            ts_ms: None,
            text: text.into(),
            tool: None,
            raw_fallback: raw,
        }
    }

    fn detail_of(turns: Vec<TurnView>) -> SessionDetail {
        SessionDetail {
            row: SessionRow {
                rid: 1,
                agent_id: "omp".into(),
                key: "k.jsonl".into(),
                path: "/tmp/k.jsonl".into(),
                cwd: None,
                bytes: 0,
                turns: turns.len() as u64,
                last_turn_ms: None,
                compressed: false,
            },
            turns,
        }
    }

    /// 连续被丢的一段压成一行;段与段之间由散文轮隔开,几段就几行。
    /// raw 兜底轮(text 是原始 JSONL 行)与工具轮同等待遇——原始行塞进
    /// 目标会话是垃圾注入。
    #[test]
    fn 连续被丢轮压成一行_分段各一行() {
        let detail = detail_of(vec![
            tv("user", "问", false),
            tv("tool", "输出A", false),
            tv("tool", "输出B", false),
            tv("assistant", "答", false),
            tv("user", "{\"raw\":1}", true),
            tv("assistant", "再答", false),
        ]);
        let (plan, dropped) = plan_turns(&detail);
        assert_eq!(dropped, 3);
        let texts: Vec<&str> = plan.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["问", TOOL_OMITTED, "答", TOOL_OMITTED, "再答"]);
        assert!(!plan[1].user && !plan[3].user, "占位行是 assistant 文本");
    }

    /// 整场没有 user 轮时,溯源单独成为首条 user 记录——不能因为源会话
    /// 没有用户发言就把出处弄丢。
    #[test]
    fn 没有_user_轮时_溯源单独成条() {
        let detail = detail_of(vec![tv("assistant", "独白", false)]);
        let (mut plan, dropped) = plan_turns(&detail);
        assert_eq!(dropped, 0);
        merge_provenance(&mut plan, "omp", "k.jsonl", "2023-11-14");
        assert_eq!(plan.len(), 2);
        assert!(plan[0].user);
        assert_eq!(
            plan[0].text,
            "[migrated from omp session k.jsonl by duster on 2023-11-14; tool activity omitted]"
        );
        assert_eq!(plan[1].text, "独白");
    }

    /// 目录编码:`/` 全换 `-`,前导 `-` 保留。
    #[test]
    fn cwd_编码保留前导横杠() {
        assert_eq!(
            encode_cwd("/Users/me/Code/agent-duster"),
            "-Users-me-Code-agent-duster"
        );
        assert_eq!(encode_cwd("/"), "-");
    }

    /// 时间戳形状:毫秒段三位补零,文件名段无冒号。
    #[test]
    fn 时间戳形状() {
        assert_eq!(iso8601_ms(NOW_MS), "2023-11-14T22:13:20.000Z");
        assert_eq!(iso8601_ms(NOW_MS + 7), "2023-11-14T22:13:20.007Z");
        assert_eq!(codex_stamp(NOW_MS), "2023-11-14T22-13-20");
        assert_eq!(omp_stamp(NOW_MS + 42), "2023-11-14T22-13-20-042Z");
    }

    /// uuid4 形状:8-4-4-4-12,version 位是 4、variant 位是 8/9/a/b;连发不撞车。
    #[test]
    fn uuid4_形状与唯一性() {
        let a = uuid4();
        let b = uuid4();
        assert_ne!(a, b);
        for u in [&a, &b] {
            let parts: Vec<&str> = u.split('-').collect();
            let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
            assert_eq!(lens, [8, 4, 4, 4, 12], "{u}");
            assert!(u.as_bytes()[14] == b'4', "version 位:{u}");
            assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "variant 位:{u}");
        }
    }
}
