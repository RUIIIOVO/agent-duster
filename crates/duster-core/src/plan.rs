//! 删除计划：`clean` / `prune` / `uninstall` 共用的一套。
//!
//! 三个动词的**同意模型**不同（见 README「三个动词」），但"要动哪些东西、
//! 为什么、动完什么后果"的表达是同一件事，所以计划结构只有一套。
//!
//! # 铁律
//!
//! 1. **默认 dry-run**。生成计划不等于执行，执行是另一个函数。
//! 2. **纯读索引**。`clean` / `prune` 的计划不重跑 probe、不重解析清单——
//!    `clean_level` 已经在 scan 时落库了。慢一步不如错一步。
//!    （`uninstall` 例外：它要删 agent 根目录整棵树，包括清单没声明的子目录，
//!    所以必须读清单拿到根路径。）
//! 3. **计划里必须出现 `install` 行**，`cleanable = false`，标注 not cleanable。
//!    让用户看见那几个 GB 的软件本体为什么不动，而不是当它不存在。
//! 4. **每一条都必须可评估**：是什么 / 为何可清（或为何判定陈旧 + 依据来源）/
//!    清后影响 / 归档包里有没有。禁止直接甩路径列表——没有回收站之后，
//!    全部安全性压在这一次确认上，它是唯一防线。
//! 5. **两套所有权模型不能塞进一个命令**（这是 uninstall 单独成词的根本原因）：
//!    `clean` / `prune` 只碰清单**声明过**的路径，白名单，没声明的绝不碰；
//!    `uninstall` 碰的是 agent 根目录**整棵树**，清单没声明的子目录
//!    （`~/.codex/computer-use`、`~/.qoder/canvas`）也必须删，否则卸载不干净。
//!    一个动词不可能同时守住这两条。所以计划结构共用，取材函数分开。
//!
//! # 计划的取舍（[`PlanFilter`]）
//!
//! 交互式确认链路由两层组成：**出计划**（本模块）与**执行**
//! （`clean.rs` / `prune.rs`）。两层之间隔着用户读清单、逐项勾选的时间，
//! 索引可能已经变了（另一个 duster 跑完一趟 scan、或某个 agent 刚写了一轮
//! 新会话）。所以「用户勾过什么」必须记成**白名单**（[`PlanFilter::allow`]）
//! 而不是黑名单（[`PlanFilter::skip`]）：Ask 路径先 `dry_run` 出一份计划给
//! 用户勾，再用同一组参数重跑一次真执行——重算时新冒出来的项，黑名单下
//! 会被执行，而用户**从没见过它**；白名单下它自动落在名单外。这是本次
//! 「确认链路」修复的核心：黑名单防的是越权，白名单防的是误伤。
//!
//! 白名单的键是「路径 + 动作」二元组，不是路径：两层之间重算的不只是
//! 「多了什么项」，同一条路径的**动作**也会变（并发的 `duster scan` 把
//! 某条 artifact 的 `clean_level` 从 l0 改成 l1）。用户勾的是 `vacuum`
//! （无损压实），执行的变成 `remove_file` / `truncate_file`（真删）——
//! 只比路径，白名单就变成了另一种黑名单。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::manifest;
use duster_adapter::probe::{self, ProbeSpec};
use duster_fs::walk::{WalkOptions, walk_stats};
use duster_index::db::Index;
use duster_index::query::{self, ResourceFilter, ResourceRecord};
use duster_model::CleanLevel;

use crate::freshness;

/// 三个动词。决定计划的取材范围与同意模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// 吃 l0 + l1：可再生垃圾，真删不归档，`--yes` 友好。
    Clean,
    /// 吃 l2 + 陈旧 skill/session：逐条过目，归档后永久删除。
    Prune,
    /// 整个 agent：独占目录整棵树，逐字输入 agent id 确认。
    Uninstall,
}

/// 计划项要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// SQLite VACUUM（l0，原地重写，数据一条不少）。
    Vacuum,
    /// 删除孤儿 WAL/SHM 或 `.tmp-*` 崩溃残留（l0）。
    RemoveSidecar,
    /// 删除整个文件。
    RemoveFile,
    /// 删除整棵目录树。
    RemoveDir,
    /// 原地截断日志文件到 0 字节。
    TruncateFile,
    /// 压缩存档（会话）：`.zst` 就地生成、往返校验通过后删原件。
    CompressFile,
    /// 不动。`install` 行与被保留兜底的备份走这个。
    Keep,
}

/// 一条计划项。字段顺序即人类模式下的阅读顺序。
#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub agent_id: String,
    /// 库内 kind 字符串。
    pub kind: String,
    pub clean_level: Option<CleanLevel>,
    pub path: PathBuf,
    /// 索引里的自然键（skill 名 / 会话文件名 / 清单声明路径）。
    pub key: String,
    /// 索引行 id；执行后据此收尾（删行 / 改 path）。计划外造出来的项记 -1。
    pub rid: i64,

    /// **是什么**：一句话说清这是个啥，如「Codex 日志库（11353 行日志）」。
    pub what: String,
    /// **为何可清 / 为何判定陈旧**。陈旧判定必须带上最后使用时间与依据来源，
    /// 如「最后一条轮次 2025-11-02，距今 282 天（依据：轮次时间戳）」。
    pub why: String,
    /// **清后影响**：如「下次启动自动重建」「历史不可再检索，需从归档包还原」。
    pub impact: String,
    /// **归档包里有没有**。
    pub archived: bool,

    pub action: Action,
    /// 这一项预计能回收的字节数。l0 只算空洞，install 恒为 0。
    pub bytes: u64,
    /// false = 永不清理（`install`）。计划里照样显示，只是标注 not cleanable。
    pub cleanable: bool,
    /// 这一项来自声明了 `keep_generations` 的资源(代际保留或超编代际)。
    /// 超编代际是**按数量冗余**而不是年龄陈旧:第 N+1 份起与年龄无关地
    /// 冗余。展示层据此把超编项分到独立一组,读者不会把"16 天前"误读成
    /// 它被删的原因;保留项则换一句准确的归档标记。
    pub generation: bool,
    /// 最后使用时间（Unix 毫秒）。取不到为 None——**一律视为最近用过**。
    pub last_used_ms: Option<i64>,
}

/// 一份完整计划。
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub verb: Verb,
    pub items: Vec<PlanItem>,
    /// 本计划执行后预计释放的字节数（只算 `cleanable && action != Keep`）。
    pub reclaim_bytes: u64,
    /// 软件本体合计（`install` 行 + skill 内嵌 install_bytes）。永不释放。
    pub install_bytes: u64,
    /// 陈旧资源合计（l2 + 超期 skill/session）。clean 报告里指向 `duster prune`。
    pub stale_bytes: u64,
    pub warnings: Vec<String>,
}

impl Plan {
    /// 需要归档的路径（`archived == true` 的项）。
    pub fn archive_roots(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter(|i| i.archived)
            .map(|i| i.path.clone())
            .collect()
    }

    /// 真正会被动的项（排除 `Keep` 与 not cleanable）。
    pub fn actionable(&self) -> impl Iterator<Item = &PlanItem> {
        self.items
            .iter()
            .filter(|i| i.cleanable && i.action != Action::Keep)
    }
}

/// 用户过目后对一份计划的取舍。为什么是白名单而不是黑名单的反面，
/// 见模块头「计划的取舍」——那是本次「确认链路」修复的核心。
#[derive(Debug, Clone, Default)]
pub struct PlanFilter {
    /// 黑名单：**这个入口无权动**的**具体路径**。
    pub skip: Vec<PathBuf>,
    /// 白名单：**用户逐项过目并勾过**的「路径 + 动作」。
    /// `None` = 没有过目这一步（命令行 `--yes`），全部执行。
    ///
    /// 键是二元组而不是路径：计划在批准与执行之间会重算，同一条路径的
    /// **动作**可以变（并发的 `duster scan` 把某条 artifact 的
    /// `clean_level` 从 l0 改成 l1）——用户屏幕上勾的是 `vacuum`
    /// （无损压实），执行的变成 `remove_file` / `truncate_file`（真删）；
    /// prune 侧同理，勾的是 `compress`（不删内容），执行成 `remove_dir`。
    /// 白名单挡得住「没见过的新项」，挡不住「见过的项换了刀」——
    /// 所以连同动作一起锁死，不匹配即剔除（fail-closed，与幽灵路径语义
    /// 一致：名单只可能缩小，不可能扩大）。
    pub allow: Option<Vec<(PathBuf, Action)>>,
    /// 结构性纵向边界：只放行 `kind` 等于它的项。`None` = 不限。
    ///
    /// 这是**入口的职责**（如 `duster session prune` 只许动会话），不是
    /// 用户的取舍。用谓词而不是快照黑名单表达：每一份重算出来的计划都
    /// 自动受它约束，两次计划之间新冒出来的非会话项也逃不掉——
    /// 黑名单盖不住没见过的新项，谓词盖得住。
    pub only_kind: Option<&'static str>,
}

impl PlanFilter {
    /// 只要白名单：用户勾过哪些「路径 + 动作」，就只动哪些。
    pub fn allow_only(items: Vec<(PathBuf, Action)>) -> Self {
        PlanFilter {
            skip: Vec::new(),
            allow: Some(items),
            only_kind: None,
        }
    }

    /// 只要黑名单：入口无权动这些路径，其余照旧。
    pub fn skipping(paths: Vec<PathBuf>) -> Self {
        PlanFilter {
            skip: paths,
            allow: None,
            only_kind: None,
        }
    }

    /// 结构性纵向边界：只放行 `kind` 等于它的项。
    pub fn only_kind(kind: &'static str) -> Self {
        PlanFilter {
            skip: Vec::new(),
            allow: None,
            only_kind: Some(kind),
        }
    }

    /// 就地过滤计划并重算 `reclaim_bytes`。
    ///
    /// 三条谓词全过才留：`!skip.contains(path)`、`only_kind` 等于 `kind`、
    /// `allow` 含 `(path, action)`。路径与动作都按**逐项相等**匹配——
    /// 计划项的 `path` 已经是展开过的绝对路径，调用方也只能从同一份计划里
    /// 取，两边同源才不会出现「以为放行了其实没有」。
    ///
    /// 查表用 `HashSet` 而不是 `Vec::contains`：交互式 clean 默认全勾时
    /// `allow.len() == plan.items.len()`，`Vec::contains` 是严格 O(n²) 次
    /// `PathBuf` 逐字节比较；集合在入口处建一次，查询摊还 O(1)。
    ///
    /// `reclaim_bytes` 必须重算：过滤后它再是全集的和，报告就承诺了一个
    /// 它不会释放的数字。`stale_bytes` / `install_bytes` 不动——那两桶讲的
    /// 是别的动词的地盘，勾掉一条待清项不会让它们变小。
    pub fn apply(&self, plan: &mut Plan) {
        let skip: HashSet<&Path> = self.skip.iter().map(|p| p.as_path()).collect();
        let allow: Option<HashSet<(&Path, Action)>> = self
            .allow
            .as_ref()
            .map(|a| a.iter().map(|(p, act)| (p.as_path(), *act)).collect());
        plan.items.retain(|i| {
            !skip.contains(i.path.as_path())
                && self.only_kind.is_none_or(|k| i.kind == k)
                && allow
                    .as_ref()
                    .is_none_or(|a| a.contains(&(i.path.as_path(), i.action)))
        });
        plan.reclaim_bytes = plan.actionable().map(|i| i.bytes).sum();
    }
}

/// 计划生成的输入。
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// 索引库路径；缺省 `<home>/.agent-duster/index.db`。
    pub index_path: Option<PathBuf>,
    /// 假 home 注入口（测试用）；缺省真实用户主目录。
    pub home: Option<PathBuf>,
    /// 只针对这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    /// 陈旧阈值（天）。prune 必填，clean 忽略。
    pub older_than_days: Option<u32>,
    /// 是否把声明了 `keep_generations` 的资源的**超编代际**纳入 prune。
    /// 默认关——关了与今天逐字节一致,只多一行收尾报告点名这个旗标。
    pub keep_generations: bool,
    /// 当前时间（Unix 毫秒）注入口，测试用；None = 系统时钟。
    pub now_ms: Option<i64>,
}

/// `--older-than` 的三个预置档位。交互模式给这三档 + 自定义输入。
pub const OLDER_THAN_PRESETS: [u32; 3] = [30, 60, 90];

/// clean 收尾报告里「陈旧资源 Y」用的缺省阈值。与 [`OLDER_THAN_PRESETS`]
/// 的第一档一致——报告里给的数字必须是用户照着敲一遍 prune 能复现出来的那个，
/// 否则「陈旧资源 2.1 GB」会变成一句没人能验证的话。
const DEFAULT_STALE_DAYS: u32 = OLDER_THAN_PRESETS[0];

/// 一天的毫秒数。
const DAY_MS: i64 = 86_400_000;

/// 会话压缩的经验压缩比（省下约 80%）。真实数字压完才知道，
/// 计划里给的只能是估算，[`Action::CompressFile`] 项的 `why` 必须说明这一点。
const COMPRESS_SAVED_NUM: u64 = 4;
const COMPRESS_SAVED_DEN: u64 = 5;

/// 拒绝 `--older-than` 时统一附上的可接受形式。所有拒绝路径共用一句，
/// 免得同一个参数有三种说法。
const ACCEPTED_FORMS: &str =
    "accepted forms: 30d / 60d / 90d, or any <N>d with N >= 1 (days only; w/m/y are not accepted)";

fn older_than_help(raw: &str) -> String {
    format!("invalid --older-than value {raw:?}: {ACCEPTED_FORMS}")
}

/// 解析 `--older-than` 的值：`30d` / `60d` / `90d`，也接受任意 `<N>d`。
///
/// 只接受天，不接受 `w` / `m` / `y`：单位越多，"90d 到底是不是三个月"这种
/// 心算就越容易出错，而这个参数决定的是删什么。
pub fn parse_older_than(s: &str) -> Result<u32> {
    let raw = s.trim();
    // 裸数字（`30`）与其他单位（`1w`）都落在这里：不猜用户想要哪个单位。
    let Some(digits) = raw.strip_suffix('d') else {
        bail!("{}", older_than_help(raw));
    };
    // 只认纯 ASCII 数字，不靠 `parse` 的宽容度兜底：`+3d` / `-1d` / `3_0d`
    // 以及空的 `d` 都得死在这里。
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{}", older_than_help(raw));
    }
    let days: u32 = match digits.parse() {
        Ok(n) => n,
        // 位数溢出 u32。u32 天已经是一千一百万年，不是笔误就是攻击。
        Err(_) => {
            bail!("invalid --older-than value {raw:?}: day count is out of range. {ACCEPTED_FORMS}")
        }
    };
    if days == 0 {
        bail!(
            "invalid --older-than value {raw:?}: 0d would sweep everything ever created; \
             the minimum is 1d. {ACCEPTED_FORMS}"
        );
    }
    Ok(days)
}

/// 陈旧判定。**`last_used_ms` 为 None 一律返回 false**（视为最近用过，宁可漏杀）。
pub fn is_stale(last_used_ms: Option<i64>, days: u32, now_ms: i64) -> bool {
    match last_used_ms {
        None => false,
        Some(t) => now_ms.saturating_sub(t) > (days as i64) * 86_400_000,
    }
}

// ---------------------------------------------------------------------------
// 共用基础设施
// ---------------------------------------------------------------------------

/// 确定 home：优先注入值（测试用假 home），否则真实用户主目录。
fn resolve_home(opts: &PlanOptions) -> Result<PathBuf> {
    if let Some(h) = &opts.home {
        return Ok(h.clone());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("cannot determine home directory (HOME is not set)");
    }
    Ok(h)
}

/// 把 `~` / `~/...` 相对指定 home 展开；其余形式原样返回。
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// 只读打开索引。缺省 `<home>/.agent-duster/index.db`。
///
/// 库还没建过就先建一次（[`crate::freshness::ensure_exists`]），与
/// [`crate::status`] 同一条路：「库在不在、谁负责让它在」整个程序里
/// 只能有一种说法。
fn open_index(opts: &PlanOptions, home: &Path) -> Result<Index> {
    let declared = opts
        .index_path
        .clone()
        .unwrap_or_else(|| home.join(".agent-duster").join("index.db"));
    let path = freshness::ensure_exists(Some(&declared))?;
    Index::open_readonly(&path)
        .with_context(|| format!("failed to open index read-only: {}", path.display()))
}

/// 当前时间：注入优先（测试），否则系统时钟。
fn now_ms(opts: &PlanOptions) -> i64 {
    if let Some(t) = opts.now_ms {
        return t;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 最后使用时间 + **依据来源** + 展示用的前缀词。
///
/// 三样必须一起走：光给「2025-11-02」用户没法判断这个日期可不可信，
/// 也就没法评估该不该删——而这次确认是删除前的唯一一道防线。
#[derive(Debug, Clone, Copy)]
struct LastUsed {
    ms: Option<i64>,
    /// 依据来源，如「轮次时间戳」。
    source: &'static str,
    /// 句子前缀，如「最后一条轮次」。
    lead: &'static str,
}

impl LastUsed {
    /// 无证据。**不是「1970 年用过」**——后者会把这一行直接判死。
    const UNKNOWN: LastUsed = LastUsed {
        ms: None,
        source: "no usable timestamp",
        lead: "Last used",
    };
}

/// 把行上的 mtime 当证据用。`mtime_ns <= 0` 表示 scan 当时没取到时间戳
/// （stats-only 采集失败、权限不足等），那是**没有证据**。
fn mtime_evidence(rec: &ResourceRecord, lead: &'static str, source: &'static str) -> LastUsed {
    if rec.mtime_ns <= 0 {
        return LastUsed::UNKNOWN;
    }
    LastUsed {
        ms: Some(rec.mtime_ms()),
        source,
        lead,
    }
}

/// 「最后使用时间」的唯一口径表。别处不许再定义第二套。
///
/// 取 `&Index` 而不是连接：duster-core 不依赖 rusqlite，连接类型
/// 一次都不该在这一层的签名里出现。
fn last_used(idx: &Index, rec: &ResourceRecord) -> Result<LastUsed> {
    Ok(match rec.kind.as_str() {
        // 会话看轮次时间戳：文件 mtime 会被备份工具、云盘同步摸到，
        // 轮次 ts 才是「人什么时候用的」。一条带 ts 的轮次都没有才退回 mtime。
        "session" => match query::last_turn_ms(idx.conn(), rec.rid)? {
            Some(ms) => LastUsed {
                ms: Some(ms),
                source: "turn timestamps",
                lead: "Last turn",
            },
            None => mtime_evidence(
                rec,
                "Last modified",
                "file mtime (this session has no timestamped turns)",
            ),
        },
        // scan 落库的 skill mtime 是目录内最新文件的改动时间。它只证明
        // 「文件最近被改过」，证明不了「人最近用过」——一个纯被对话调用、
        // 从不改文件的 skill 会永远显得陈旧。真正的使用证据是会话里的
        // **调用记录**（skill_event 表，解析器从工具调用里提取），取两者
        // 较新者。正文里提到技能名不算——那是引用不是调用，旧实现的名字
        // 全文检索（skill_last_seen_ms）就死在这上面，已删。
        "skill" => {
            // 反误删大闸:证据没采集过的索引(本改动之前的旧库)里,空事件表
            // 会读成「从没调用过」,把整机 skill 全扫进删除计划。「没查过」
            // 和「查过、没有」是两个事实,必须先跑一次 `duster scan`。
            // 连 mtime 兜底都不许用:目录三年没动的 skill 可能天天被调用,
            // 「没记录」只是因为我们还没看。
            if !query::skill_evidence_ready(idx.conn())? {
                return Ok(LastUsed {
                    ms: None,
                    source: "no skill-invocation evidence collected yet (run `duster scan` once)",
                    lead: "Last invocation",
                });
            }
            let mut lu = mtime_evidence(
                rec,
                "Newest change in the folder",
                "newest mtime in the folder",
            );
            // 匹配声明名**或**目录 basename:7 份真实副本两者不同
            // (design-taste-frontend 在 taste-skill/ 里、open-gstack-browser 在
            // connect-chrome/ 里……),codex 的记录带的是目录名,只按声明名
            // 匹配会凭空造出新误删。
            let dir_name = Path::new(&rec.path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match query::skill_last_invoked_ms(
                idx.conn(),
                &query::skill_name_of(&rec.key),
                &dir_name,
            )? {
                Some(ms) if lu.ms.is_none_or(|m| ms > m) => {
                    lu = LastUsed {
                        ms: Some(ms),
                        source: "skill invocation records in sessions",
                        lead: "Last invoked",
                    };
                }
                Some(_) => {} // 有调用记录但目录 mtime 更新:以 mtime 为准,措辞不变。
                None => {
                    // 查过、没有调用记录——「以目录 mtime 为准」的理由必须
                    // 讲明白,否则读者会以为这是纯年龄判定,而年龄从来不是
                    // skill 的删因:是「目录没动 + 查过、没调用」两个事实。
                    lu.source = "no invocation on record; newest folder mtime";
                }
            }
            lu
        }
        // MCP 只有「出现在会话正文里」这一种使用证据。查不到就是没证据，
        // 不是没用过——装着偶尔用一次的 MCP 到处都是。
        "mcp" => match query::mcp_last_seen_ms(idx.conn(), &rec.key)? {
            Some(ms) => LastUsed {
                ms: Some(ms),
                source: "full-text search across session bodies",
                lead: "Last seen in a session",
            },
            None => LastUsed::UNKNOWN,
        },
        "artifact" => mtime_evidence(rec, "Last modified", "file mtime"),
        _ => LastUsed::UNKNOWN,
    })
}

/// Unix 毫秒 -> `YYYY-MM-DD`（UTC）。日历算术复用 `duster_fs::zst::stamp`，
/// 不在这里再抄一份闰年规则。
fn render_date(ms: i64) -> String {
    let secs = ms.div_euclid(1_000).max(0) as u64;
    let s = duster_fs::zst::stamp(UNIX_EPOCH + Duration::from_secs(secs));
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// 距今天数（向下取整，未来时间归零）。
fn days_since(ms: i64, now: i64) -> i64 {
    now.saturating_sub(ms).max(0) / DAY_MS
}

/// 「距今 N 天」的人话。0 与 1 单独出：同一行里蹦出个 "1 days ago"，
/// 会让人连带怀疑旁边那几个数字。
fn days_ago(n: i64) -> String {
    match n {
        0 => "today".to_string(),
        1 => "1 day ago".to_string(),
        n => format!("{n} days ago"),
    }
}

/// 陈旧判定的 `why`：时间 + 距今 + 阈值 + 依据来源，四样缺一不可。
fn stale_why(lu: &LastUsed, now: i64, days: u32) -> String {
    match lu.ms {
        Some(ms) => format!(
            "{} {}, {}, past the --older-than {days}d threshold (evidence: {}).",
            lu.lead,
            render_date(ms),
            days_ago(days_since(ms, now)),
            lu.source
        ),
        // 正常路径到不了这里（无证据永不判定陈旧），兜底也要把话说清楚。
        None => format!("{} is unknown (evidence: {}).", lu.lead, lu.source),
    }
}

/// 判定「还在用」的 `why`。
fn fresh_why(lu: &LastUsed, now: i64, days: u32) -> String {
    match lu.ms {
        Some(ms) => format!(
            "{} {}, {}, within the --older-than {days}d threshold (evidence: {}).",
            lu.lead,
            render_date(ms),
            days_ago(days_since(ms, now)),
            lu.source
        ),
        None => format!(
            "{} is unknown (evidence: {}). No evidence is not the same as unused, so this counts as recently used.",
            lu.lead, lu.source
        ),
    }
}

/// 只陈述证据（日期 + 距今 + 来源），不带阈值判定。
///
/// fresh/stale 模板都内置了「在阈值内 / 已超期」的结论，而代际保留项的
/// 依据是数量兜底不是年龄——整组都超期时保留的仍是最近 N 份，套 fresh
/// 会说谎、套 stale 会自相矛盾，所以给它们一个中性句式。
fn evidence_why(lu: &LastUsed, now: i64) -> String {
    match lu.ms {
        Some(ms) => format!(
            "{} {}, {} (evidence: {}).",
            lu.lead,
            render_date(ms),
            days_ago(days_since(ms, now)),
            lu.source
        ),
        None => format!(
            "{} is unknown (evidence: {}). No evidence is not the same as unused, so this counts as recently used.",
            lu.lead, lu.source
        ),
    }
}

/// 人话体积。计划是给人读的，`812345678` 读不出来。
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 库里的级别字符串 -> 枚举。非法值当没有级别处理，由调用方报 warning。
fn parse_level(raw: Option<&str>) -> Option<CleanLevel> {
    match raw {
        Some("l0") => Some(CleanLevel::L0),
        Some("l1") => Some(CleanLevel::L1),
        Some("l2") => Some(CleanLevel::L2),
        _ => None,
    }
}

/// 一条 artifact 能回收多少：优先 scan 实测的 `reclaimable`（l0 只算空洞），
/// 缺失才退回占用量。把「占了多少」当「能清多少」报出去是在骗自己。
fn item_bytes(rec: &ResourceRecord) -> u64 {
    rec.reclaimable.unwrap_or(rec.size)
}

/// 文件名（取不到时为空串）。
fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

/// ASCII 大小写无关的子串判断。`needle` 必须是小写。
/// 文件名的大小写不该决定一个文件删不删。
fn contains_ci(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// SQLite 的边车与崩溃残留。
fn is_sidecar(path: &Path) -> bool {
    let name = file_name(path);
    name.ends_with("-wal") || name.ends_with("-shm") || name.contains(".tmp-")
}

/// 看起来是个 SQLite 库。
fn looks_sqlite(path: &Path, key: &str) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("db" | "sqlite" | "sqlite3")
    ) || contains_ci(file_name(path), "sqlite")
        || contains_ci(key, "sqlite")
}

/// 看起来是个日志文件。声明路径与索引键都看：有的清单把日志声明成
/// `key = "logs"` 而文件名是个 uuid。
fn looks_log(path: &Path, key: &str) -> bool {
    contains_ci(file_name(path), "log") || contains_ci(key, "log")
}

/// 计划里真正会释放的字节数。
fn sum_actionable(items: &[PlanItem]) -> u64 {
    items
        .iter()
        .filter(|i| i.cleanable && i.action != Action::Keep)
        .map(|i| i.bytes)
        .fold(0u64, u64::saturating_add)
}

/// 最后一道闸：任何落在 `install` 路径里的项都不许成为可执行项。
///
/// 上游的 kind 过滤已经把 `install` 行挡在外面了，这里防的是另一种情况——
/// 清单写错、或某个 artifact 声明恰好盖住了 `node_modules` / `dist` / `bin`。
/// 软件本体删错的代价是重装，而这层检查是免费的。
fn drop_install_overlaps(
    items: &mut Vec<PlanItem>,
    install_paths: &[PathBuf],
    warnings: &mut Vec<String>,
) {
    items.retain(|item| match install_paths.iter().find(|p| item.path.starts_with(p)) {
        Some(hit) => {
            warnings.push(format!(
                "skipped {}: it lives inside the install path {} — software bodies are never cleaned",
                item.path.display(),
                hit.display()
            ));
            false
        }
        None => true,
    });
}

/// `install` 行在 clean / prune 计划里的样子：显示，但永不清理。
///
/// 必须出现——让用户看见那几个 GB 的软件本体为什么不动，而不是当它不存在，
/// 然后自己去猜「怎么只回收了这么点」。
fn install_keep_item(rec: &ResourceRecord) -> PlanItem {
    PlanItem {
        agent_id: rec.agent_id.clone(),
        kind: rec.kind.clone(),
        clean_level: None,
        path: PathBuf::from(&rec.path),
        key: rec.key.clone(),
        rid: rec.rid,
        what: format!(
            "{} installed software: {} ({})",
            rec.agent_id,
            rec.key,
            human_bytes(rec.size)
        ),
        why: "kind = install, not cleanable. The test is simple: if deleting it means installing it again, duster does not touch it.".to_string(),
        impact: format!(
            "This command does not touch a single byte of it. The only way to reclaim this space is `duster uninstall {}`, which takes all of that agent's data with it.",
            rec.agent_id
        ),
        archived: false,
        action: Action::Keep,
        bytes: 0,
        cleanable: false,
        generation: false,
        last_used_ms: None,
    }
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

/// 决定一条 artifact 的动作。顺序即优先级，第一条命中即定。
fn clean_action(level: CleanLevel, path: &Path, key: &str) -> Action {
    match level {
        // 边车判定必须排在 SQLite 之前：`logs_2.sqlite-wal` 两条都命中，
        // 但它是要删掉的残留，不是要 VACUUM 的库。
        CleanLevel::L0 if is_sidecar(path) => Action::RemoveSidecar,
        CleanLevel::L0 if looks_sqlite(path, key) => Action::Vacuum,
        // l0 的无损操作只有 VACUUM 与删边车两种。都不是就不动它：
        // 列出来让用户看见，但绝不为了凑一条计划猜一个删除动作。
        CleanLevel::L0 => Action::Keep,
        // 目录判定排在日志之前：一个叫 `logs` 的目录是整个删掉，不是截断。
        CleanLevel::L1 if path.is_dir() => Action::RemoveDir,
        CleanLevel::L1 if looks_log(path, key) => Action::TruncateFile,
        CleanLevel::L1 => Action::RemoveFile,
        // l2 归 prune，调用方已过滤；这里是双保险。
        CleanLevel::L2 => Action::Keep,
    }
}

fn clean_item(rec: &ResourceRecord, level: CleanLevel) -> PlanItem {
    let path = PathBuf::from(&rec.path);
    let action = clean_action(level, &path, &rec.key);
    let size = human_bytes(item_bytes(rec));
    let (what, why, impact) = match action {
        Action::Vacuum => (
            format!(
                "{} SQLite database {} ({size} reclaimable)",
                rec.agent_id, rec.key
            ),
            "l0, lossless: VACUUM rewrites the file in place and reclaims only the free pages left behind by deleted rows.".to_string(),
            "Not one row is lost, the file just gets smaller. The database cannot be written during the rewrite, so a lock hit skips this item.".to_string(),
        ),
        Action::RemoveSidecar => (
            format!(
                "{} SQLite sidecar / crash leftover {} ({size})",
                rec.agent_id,
                file_name(&path)
            ),
            "l0, lossless: `-wal` / `-shm` / `.tmp-*` are what a process leaves behind when it exits abnormally; the main database is already self-consistent."
                .to_string(),
            "No impact. The program rebuilds them on demand the next time it opens the database.".to_string(),
        ),
        Action::RemoveDir => (
            format!("{} cache folder {} ({size})", rec.agent_id, rec.key),
            "l1, regenerable: the manifest declares this as program-generated output, so it comes back on its own.".to_string(),
            "Rebuilt on next use, at the cost of one slower cold start.".to_string(),
        ),
        Action::TruncateFile => (
            format!("{} log file {} ({size})", rec.agent_id, file_name(&path)),
            "l1, regenerable: logs are only for after-the-fact debugging and play no part in running the program.".to_string(),
            "The file itself stays (truncated in place to 0 bytes), but the log history already in it is gone.".to_string(),
        ),
        Action::RemoveFile => (
            format!("{} regenerable output {} ({size})", rec.agent_id, rec.key),
            "l1, regenerable: the manifest declares this as program-generated output, so it comes back on its own.".to_string(),
            "Rebuilt the next time it is needed.".to_string(),
        ),
        _ => (
            format!("{} l0 resource {} ({size})", rec.agent_id, rec.key),
            "Marked l0 (lossless), but it is neither a SQLite database nor a crash leftover, and duster has no lossless operation for it."
                .to_string(),
            "Nothing happens to it this run. Either the manifest should change its level, or this row should never have been an artifact.".to_string(),
        ),
    };
    PlanItem {
        agent_id: rec.agent_id.clone(),
        kind: rec.kind.clone(),
        clean_level: Some(level),
        path,
        key: rec.key.clone(),
        rid: rec.rid,
        what,
        why,
        impact,
        // 缓存永不归档：撤销一个缓存毫无意义，而它恰恰是体积最大的一档，
        // 进归档等于「承诺清理却一个字节没释放」。
        archived: false,
        action,
        bytes: if action == Action::Keep {
            0
        } else {
            item_bytes(rec)
        },
        cleanable: true,
        generation: false,
        last_used_ms: mtime_evidence(rec, "Last modified", "file mtime").ms,
    }
}

/// clean 收尾报告里的「陈旧资源 Y → `duster prune --older-than`」。
///
/// = 全部 l2 artifact（prune 的固定地盘）+ 按缺省 30 天阈值判定为陈旧的
/// skill / session。给的是用户照着敲一遍 prune 能看到的那个量级。
fn clean_stale_estimate(idx: &Index, opts: &PlanOptions, now: i64) -> Result<u64> {
    let mut total = 0u64;
    for rec in query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["artifact".to_string()],
            clean_levels: vec![CleanLevel::L2.as_str().to_string()],
        },
    )? {
        total = total.saturating_add(item_bytes(&rec));
    }
    for rec in query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["skill".to_string(), "session".to_string()],
            clean_levels: Vec::new(),
        },
    )? {
        if is_stale(last_used(idx, &rec)?.ms, DEFAULT_STALE_DAYS, now) {
            total = total.saturating_add(rec.size);
        }
    }
    Ok(total)
}

/// `clean` 的计划：l0 + l1 的 artifact，外加全部 `install` 行（not cleanable）。
///
/// 不含任何 l2——那是 `prune` 的地盘，clean 看不见它。但 `stale_bytes`
/// 要统计出来，收尾报告要报「陈旧资源 Y → `duster prune --older-than`」。
pub fn plan_clean(opts: &PlanOptions) -> Result<Plan> {
    let home = resolve_home(opts)?;
    let idx = open_index(opts, &home)?;
    let now = now_ms(opts);

    let mut warnings: Vec<String> = Vec::new();
    let mut items: Vec<PlanItem> = Vec::new();

    for rec in query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["artifact".to_string()],
            clean_levels: vec![
                CleanLevel::L0.as_str().to_string(),
                CleanLevel::L1.as_str().to_string(),
            ],
        },
    )? {
        let Some(level) = parse_level(rec.clean_level.as_deref()) else {
            // artifact 必须带级别。没有级别的行是脏数据，宁可跳过也不猜一档出来。
            warnings.push(format!(
                "skipped {}: artifact row has no usable clean_level ({:?})",
                rec.path, rec.clean_level
            ));
            continue;
        };
        if level == CleanLevel::L2 {
            continue; // 双保险：l2 永不进 clean 计划。
        }
        items.push(clean_item(&rec, level));
    }

    let installs = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["install".to_string()],
            clean_levels: Vec::new(),
        },
    )?;
    let install_paths: Vec<PathBuf> = installs.iter().map(|r| PathBuf::from(&r.path)).collect();
    drop_install_overlaps(&mut items, &install_paths, &mut warnings);
    items.extend(installs.iter().map(install_keep_item));
    drop_vanished(&mut items, &mut warnings);

    let reclaim_bytes = sum_actionable(&items);
    Ok(Plan {
        verb: Verb::Clean,
        items,
        reclaim_bytes,
        install_bytes: query::install_total(idx.conn(), &opts.agents)?,
        stale_bytes: clean_stale_estimate(&idx, opts, now)?,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// 删除动作按路径形态选：目录整棵删，文件单个删。
fn remove_action(path: &Path) -> Action {
    if path.is_dir() {
        Action::RemoveDir
    } else {
        Action::RemoveFile
    }
}

/// 索引是**派生物**，文件系统才是事实。
///
/// 上一次 scan 之后消失的路径（用户手删、或者上一条 `duster clean` 刚清掉的
/// 缓存目录）还留在库里。照着它出计划不只是多一行噪声——连动作类型都会判错：
/// `~/.claude/cache` 已经不在了，`path.is_dir()` 为假，于是一个目录被排成
/// 「delete file」。与其让每个 builder 各判一次，在出口统一滤掉。
fn drop_vanished(items: &mut Vec<PlanItem>, warnings: &mut Vec<String>) {
    let before = items.len();
    items.retain(|i| i.path.exists());
    let gone = before - items.len();
    if gone > 0 {
        warnings.push(format!(
            "{gone} indexed path(s) no longer exist on disk and were left out of this plan \
             (they were removed outside duster after being indexed; the plan already \
             accounts for that)"
        ));
    }
}

/// l2 artifact：按父目录成组，组内最新的一份留着兜底。
///
/// 红线是 todo.md §1.2 那句「绝不动唯一副本」：备份类资源的意义就是最后
/// 那一份还在，所以哪怕整组都超期，最新的那个也不进删除计划——组里只有
/// 一个成员时，留的就是它自己。
///
/// 声明了 `keep_generations` 的资源走另一套判定（按声明资源分组、按数量
/// 保留，见 [`prune_l2_generations`]）：它绕开年龄阈值，只在调用方显式
/// `--keep-generations` 时产出删除项。
fn prune_l2_items(
    idx: &Index,
    opts: &PlanOptions,
    days: u32,
    now: i64,
    warnings: &mut Vec<String>,
) -> Result<Vec<PlanItem>> {
    let rows = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["artifact".to_string()],
            clean_levels: vec![CleanLevel::L2.as_str().to_string()],
        },
    )?;

    // 代际资源（keep_generations 有值）与普通 l2 是两套判定，先分开：
    // 代际按数量保留、与年龄无关；普通按年龄清理。混进同一个「父目录组」
    // 里，代际的 16 天旧副本会被普通路径按 90d 阈值放过，而普通资源又
    // 永远等不到按数量的保留——互相污染。
    let (generations, plain): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|r| r.keep_generations.is_some());

    let mut out = prune_l2_generations(idx, opts, generations, days, now, warnings)?;
    out.extend(prune_l2_plain(idx, plain, days, now)?);
    Ok(out)
}

/// 普通 l2：按父目录成组，组内最新的一份留着兜底，其余按 `--older-than` 清。
///
/// 与今天的实现逐字一致——`--keep-generations` 关闭时，这份函数是唯一在
/// 跑的路，行为不得有任何漂移。
fn prune_l2_plain(
    idx: &Index,
    rows: Vec<ResourceRecord>,
    days: u32,
    now: i64,
) -> Result<Vec<PlanItem>> {
    let mut groups: BTreeMap<PathBuf, Vec<(ResourceRecord, LastUsed)>> = BTreeMap::new();
    for rec in rows {
        let lu = last_used(idx, &rec)?;
        let parent = Path::new(&rec.path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        groups.entry(parent).or_default().push((rec, lu));
    }

    let mut out = Vec::new();
    for members in groups.into_values() {
        let mut members = members;
        // 「最新」= last_used 最大；并列时按 path 取定，保证同一个库两次生成
        // 的计划逐字一致（可 diff、可先重定向到文件再回来执行）。
        members.sort_by(|a, b| a.1.ms.cmp(&b.1.ms).then_with(|| a.0.path.cmp(&b.0.path)));
        let newest = members.len() - 1;
        for (i, (rec, lu)) in members.iter().enumerate() {
            let path = PathBuf::from(&rec.path);
            let size = human_bytes(item_bytes(rec));
            if i == newest {
                out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: Some(CleanLevel::L2),
                    path,
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what: format!("{} l2 stale resource {} ({size})", rec.agent_id, rec.key),
                    why: format!(
                        "Newest copy in this folder, kept as a fallback — the whole point of a backup is that the last one survives. {}",
                        fresh_why(lu, now, days)
                    ),
                    impact: "Nothing happens to it this run. To clear it too, first make sure another copy exists somewhere else.".to_string(),
                    archived: false,
                    action: Action::Keep,
                    bytes: 0,
                    cleanable: true,
                    generation: false,
                    last_used_ms: lu.ms,
                });
                continue;
            }
            if !is_stale(lu.ms, days, now) {
                continue;
            }
            out.push(PlanItem {
                agent_id: rec.agent_id.clone(),
                kind: rec.kind.clone(),
                clean_level: Some(CleanLevel::L2),
                action: remove_action(&path),
                path,
                key: rec.key.clone(),
                rid: rec.rid,
                what: format!("{} l2 stale resource {} ({size})", rec.agent_id, rec.key),
                why: format!(
                    "The manifest marks this l2 (time-limited). {}",
                    stale_why(lu, now, days)
                ),
                impact: "Permanently deleted. It goes into the archive byte for byte first, so `tar -xf` brings it back."
                    .to_string(),
                archived: true,
                bytes: item_bytes(rec),
                cleanable: true,
                generation: false,
                last_used_ms: lu.ms,
            });
        }
    }
    Ok(out)
}

/// 代际分组键:(agent, kind, scope, 声明路径)。见 [`prune_l2_generations`]
/// 的分组说明——四段都进键才不会把两份清单声明的同一个目录塌成一组。
type GenerationKey = (String, String, String, String);

/// 一组代际成员:资源行 + 它的使用证据。
type GenerationGroup = BTreeMap<GenerationKey, Vec<(ResourceRecord, LastUsed)>>;

/// 代际 l2（声明了 `keep_generations` 的资源）：**按声明资源成组**，
/// 组内保留最新 N 份，其余是超编代际。
///
/// 分组键 = (agent, kind, scope, key 的父目录)：key 的格式是
/// 「声明路径 + 相对路径」（scan 侧，见 `scan_stats_glob`），而清单校验
/// 保证代际 glob 不含 `/`，所以 key 的父目录恒等于声明路径——按 key 的
/// 父目录分组就是按声明资源分组，不是按文件系统的父目录（两份清单声明
/// 同一备份目录时 agent 不同，照样分开）。
///
/// 超编代际**与年龄正交**：第 8 份副本冗余不冗余看数量不看 `--older-than`，
/// 所以它们只在调用方显式 `--keep-generations` 时进计划；旗标关闭时组里
/// 一行都不出，只把「多少份、多少字节」写进收尾报告并点名旗标——与
/// `install` 桶同一条发现路径：报出来，但一个字节都不碰。
fn prune_l2_generations(
    idx: &Index,
    opts: &PlanOptions,
    rows: Vec<ResourceRecord>,
    days: u32,
    now: i64,
    warnings: &mut Vec<String>,
) -> Result<Vec<PlanItem>> {
    let mut groups: GenerationGroup = BTreeMap::new();
    for rec in rows {
        let lu = last_used(idx, &rec)?;
        let declared = Path::new(&rec.key)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        groups
            .entry((
                rec.agent_id.clone(),
                rec.kind.clone(),
                rec.scope.clone(),
                declared,
            ))
            .or_default()
            .push((rec, lu));
    }

    let mut out = Vec::new();
    for ((agent, _kind, _scope, declared), members) in groups {
        let n = members[0].0.keep_generations.unwrap_or(1) as usize;
        let mut members = members;
        // 排序与普通 l2 同一把尺子：last_used 升序 + path 决胜，两个运行
        // 产生逐字一致的计划（可 diff、可先重定向到文件再回来执行）。
        // 升序下末尾 n 份即最新 n 份，前面的都是超编。
        members.sort_by(|a, b| a.1.ms.cmp(&b.1.ms).then_with(|| a.0.path.cmp(&b.0.path)));
        let total = members.len();
        let surplus = total.saturating_sub(n);

        if surplus > 0 && !opts.keep_generations {
            let bytes: u64 = members[..surplus]
                .iter()
                .map(|(rec, _)| item_bytes(rec))
                .fold(0u64, u64::saturating_add);
            warnings.push(format!(
                "{surplus} surplus generation(s) under {agent} {declared} ({}) — this resource keeps the newest {n}. Surplus means redundant by count, not by age, so --older-than never touches them; --keep-generations is off, so prune leaves them alone. Pass --keep-generations to include them.",
                human_bytes(bytes)
            ));
            continue;
        }

        for (i, (rec, lu)) in members.iter().enumerate() {
            let path = PathBuf::from(&rec.path);
            let size = human_bytes(item_bytes(rec));
            if i >= surplus {
                // 最新 n 份：兜底保留。与普通 l2 的「最新一份留着」是同一句
                // 话，只是份数从 1 变成声明值；`why` 不带阈值判定——整组都
                // 超期时套 fresh 模板会说「在阈值内」，那是谎话。
                out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: Some(CleanLevel::L2),
                    path,
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what: format!("{} l2 kept generation {} ({size})", rec.agent_id, rec.key),
                    why: format!(
                        "One of the newest {n} copies in this folder, kept as a fallback — the whole point of a backup is that the last {n} survive, even when every copy is past the --older-than {days}d threshold. {}",
                        evidence_why(lu, now)
                    ),
                    impact: "Nothing happens to it this run. To clear it too, first make sure another copy exists somewhere else.".to_string(),
                    archived: false,
                    action: Action::Keep,
                    bytes: 0,
                    cleanable: true,
                    generation: true,
                    last_used_ms: lu.ms,
                });
                continue;
            }
            // 超编代际：独立于 --older-than 的可删项。`why` 必须自己说清
            // 「数量冗余、不是年龄判断」——读者可能带着 90d 阈值来看这份
            // 只有 16 天的文件，行里没有这句话就解释不了它为什么在这。
            out.push(PlanItem {
                agent_id: rec.agent_id.clone(),
                kind: rec.kind.clone(),
                clean_level: Some(CleanLevel::L2),
                action: remove_action(&path),
                path,
                key: rec.key.clone(),
                rid: rec.rid,
                what: format!("{} l2 surplus generation {} ({size})", rec.agent_id, rec.key),
                why: format!(
                    "Copy {} of {total} in this folder; this resource keeps the newest {n}, so this copy is surplus by count, not by age — --keep-generations includes it regardless of the --older-than {days}d threshold.",
                    i + 1
                ),
                impact: "Permanently deleted. It goes into the archive byte for byte first, so `tar -xf` brings it back."
                    .to_string(),
                archived: true,
                bytes: item_bytes(rec),
                cleanable: true,
                generation: true,
                last_used_ms: lu.ms,
            });
        }
    }
    Ok(out)
}

/// 超期 skill：**跨 agent 同名整组判定**。
///
/// 只要组里任一副本还活跃，整组保留——`duster skill link` 的硬链源就在
/// 其中一份里，删掉源目录另一头直接失效。分组用还原后的 skill 名，
/// 不是 key 原文（同名冲突时 scan 会退化成 `name@目录名`）。
///
/// 「活跃」的唯一使用证据是调用记录（`skill_event` 表）。证据没采集过的
/// 索引（旧库升级而来、尚未重扫）里空表会读成「从没调用过」，把整机 skill
/// 全扫进删除计划——这种库整组冻结为 Keep，`why` 明说下一步是 `duster scan`。
fn prune_skill_items(
    idx: &Index,
    opts: &PlanOptions,
    days: u32,
    now: i64,
) -> Result<Vec<PlanItem>> {
    let mut out = Vec::new();
    for (name, members) in query::skill_groups(idx.conn())? {
        // 反误删大闸见函数文档。「没查过」和「查过、没有」是两个事实,
        // 前者一个都不许判陈旧。
        if !query::skill_evidence_ready(idx.conn())? {
            for rec in &members {
                if !opts.agents.is_empty() && !opts.agents.contains(&rec.agent_id) {
                    continue;
                }
                out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: None,
                    path: PathBuf::from(&rec.path),
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what: format!(
                        "{} skill `{name}` ({} of user content)",
                        rec.agent_id,
                        human_bytes(rec.size)
                    ),
                    why: "No skill-invocation evidence collected yet: this index predates invocation tracking, and an empty record would read as \"never invoked\" and sweep every skill away. Run `duster scan` once — it re-reads every session and records which skills were actually invoked. Until then this skill is kept."
                        .to_string(),
                    impact: "Nothing happens to it this run.".to_string(),
                    archived: false,
                    action: Action::Keep,
                    bytes: 0,
                    cleanable: true,
                    generation: false,
                    last_used_ms: None,
                });
            }
            continue;
        }

        let mut lus = Vec::with_capacity(members.len());
        for rec in &members {
            lus.push(last_used(idx, rec)?);
        }
        // 活跃判定跨全部 agent，即使本次只清一个 agent 也一样。
        let live = (0..members.len()).find(|&i| !is_stale(lus[i].ms, days, now));

        for (i, rec) in members.iter().enumerate() {
            if !opts.agents.is_empty() && !opts.agents.contains(&rec.agent_id) {
                continue;
            }
            let path = PathBuf::from(&rec.path);
            let embedded = match rec.install_bytes {
                Some(b) if b > 0 => format!(
                    ", plus {} of installed software (node_modules / dist and friends) that is neither counted nor archived",
                    human_bytes(b)
                ),
                _ => String::new(),
            };
            let what = format!(
                "{} skill `{name}` ({} of user content{embedded})",
                rec.agent_id,
                human_bytes(rec.size)
            );
            match live {
                Some(li) if li == i => out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: None,
                    path,
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what,
                    why: fresh_why(&lus[i], now, days),
                    impact: "Nothing happens to it this run.".to_string(),
                    archived: false,
                    action: Action::Keep,
                    bytes: 0,
                    cleanable: true,
                    generation: false,
                    last_used_ms: lus[i].ms,
                }),
                Some(li) => out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: None,
                    path,
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what,
                    // `live` 只认组里第一个没过期的成员,其余成员并不都是
                    // 过期的——两份都新鲜时,非第一份会被上面的分支误套
                    // "past the threshold" 模板,印出「1 天前、已过期」这种
                    // 自相矛盾的话。所以这里按这份副本自己的证据分两说。
                    why: if is_stale(lus[i].ms, days, now) {
                        format!(
                            "This copy is past the threshold: {} The same skill still has a live copy under {}: {} So the whole group is kept — deleting the source folder would break the hard link or symlink made by `duster skill link`, and the other end dies with it.",
                            stale_why(&lus[i], now, days),
                            members[li].agent_id,
                            fresh_why(&lus[li], now, days)
                        )
                    } else {
                        fresh_why(&lus[i], now, days)
                    },
                    impact: "Nothing happens to it this run. To clear just this copy, first confirm no other agent links to it."
                        .to_string(),
                    archived: false,
                    action: Action::Keep,
                    bytes: 0,
                    cleanable: true,
                    generation: false,
                    last_used_ms: lus[i].ms,
                }),
                None => out.push(PlanItem {
                    agent_id: rec.agent_id.clone(),
                    kind: rec.kind.clone(),
                    clean_level: None,
                    action: remove_action(&path),
                    path,
                    key: rec.key.clone(),
                    rid: rec.rid,
                    what,
                    why: format!(
                        "Every copy of this skill ({} in total) is past the threshold. {}",
                        members.len(),
                        stale_why(&lus[i], now, days)
                    ),
                    impact: "Permanently deleted, and this agent stops loading the skill. It goes into the archive byte for byte first (without the installed software inside the folder), so `tar -xf` brings it back."
                        .to_string(),
                    archived: true,
                    bytes: rec.size,
                    cleanable: true,
                    generation: false,
                    last_used_ms: lus[i].ms,
                }),
            }
        }
    }
    Ok(out)
}

/// 超期会话：**压缩存档，不删除内容**。
fn prune_session_items(
    idx: &Index,
    opts: &PlanOptions,
    days: u32,
    now: i64,
) -> Result<Vec<PlanItem>> {
    let mut out = Vec::new();
    for rec in query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["session".to_string()],
            clean_levels: Vec::new(),
        },
    )? {
        let lu = last_used(idx, &rec)?;
        if !is_stale(lu.ms, days, now) {
            continue;
        }
        let saved = rec.size.saturating_mul(COMPRESS_SAVED_NUM) / COMPRESS_SAVED_DEN;
        // 会话资源有两种形态：一场会话一个 jsonl（原生适配器，每文件一行索引），
        // 与整个目录只有一行索引（`~/.codex/archived_sessions` 走 stats-only）。
        // 两者都压，但说给用户的话不一样——「压这个目录里的每一个会话」和
        // 「压这一场会话」是不同的承诺。
        let is_dir = PathBuf::from(&rec.path).is_dir();
        out.push(PlanItem {
            agent_id: rec.agent_id.clone(),
            kind: rec.kind.clone(),
            clean_level: None,
            path: PathBuf::from(&rec.path),
            key: rec.key.clone(),
            rid: rec.rid,
            what: if is_dir {
                format!(
                    "{} session folder {} ({}, every jsonl inside compressed)",
                    rec.agent_id,
                    rec.key,
                    human_bytes(rec.size)
                )
            } else {
                format!(
                    "{} session {} ({})",
                    rec.agent_id,
                    rec.key,
                    human_bytes(rec.size)
                )
            },
            why: format!(
                "{} Expected saving is about {} — an **estimate** based on the usual 80% compression ratio; the real number only shows up once it is compressed.",
                stale_why(&lu, now, days),
                human_bytes(saved)
            ),
            impact: "Not one line is lost: compressed in place to `.zst`, and `duster search` / `duster open` decompress transparently on read. The original is deleted only after a round-trip check (BLAKE3 of the decompressed bytes matches the original byte for byte)."
                .to_string(),
            // 不进归档包：压缩是原地瘦身，原件的每个字节都还在 `.zst` 里，
            // 再打一个 tar 是把同样的内容存两遍。
            archived: false,
            action: Action::CompressFile,
            bytes: saved,
            cleanable: true,
            generation: false,
            last_used_ms: lu.ms,
        });
    }
    Ok(out)
}

/// 陈旧 MCP：M1 只报不删。
///
/// 移除一条 MCP 声明是**原地改写配置文件**（`~/.claude.json` 里还躺着二十条
/// 别的），依赖 M2 的 Codec 写方向 + schema_guard。但信息不能丢——查出来了
/// 就得说出来，否则用户永远不知道这些声明还在。
fn stale_mcp_warning(
    idx: &Index,
    opts: &PlanOptions,
    days: u32,
    now: i64,
) -> Result<Option<String>> {
    let mut stale: Vec<String> = Vec::new();
    for rec in query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["mcp".to_string()],
            clean_levels: Vec::new(),
        },
    )? {
        let lu = last_used(idx, &rec)?;
        if is_stale(lu.ms, days, now) {
            stale.push(format!("{}/{}", rec.agent_id, rec.key));
        }
    }
    if stale.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} MCP declaration(s) have not shown up in any session for over {days} days ({}). M1 does not remove them: that means rewriting a config file in place, which waits on the M2 Codec write path plus schema_guard. This is a report only and produces no plan items — to deal with it now, edit the config by hand.",
        stale.len(),
        stale.join(", ")
    )))
}

/// `prune` 的计划：l2 artifact + 超期 skill + 超期 session，外加 `install` 行。
///
/// `opts.older_than_days` 为 None 时报错——prune **默认关闭**，
/// 必须显式 `--older-than`。
///
/// 备份类 l2 资源保留最新一份兜底（同一声明路径下按 mtime 最新的那个不进计划）。
/// 声明了 `keep_generations` 的资源按数量保留最新 N 份，超编代际只在
/// `opts.keep_generations` 为真时进计划；为假时收尾报告点名旗标（见
/// [`prune_l2_generations`]）。
/// MCP 的陈旧清理依赖 M2 的 Codec 写方向 + schema_guard，M1 不做：
/// 计划里以 warning 形式说明，不产出任何改写配置的项。
pub fn plan_prune(opts: &PlanOptions) -> Result<Plan> {
    let Some(days) = opts.older_than_days else {
        bail!(
            "prune is off by default and never guesses a threshold: pass an explicit \
             --older-than (e.g. `duster prune --older-than 30d`). {ACCEPTED_FORMS}"
        );
    };
    let home = resolve_home(opts)?;
    let idx = open_index(opts, &home)?;
    let now = now_ms(opts);

    let mut warnings: Vec<String> = Vec::new();
    let mut items = prune_l2_items(&idx, opts, days, now, &mut warnings)?;
    items.extend(prune_skill_items(&idx, opts, days, now)?);
    items.extend(prune_session_items(&idx, opts, days, now)?);
    if let Some(w) = stale_mcp_warning(&idx, opts, days, now)? {
        warnings.push(w);
    }

    let installs = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: opts.agents.clone(),
            kinds: vec!["install".to_string()],
            clean_levels: Vec::new(),
        },
    )?;
    let install_paths: Vec<PathBuf> = installs.iter().map(|r| PathBuf::from(&r.path)).collect();
    drop_install_overlaps(&mut items, &install_paths, &mut warnings);
    items.extend(installs.iter().map(install_keep_item));
    drop_vanished(&mut items, &mut warnings);

    let reclaim_bytes = sum_actionable(&items);
    Ok(Plan {
        verb: Verb::Prune,
        items,
        reclaim_bytes,
        install_bytes: query::install_total(idx.conn(), &opts.agents)?,
        // 过滤前必然相等：prune 计划里的每一项本身就是「陈旧」那一桶。
        // `PlanFilter::apply` 有意不动 `stale_bytes`，过一道白名单后两者
        // 不再相等——别拿这句当不变式。
        stale_bytes: reclaim_bytes,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

/// `uninstall --data-only` 的计划：agent 的独占目录与独占文件整棵树。
///
/// 与 clean/prune 的所有权模型相反：**清单没声明的子目录也要删**
/// （`~/.codex/computer-use`、`~/.qoder/canvas`），否则卸载不干净。
/// 所以这里必须读清单拿 probe 路径当根，而不是只看索引里的资源行。
///
/// 这里只出**独占路径**这一份计划。共享文件里的本 agent 段落与包管理器
/// 提示不是"要删的路径"，它们由 [`crate::uninstall`] 直接读清单的
/// `[uninstall]` 段处理，逐条出现在报告的 `shared` / `packages` 两栏里。
pub fn plan_uninstall(opts: &PlanOptions, agent: &str) -> Result<Plan> {
    let home = resolve_home(opts)?;
    let adapters_dir = home.join(".agent-duster").join("adapters");
    let manifests = manifest::load_all(Some(&adapters_dir))?;
    let Some(m) = manifests.iter().find(|m| m.agent.id == agent) else {
        let known: Vec<&str> = manifests.iter().map(|m| m.agent.id.as_str()).collect();
        bail!(
            "unknown agent id {agent:?}. Known agents: {}",
            known.join(", ")
        );
    };

    let outcome = probe::probe(
        &ProbeSpec {
            any_of: m.probe.any_of.clone(),
            all_of: m.probe.all_of.clone(),
            binary: m.probe.binary.clone(),
            version_cmd: m.probe.version_cmd.clone(),
        },
        &home,
    );

    let idx = open_index(opts, &home)?;
    let rows = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: vec![agent.to_string()],
            kinds: Vec::new(),
            clean_levels: Vec::new(),
        },
    )?;

    let mut warnings: Vec<String> = Vec::new();
    if !outcome.installed {
        warnings.push(format!(
            "probe found no trace of {agent} under {}, so the plan below may be empty.",
            home.display()
        ));
    }

    // 根 = 清单声明为「本 agent 独占」且**实际存在**的路径。整棵树都删，
    // 包括清单没声明的子目录——这正是 uninstall 与 clean/prune 所有权模型
    // 相反的地方。
    //
    // 口径走 [`manifest::Manifest::owned_roots`]，不自己拿 probe 路径凑：
    // `[uninstall].owns` 才是「哪些树是它的」的正式声明，probe 只是
    // 它省略时的推导来源。两处各推一遍的话，一份把 owns 写窄的用户清单
    // 会出现「校验按 owns 判、删除按 probe 删」的错位。
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    for raw in m.owned_roots() {
        let p = expand(&raw, &home);
        if !p.exists() || roots.iter().any(|(_, seen)| seen == &p) {
            continue;
        }
        roots.push((raw, p));
    }

    let mut items: Vec<PlanItem> = Vec::new();
    for (raw, path) in &roots {
        let is_dir = path.is_dir();
        let bytes = if is_dir {
            walk_stats(path, &WalkOptions::default())
                .with_context(|| format!("failed to size {}", path.display()))?
                .total_bytes
        } else {
            std::fs::metadata(path).map(|md| md.len()).unwrap_or(0)
        };
        // 树里有会话或记忆就必须先归档：那是用户自己写出来的内容，不可再生。
        let user_data = rows
            .iter()
            .filter(|r| matches!(r.kind.as_str(), "session" | "memory"))
            .filter(|r| Path::new(&r.path).starts_with(path))
            .count();
        items.push(PlanItem {
            agent_id: agent.to_string(),
            kind: "root".to_string(),
            clean_level: None,
            path: path.clone(),
            key: raw.clone(),
            // 清单派生，不是索引行：执行后没有行要收尾。
            rid: -1,
            what: format!(
                "{agent} exclusive {}: {raw} ({})",
                if is_dir { "folder tree" } else { "file" },
                human_bytes(bytes)
            ),
            why: "Path the manifest probe declares as this agent's own. uninstall deletes the whole tree, including subfolders the manifest never declared — leaving half of it behind is not an uninstall."
                .to_string(),
            impact: if user_data > 0 {
                format!(
                    "{agent} will no longer start. The tree holds {user_data} session/memory file(s), packed into the archive byte for byte before deletion, so `tar -xf` restores them."
                )
            } else {
                format!(
                    "{agent} will no longer start. The index has no sessions or memories under this tree, so nothing goes into the archive and no local copy is left after deletion."
                )
            },
            archived: user_data > 0,
            action: if is_dir {
                Action::RemoveDir
            } else {
                Action::RemoveFile
            },
            bytes,
            cleanable: true,
            generation: false,
            last_used_ms: None,
        });
    }

    // `install` 行在这里翻转成 cleanable：uninstall 正是唯一被允许删除
    // 软件本体的动词。体积只在根目录项上计一次，避免同一批字节报两遍。
    for rec in rows.iter().filter(|r| r.kind == "install") {
        let path = PathBuf::from(&rec.path);
        let inside = roots.iter().any(|(_, root)| path.starts_with(root));
        items.push(PlanItem {
            agent_id: rec.agent_id.clone(),
            kind: rec.kind.clone(),
            clean_level: None,
            action: remove_action(&path),
            path,
            key: rec.key.clone(),
            rid: rec.rid,
            what: format!(
                "{} installed software: {} ({})",
                rec.agent_id,
                rec.key,
                human_bytes(rec.size)
            ),
            why: if inside {
                "Installed software. **uninstall is the only verb allowed to delete it** — clean / prune always mark it not cleanable, and here it flips to cleanable. It sits under a probe root, so it goes with the whole tree; its size is already counted on the root item and is not counted twice."
                    .to_string()
            } else {
                "Installed software. **uninstall is the only verb allowed to delete it** — clean / prune always mark it not cleanable, and here it flips to cleanable. It sits outside every probe root, so it is deleted on its own and counted on its own."
                    .to_string()
            },
            impact: format!("You have to install {} again to use it; that is exactly what uninstall means.", rec.agent_id),
            archived: false,
            bytes: if inside { 0 } else { rec.size },
            cleanable: true,
            generation: false,
            last_used_ms: None,
        });
    }

    let reclaim_bytes = sum_actionable(&items);
    Ok(Plan {
        verb: Verb::Uninstall,
        items,
        reclaim_bytes,
        install_bytes: query::install_total(idx.conn(), &[agent.to_string()])?,
        // uninstall 整体带走，不再区分「陈旧」与否——这个数字在这里没有意义。
        stale_bytes: 0,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::upsert::{self, ResourceRow, upsert_resource};
    use duster_model::{Role, SkillInvocation, TurnRecord};
    use std::fs;
    use tempfile::TempDir;

    /// 固定的「现在」（2025-10-09T07:33:20Z）。测试永不读系统时钟，
    /// 否则「距今 282 天」这类断言会随着日历慢慢烂掉。
    const NOW: i64 = 1_760_000_000_000;

    /// 造一行资源。`mtime_ms` 传 0 表示「scan 没取到时间戳」。
    fn row(
        agent: &str,
        kind: &str,
        key: &str,
        path: &Path,
        size: u64,
        mtime_ms: i64,
    ) -> ResourceRow {
        ResourceRow {
            agent_id: agent.to_string(),
            kind: kind.to_string(),
            scope: "global".to_string(),
            key: key.to_string(),
            path: path.display().to_string(),
            size,
            mtime_ns: mtime_ms * 1_000_000,
            hash_content: None,
            cheap_print: None,
            clean_level: None,
            reclaimable: None,
            install_bytes: None,
            mapper: None,
        }
    }

    fn artifact(
        agent: &str,
        key: &str,
        path: &Path,
        size: u64,
        mtime_ms: i64,
        level: &str,
        reclaimable: u64,
    ) -> ResourceRow {
        let mut r = row(agent, "artifact", key, path, size, mtime_ms);
        r.clean_level = Some(level.to_string());
        r.reclaimable = Some(reclaimable);
        r
    }

    fn mkfile(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    fn mkdir(p: &Path) {
        fs::create_dir_all(p).unwrap();
    }

    /// 建库写入后立刻释放连接：单实例写锁不留给后面的只读打开。
    fn seed(home: &Path, rows: &[ResourceRow]) -> PathBuf {
        let path = home.join(".agent-duster").join("index.db");
        let idx = Index::open(&path).unwrap();
        for r in rows {
            upsert_resource(idx.conn(), r).unwrap();
        }
        drop(idx);
        path
    }

    /// 建库 + 点亮「调用证据已采集」标记。skill 的陈旧判定只认调用记录
    /// （`skill_event`），证据没采集时整组冻结为 Keep——测 mtime / 调用
    /// 证据本身的行为必须先点亮，否则全部走冻结分支。
    fn seed_with_evidence(home: &Path, rows: &[ResourceRow]) -> PathBuf {
        let path = seed(home, rows);
        let idx = Index::open(&path).unwrap();
        duster_index::meta::set(idx.conn(), duster_index::meta::SKILL_EVIDENCE_READY, "1").unwrap();
        drop(idx);
        path
    }

    /// 全部注入：假 home、显式索引路径、固定时钟。真实 `$HOME` 一次都不碰。
    fn opts(home: &Path, index: &Path) -> PlanOptions {
        PlanOptions {
            index_path: Some(index.to_path_buf()),
            home: Some(home.to_path_buf()),
            agents: Vec::new(),
            older_than_days: None,
            keep_generations: false,
            now_ms: Some(NOW),
        }
    }

    #[test]
    fn older_than_只接受_n_天() {
        assert_eq!(parse_older_than("30d").unwrap(), 30);
        assert_eq!(parse_older_than("7d").unwrap(), 7);
        assert_eq!(parse_older_than(" 90d ").unwrap(), 90);

        for bad in [
            "30",
            "1w",
            "0d",
            "abc",
            "",
            "d",
            "-1d",
            "+3d",
            "99999999999999d",
        ] {
            let err = match parse_older_than(bad) {
                Ok(v) => panic!("{bad:?} 不该被接受，却解析成了 {v}"),
                Err(e) => e.to_string(),
            };
            // 拒绝时必须把可接受形式说出来，否则用户只能瞎试。
            assert!(
                err.contains("30d"),
                "{bad:?} 的错误信息没给出可接受形式：{err}"
            );
        }
    }

    #[test]
    fn clean_含_l0_l1_不含_l2_并列出_install() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let db = home.join(".codex/logs_2.sqlite");
        let wal = home.join(".codex/logs_2.sqlite-wal");
        let cache = home.join(".codex/cache");
        let log = home.join(".codex/debug.log");
        let l2 = home.join(".codex/archived_sessions");
        let inst = home.join(".codex/extensions");
        mkfile(&db);
        mkfile(&wal);
        mkdir(&cache);
        mkfile(&log);
        mkdir(&l2);
        mkdir(&inst);

        let index = seed(
            home,
            &[
                artifact("codex", "logs", &db, 1000, NOW, "l0", 800),
                artifact("codex", "logs-wal", &wal, 100, NOW, "l0", 100),
                artifact("codex", "cache", &cache, 500, NOW, "l1", 500),
                artifact("codex", "debug", &log, 300, NOW, "l1", 300),
                artifact(
                    "codex",
                    "archived",
                    &l2,
                    2000,
                    NOW - 400 * DAY_MS,
                    "l2",
                    2000,
                ),
                row("codex", "install", "extensions", &inst, 9000, NOW),
            ],
        );

        let plan = plan_clean(&opts(home, &index)).unwrap();
        assert_eq!(plan.verb, Verb::Clean);
        assert!(
            plan.items
                .iter()
                .all(|i| i.clean_level != Some(CleanLevel::L2)),
            "l2 永远不该出现在 clean 计划里"
        );

        let by_key = |k: &str| plan.items.iter().find(|i| i.key == k).unwrap();
        assert_eq!(by_key("logs").action, Action::Vacuum);
        assert_eq!(by_key("logs-wal").action, Action::RemoveSidecar);
        assert_eq!(by_key("cache").action, Action::RemoveDir);
        assert_eq!(by_key("debug").action, Action::TruncateFile);

        let inst_item = by_key("extensions");
        assert!(!inst_item.cleanable, "install 行必须标 not cleanable");
        assert_eq!(inst_item.action, Action::Keep);
        assert_eq!(inst_item.bytes, 0);
        assert!(inst_item.impact.contains("duster uninstall"));

        // 每一条都必须可评估，且 clean 一律不归档。
        for i in &plan.items {
            assert!(!i.what.is_empty() && !i.why.is_empty() && !i.impact.is_empty());
            assert!(!i.archived, "clean 的项永不进归档包：{}", i.key);
        }

        assert_eq!(plan.reclaim_bytes, 800 + 100 + 500 + 300);
        assert_eq!(plan.install_bytes, 9000);
        // 收尾三桶报告的第二桶：l2 要被算进去，指向 `duster prune`。
        assert!(
            plan.stale_bytes >= 2000,
            "stale_bytes = {}",
            plan.stale_bytes
        );
    }

    #[test]
    fn prune_必须显式阈值_且只选超期行() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let old = home.join(".claude/projects/p/old.jsonl");
        let fresh = home.join(".claude/projects/p/fresh.jsonl");
        mkfile(&old);
        mkfile(&fresh);
        let index = seed(
            home,
            &[
                row(
                    "claude",
                    "session",
                    "old.jsonl",
                    &old,
                    1000,
                    NOW - 100 * DAY_MS,
                ),
                row(
                    "claude",
                    "session",
                    "fresh.jsonl",
                    &fresh,
                    1000,
                    NOW - 5 * DAY_MS,
                ),
            ],
        );

        let mut o = opts(home, &index);
        let err = plan_prune(&o).unwrap_err().to_string();
        assert!(err.contains("--older-than"), "{err}");
        assert!(err.contains("off by default"), "{err}");

        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();
        let acted: Vec<&PlanItem> = plan.actionable().collect();
        assert_eq!(acted.len(), 1, "只有 100 天前那份该进计划");
        assert_eq!(acted[0].key, "old.jsonl");
        assert_eq!(acted[0].action, Action::CompressFile);
        // 压缩是原地瘦身，原件的字节都还在 .zst 里，不再打一份 tar。
        assert!(!acted[0].archived);
        assert_eq!(acted[0].bytes, 800);
        // why 必须同时带最后使用时间与依据来源。
        assert!(
            acted[0].why.contains(&render_date(NOW - 100 * DAY_MS)),
            "{}",
            acted[0].why
        );
        assert!(acted[0].why.contains("evidence:"), "{}", acted[0].why);
    }

    #[test]
    fn 同名_skill_只要一份活跃就整组保留() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let a = home.join(".claude/skills/foo");
        let b = home.join(".codex/skills/foo");
        mkdir(&a);
        mkdir(&b);
        let index = seed_with_evidence(
            home,
            &[
                row("claude", "skill", "foo", &a, 100, NOW - 300 * DAY_MS),
                row("codex", "skill", "foo", &b, 200, NOW - DAY_MS),
            ],
        );
        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 2, "两份副本都要出现在计划里");
        assert!(skills.iter().all(|i| i.action == Action::Keep));
        assert_eq!(plan.actionable().count(), 0);
        assert_eq!(plan.reclaim_bytes, 0);

        // 保留理由必须说清楚是「有活跃的同名副本 + 删了会打断 link」，
        // 而不是干巴巴一句「kept」。
        let stale_copy = skills.iter().find(|i| i.agent_id == "claude").unwrap();
        assert!(stale_copy.why.contains("skill link"), "{}", stale_copy.why);
        assert!(stale_copy.why.contains("codex"), "{}", stale_copy.why);
    }

    /// skill 的目录 300 天没改过,但有 5 天前的**真实调用记录**——
    /// 「最后使用」取目录 mtime 与调用证据的较新者,不算陈旧,整组保留。
    /// 这是「一个纯被调用、从不改文件的 skill 会永远显得陈旧」的回归测试,
    /// 证据是调用记录而不是正文里的名字出现。
    #[test]
    fn skill_最近有调用记录_即使目录久未改动也不算陈旧() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude/skills/used-in-chat");
        let sess = home.join(".claude/projects/p/x.jsonl");
        mkdir(&skill);
        mkfile(&sess);
        let index = seed_with_evidence(
            home,
            &[row(
                "claude",
                "skill",
                "used-in-chat",
                &skill,
                100,
                NOW - 300 * DAY_MS,
            )],
        );

        // seed() 只插资源行;手工补一场 5 天前的调用事件(挂在会话资源下)。
        let idx = Index::open(&index).unwrap();
        let sess_row = upsert_resource(
            idx.conn(),
            &row(
                "claude",
                "session",
                "x.jsonl",
                &sess,
                1000,
                NOW - 300 * DAY_MS,
            ),
        )
        .unwrap();
        upsert::replace_skill_events(
            idx.conn(),
            sess_row.rid,
            &[SkillInvocation {
                skill: "used-in-chat".to_string(),
                ts_ms: NOW - 5 * DAY_MS,
            }],
        )
        .unwrap();
        // 事件载体会话也要新鲜(5 天前有轮次),否则它自己会被 prune 压档,
        // 干扰「该删的是不是 skill」这个断言。
        upsert::replace_turns(
            idx.conn(),
            sess_row.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: Some(NOW - 5 * DAY_MS),
                byte_off: 0,
                byte_len: 40,
                text: "调用一下技能".to_string(),
            }],
        )
        .unwrap();
        drop(idx);

        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].action, Action::Keep);
        // 保留理由必须说清楚证据来源是调用记录,而不是干巴巴一句「目录没改」。
        assert!(skills[0].why.contains("Last invoked"), "{}", skills[0].why);
        assert_eq!(plan.actionable().count(), 0);
    }

    /// 声明名与目录名不同的 skill（design-taste-frontend 在 taste-skill/ 里），
    /// 调用记录带的是目录名，plan 层必须同样认——只按声明名匹配会误删。
    #[test]
    fn 声明名与目录名不同_按目录名的调用记录也算活跃() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude/skills/taste-skill");
        let sess = home.join(".claude/projects/p/x.jsonl");
        mkdir(&skill);
        mkfile(&sess);
        let index = seed_with_evidence(
            home,
            &[row(
                "claude",
                "skill",
                "design-taste-frontend", // key = 声明名
                &skill,
                100,
                NOW - 300 * DAY_MS,
            )],
        );

        let idx = Index::open(&index).unwrap();
        let sess_row = upsert_resource(
            idx.conn(),
            &row(
                "claude",
                "session",
                "x.jsonl",
                &sess,
                1000,
                NOW - 300 * DAY_MS,
            ),
        )
        .unwrap();
        upsert::replace_skill_events(
            idx.conn(),
            sess_row.rid,
            &[SkillInvocation {
                skill: "taste-skill".to_string(), // codex 记录带的是目录名
                ts_ms: NOW - 5 * DAY_MS,
            }],
        )
        .unwrap();
        // 事件载体会话也要新鲜,否则它自己会被 prune 压档(见上一测试的注释)。
        upsert::replace_turns(
            idx.conn(),
            sess_row.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: Some(NOW - 5 * DAY_MS),
                byte_off: 0,
                byte_len: 40,
                text: "调用一下技能".to_string(),
            }],
        )
        .unwrap();
        drop(idx);

        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].action, Action::Keep);
        assert!(skills[0].why.contains("Last invoked"), "{}", skills[0].why);
        assert_eq!(plan.actionable().count(), 0);
    }

    /// 反误删大闸:证据未采集(旧库升级而来、尚未重扫)时,空事件表会读成
    /// 「从没调用过」——目录 300 天没动的 skill 必须 Keep,why 指向 duster scan。
    #[test]
    fn 证据未采集时_久未改动的skill保持且why指向scan() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude/skills/old-but-maybe-used");
        mkdir(&skill);
        let index = seed(
            home,
            &[row(
                "claude",
                "skill",
                "old-but-maybe-used",
                &skill,
                100,
                NOW - 300 * DAY_MS,
            )],
        );

        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].action, Action::Keep);
        assert!(skills[0].why.contains("duster scan"), "{}", skills[0].why);
        assert_eq!(plan.actionable().count(), 0);
        assert_eq!(plan.reclaim_bytes, 0);
    }

    /// 证据已采集、且确实没有调用记录时,同一个 skill 才被判陈旧——
    /// 「查过、没有」和「没查过」必须产出相反的结果。
    #[test]
    fn 证据已采集且无调用记录_久未改动的skill判陈旧() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude/skills/old-and-unused");
        mkdir(&skill);
        let index = seed_with_evidence(
            home,
            &[row(
                "claude",
                "skill",
                "old-and-unused",
                &skill,
                100,
                NOW - 300 * DAY_MS,
            )],
        );

        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills[0].action,
            remove_action(&PathBuf::from(&skills[0].path))
        );
        assert!(skills[0].archived);
        assert!(
            skills[0].why.contains("Every copy of this skill"),
            "{}",
            skills[0].why
        );
        assert_eq!(plan.actionable().count(), 1);
    }

    /// 同组两份副本都新鲜时,只有第一份被指认为 live,第二份的保留理由
    /// 必须照实说「在阈值内」,不能套 "past the threshold" 模板——
    /// 否则会印出「1 天前、已过期」这种自相矛盾的话。
    #[test]
    fn 同组两份都新鲜时_非live那份不能说_past() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let a = home.join(".claude/skills/pair");
        let b = home.join(".codex/skills/pair");
        mkdir(&a);
        mkdir(&b);
        let index = seed_with_evidence(
            home,
            &[
                row("claude", "skill", "pair", &a, 100, NOW - 5 * DAY_MS),
                row("codex", "skill", "pair", &b, 200, NOW - 3 * DAY_MS),
            ],
        );
        let mut o = opts(home, &index);
        o.older_than_days = Some(30);
        let plan = plan_prune(&o).unwrap();

        let skills: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "skill").collect();
        assert_eq!(skills.len(), 2);
        assert!(skills.iter().all(|i| i.action == Action::Keep));
        for s in &skills {
            assert!(
                !s.why.contains("past the --older-than"),
                "两份都新鲜,不该有任何一份说 past: {}",
                s.why
            );
            assert!(s.why.contains("within the --older-than"), "{}", s.why);
        }
        assert_eq!(plan.actionable().count(), 0);
    }

    #[test]
    fn 取不到时间戳的行永不被_prune_选中() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude/skills/mystery");
        let sess = home.join(".claude/projects/p/x.jsonl");
        mkdir(&skill);
        mkfile(&sess);
        let index = seed(
            home,
            &[
                // mtime_ns = 0 = scan 当时没取到时间戳，不是「1970 年用过」。
                row("claude", "skill", "mystery", &skill, 500, 0),
                row("claude", "session", "x.jsonl", &sess, 500, 0),
            ],
        );
        let mut o = opts(home, &index);
        o.older_than_days = Some(1); // 阈值卡到最紧，仍然一条都不该选中。
        let plan = plan_prune(&o).unwrap();
        assert_eq!(plan.actionable().count(), 0);
        assert_eq!(plan.reclaim_bytes, 0);
    }

    #[test]
    fn 可执行项永不落在_install_路径下() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let node_modules = home.join(".claude/skills/gstack/node_modules");
        let bad_cache = node_modules.join(".cache");
        let dist = home.join(".claude/plugins/dist");
        let skill = home.join(".claude/skills/gstack");
        let sess = home.join(".claude/projects/p/old.jsonl");
        mkdir(&bad_cache);
        mkdir(&dist);
        mkfile(&sess);

        // skill 陈旧判定只看调用记录;没有标记时整组冻结为 Keep,这条测试
        // 断言的是「install 路径不许进计划」,需要 skill 真正进入陈旧判定
        // (300 天没动 → 删)才成立,所以点亮证据标记。
        let index = seed_with_evidence(
            home,
            &[
                row(
                    "claude",
                    "install",
                    "node_modules",
                    &node_modules,
                    7000,
                    NOW,
                ),
                row("claude", "install", "dist", &dist, 3000, NOW),
                // 清单写错，一个 l1 声明盖住了 install 子树。这一项必须被拦掉。
                artifact("claude", "gstack-cache", &bad_cache, 500, NOW, "l1", 500),
                row("claude", "skill", "gstack", &skill, 100, NOW - 300 * DAY_MS),
                row(
                    "claude",
                    "session",
                    "old.jsonl",
                    &sess,
                    1000,
                    NOW - 300 * DAY_MS,
                ),
            ],
        );

        let mut o = opts(home, &index);
        let clean = plan_clean(&o).unwrap();
        o.older_than_days = Some(30);
        let prune = plan_prune(&o).unwrap();

        let installs = [node_modules.clone(), dist.clone()];
        for (label, plan) in [("clean", &clean), ("prune", &prune)] {
            for item in plan.actionable() {
                assert!(
                    !installs.iter().any(|p| item.path.starts_with(p)),
                    "{label} 计划里出现了 install 路径：{}",
                    item.path.display()
                );
            }
        }
        // 拦掉不等于吞掉：必须留下痕迹。
        assert!(
            clean.warnings.iter().any(|w| w.contains("node_modules")),
            "{:?}",
            clean.warnings
        );
        // prune 该抓的还是要抓到：skill 与 session 都超期了。
        assert!(prune.actionable().count() >= 2);
    }

    #[test]
    fn uninstall_按清单根目录整棵树成项_且_install_翻转为可删() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let root = home.join(".claude");
        let cfg = home.join(".claude.json");
        // 清单没声明的子目录也在树里，uninstall 一并带走。
        let undeclared = root.join("computer-use");
        let sess = root.join("projects/p/a.jsonl");
        let plugins = root.join("plugins");
        mkdir(&undeclared);
        mkdir(&plugins);
        mkfile(&sess);
        mkfile(&cfg);

        let index = seed(
            home,
            &[
                row("claude-code", "session", "a.jsonl", &sess, 1000, NOW),
                row("claude-code", "install", "plugins", &plugins, 5000, NOW),
            ],
        );
        let o = opts(home, &index);

        let err = plan_uninstall(&o, "no-such-agent").unwrap_err().to_string();
        assert!(
            err.contains("claude-code"),
            "未知 id 必须列出已知 id：{err}"
        );

        let plan = plan_uninstall(&o, "claude-code").unwrap();
        assert_eq!(plan.verb, Verb::Uninstall);

        let roots: Vec<&PlanItem> = plan.items.iter().filter(|i| i.kind == "root").collect();
        assert_eq!(roots.len(), 2, "~/.claude 与 ~/.claude.json 各一项");
        let dir_item = roots.iter().find(|i| i.path == root).unwrap();
        assert_eq!(dir_item.action, Action::RemoveDir);
        assert!(dir_item.cleanable);
        assert!(dir_item.archived, "树内有会话，必须先归档");
        assert!(dir_item.bytes > 0);
        let file_item = roots.iter().find(|i| i.path == cfg).unwrap();
        assert_eq!(file_item.action, Action::RemoveFile);
        assert!(!file_item.archived, "配置文件树里没有会话/记忆");

        // install 在这里翻转成 cleanable，但体积只在根目录项上计一次。
        let inst = plan.items.iter().find(|i| i.kind == "install").unwrap();
        assert!(inst.cleanable, "uninstall 是唯一允许删软件本体的动词");
        assert_ne!(inst.action, Action::Keep);
        assert_eq!(inst.bytes, 0, "已在根目录项里计过，不许重复计");
        assert_eq!(plan.reclaim_bytes, dir_item.bytes + file_item.bytes);
        assert_eq!(plan.install_bytes, 5000);
        assert_eq!(plan.stale_bytes, 0);
        assert!(
            !plan.warnings.iter().any(|w| w.contains("M2")),
            "shared / package 已经落地，计划里不该再说它们排在 M2：{:?}",
            plan.warnings
        );
    }

    /// 上一条 `duster clean` 刚删掉的缓存目录还留在索引里。再出一次计划时
    /// 它必须整条消失——留着不只是噪声：路径已经不在，`is_dir()` 为假，
    /// 一个目录会被排成「删文件」，执行时必然报错。
    #[test]
    fn 索引里已消失的路径不进计划() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let alive = home.join(".codex/cache");
        let gone = home.join(".claude/cache");
        mkdir(&alive);
        // `gone` 故意不落盘：模拟 scan 之后被清掉。

        let index = seed(
            home,
            &[
                artifact("codex", "cache", &alive, 500, NOW, "l1", 500),
                artifact("claude-code", "cache", &gone, 900, NOW, "l1", 900),
            ],
        );
        let plan = plan_clean(&opts(home, &index)).unwrap();

        let keys: Vec<&str> = plan.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["cache"], "只剩还在盘上的那一条");
        assert_eq!(plan.items[0].path, alive);
        assert_eq!(plan.items[0].action, Action::RemoveDir);
        assert_eq!(plan.reclaim_bytes, 500, "消失的那条不许计进可回收量");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("no longer exist on disk")),
            "必须说明有索引行对不上磁盘: {:?}",
            plan.warnings
        );
        assert!(
            !plan.warnings.iter().any(|w| w.contains("duster scan")),
            "重扫救不了「盘上已经没有」,不许再叫用户去跑一遍: {:?}",
            plan.warnings
        );
    }

    /// 造一个声明了 `keep_generations` 的代际资源:10 份备份全部比 90d 阈值老,
    /// key 按 scan 的格式「声明路径 + 相对路径」,每行再写 keep_generations。
    /// 返回 (索引路径, 各代际的 key, 各代际的落盘路径);第 1 份最老、第 10 份最新。
    fn seed_generations(home: &Path, keep: u32) -> (PathBuf, Vec<String>, Vec<PathBuf>) {
        let dir = home.join(".cc-switch/backups");
        mkdir(&dir);
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut paths = Vec::new();
        for i in 1..=10u64 {
            let key = format!("~/.cc-switch/backups/backup-{i:02}.db");
            let path = dir.join(format!("backup-{i:02}.db"));
            mkfile(&path);
            // 全部严格超期:i=10 是 91 天前,阈值 90d 也判它 stale——
            // 测试要证的就是「整组超期时保留仍按数量,不按年龄」。
            rows.push(artifact(
                "cc-switch",
                &key,
                &path,
                1000 + i,
                NOW - (101 - i as i64) * DAY_MS,
                "l2",
                1000 + i,
            ));
            keys.push(key);
            paths.push(path);
        }
        let index = seed(home, &rows);
        let idx = Index::open(&index).unwrap();
        for key in &keys {
            upsert::set_keep_generations(
                idx.conn(),
                "cc-switch",
                "artifact",
                "global",
                key,
                Some(keep),
            )
            .unwrap();
        }
        drop(idx);
        (index, keys, paths)
    }

    /// 整组代际全部超期(每份都比 90d 老)时,保留仍按声明数量:
    /// 最新 2 份兜底,其余 8 份按数量冗余、与年龄无关地进计划。
    /// 这是「--keep-generations 绕开 --older-than」的合同测试。
    #[test]
    fn 代际资源_全部超期也保留最新两份_超编按数量清() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let (index, keys, _paths) = seed_generations(home, 2);
        let mut o = opts(home, &index);
        o.older_than_days = Some(90);
        o.keep_generations = true;
        let plan = plan_prune(&o).unwrap();

        let surplus: Vec<&PlanItem> = plan
            .items
            .iter()
            .filter(|i| i.generation && i.action != Action::Keep)
            .collect();
        assert_eq!(surplus.len(), 8, "10 份留 2 份,超编 8 份");
        assert_eq!(plan.actionable().count(), 8, "可执行的只有超编代际");

        for k in &keys[..8] {
            assert!(
                plan.actionable().any(|i| &i.key == k),
                "{k} 是第 N+1 份起,应可清"
            );
        }
        for k in &keys[8..] {
            assert!(
                !plan.actionable().any(|i| &i.key == k),
                "{k} 是最新 2 份,应保留"
            );
        }

        // 超编项的 why 必须自己说清「第几份、保留几份、与年龄无关」——
        // 读者可能带着 90d 阈值来看这份 91 天前的文件,行里没有这句话
        // 就解释不了它为什么在这。
        let oldest = surplus[0];
        assert!(oldest.why.contains("Copy 1 of 10"), "{}", oldest.why);
        assert!(oldest.why.contains("keeps the newest 2"), "{}", oldest.why);
        assert!(oldest.why.contains("not by age"), "{}", oldest.why);
        assert!(
            oldest.what.contains("surplus generation"),
            "{}",
            oldest.what
        );
        assert_ne!(oldest.action, Action::Keep);
        assert!(oldest.archived, "超编代际与按龄删除一样先归档");

        // 保留项:说「最新 N 份兜底」,整组都超期时不许套 fresh 模板
        // 说「在阈值内」——那是谎话。
        let kept: Vec<&PlanItem> = plan
            .items
            .iter()
            .filter(|i| i.action == Action::Keep && i.kind == "artifact")
            .collect();
        assert_eq!(kept.len(), 2);
        for k in kept {
            assert!(k.why.contains("newest 2"), "{}", k.why);
            assert!(
                !k.why.contains("within the --older-than"),
                "整组超期时保留项不许说在阈值内: {}",
                k.why
            );
            assert!(k.why.contains("even when every copy is past"), "{}", k.why);
        }

        // 回收量 = 8 份超编的体积之和,一份不多一份不少。
        let expect: u64 = (1u64..=8).map(|i| 1000 + i).sum();
        assert_eq!(plan.reclaim_bytes, expect);
    }

    /// 旗标关闭:代际资源一行都不进计划,只在收尾报告里点名旗标。
    /// 与 `install` 桶同一条发现路径——报出来,但一个字节都不碰。
    #[test]
    fn 代际资源_旗标关闭_零超编项_只出收尾警告() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let (index, _keys, _paths) = seed_generations(home, 2);
        let mut o = opts(home, &index);
        o.older_than_days = Some(90);
        o.keep_generations = false;
        let plan = plan_prune(&o).unwrap();

        assert_eq!(plan.actionable().count(), 0, "旗标关:一个超编项都不该有");
        assert!(
            plan.items.iter().all(|i| !i.generation),
            "旗标关:计划里不该出现代际项(连 keep 项都不出)"
        );
        // 收尾报告:数量 + 字节 + 旗标名,三样缺一不可。
        let w = plan
            .warnings
            .iter()
            .find(|w| w.contains("--keep-generations"))
            .expect("收尾报告必须点名 --keep-generations");
        assert!(w.contains("8 surplus generation"), "{w}");
        assert!(w.contains("keeps the newest 2"), "{w}");
        assert!(w.contains("Pass --keep-generations"), "{w}");
    }

    /// 旗标关时,普通 l2(没声明 keep_generations)照旧按年龄清——
    /// 「off = 今天的行为」这句话必须对普通资源一字不差地成立;
    /// 代际资源则一行都不出。
    #[test]
    fn 旗标关_普通l2照旧按年龄清_代际不动() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        // 普通 l2:同一父目录下两份兄弟文件声明(单目录声明只会有一行,
        // 永远 keep 自己——这正是 T1 要修掉的粒度问题),老的那份按 90d 清。
        let d = home.join(".codex/backups");
        mkdir(&d);
        let old = d.join("a.bak");
        let fresh = d.join("b.bak");
        mkfile(&old);
        mkfile(&fresh);
        let (index, _keys, _paths) = seed_generations(home, 2);
        let idx = Index::open(&index).unwrap();
        for (k, p, mtime) in [
            ("a.bak", &old, NOW - 200 * DAY_MS),
            ("b.bak", &fresh, NOW - 5 * DAY_MS),
        ] {
            upsert_resource(idx.conn(), &artifact("codex", k, p, 500, mtime, "l2", 500)).unwrap();
        }
        drop(idx);

        let mut o = opts(home, &index);
        o.older_than_days = Some(90);
        o.keep_generations = false;
        let plan = plan_prune(&o).unwrap();

        assert!(
            plan.actionable().any(|i| i.key == "a.bak"),
            "普通 l2 的年龄清理不该受旗标影响"
        );
        assert!(
            !plan.actionable().any(|i| i.key == "b.bak"),
            "最新的那份兜底"
        );
        assert!(plan.items.iter().all(|i| !i.generation), "旗标关:代际零项");
        assert_eq!(plan.reclaim_bytes, 500);
    }

    /// 造一条计划项。可行动与否 = `cleanable && action != Keep`。
    fn filter_item(path: &str, bytes: u64, action: Action, cleanable: bool) -> PlanItem {
        PlanItem {
            agent_id: "fixture".into(),
            kind: "artifact".into(),
            clean_level: None,
            path: PathBuf::from(path),
            key: path.into(),
            rid: -1,
            what: String::new(),
            why: String::new(),
            impact: String::new(),
            archived: false,
            action,
            bytes,
            cleanable,
            generation: false,
            last_used_ms: None,
        }
    }

    /// 用一组计划项造一份计划。`reclaim_bytes` 故意给全集之和这个**错误值**：
    /// 断言必须证明 `apply` **重算**了它，而不是碰巧没动。
    fn filter_plan(items: Vec<PlanItem>) -> Plan {
        let reclaim_bytes = items.iter().map(|i| i.bytes).sum();
        Plan {
            verb: Verb::Clean,
            items,
            reclaim_bytes,
            install_bytes: 0,
            stale_bytes: 0,
            warnings: Vec::new(),
        }
    }

    fn kept_paths(plan: &Plan) -> Vec<PathBuf> {
        plan.items.iter().map(|i| i.path.clone()).collect()
    }

    /// `PlanFilter::apply` 的三档语义：只有 skip、只有 allow、两者同时
    /// （allow ∩ ¬skip）。可行动项刻意少于全部剩余项（Keep 行与 not
    /// cleanable 行在场）——`reclaim_bytes` 必须重算成过滤后 `actionable()`
    /// 的和，抄错数字立刻对不上。
    #[test]
    fn plan_filter_三档语义各归各位_且重算reclaim() {
        let a = filter_item("/h/a/cache", 100, Action::RemoveDir, true);
        let b = filter_item("/h/b/cache", 200, Action::RemoveDir, true);
        let keep = filter_item("/h/c/cache", 400, Action::Keep, true); // 不可行动
        let install = filter_item("/h/install", 999, Action::Keep, false); // 永不清理
        let all = vec![a.clone(), b.clone(), keep.clone(), install.clone()];

        // 档一：只有 skip。b 被剔除，keep/install 不过滤、留着展示。
        let mut plan = filter_plan(all.clone());
        PlanFilter::skipping(vec![b.path.clone()]).apply(&mut plan);
        assert_eq!(
            kept_paths(&plan),
            vec![a.path.clone(), keep.path.clone(), install.path.clone()]
        );
        assert_eq!(
            plan.reclaim_bytes, 100,
            "skip 后 reclaim 只算留下的可行动项"
        );

        // 档二：只有 allow。勾了 a、b，keep/install 不在名单上，被剔除。
        let mut plan = filter_plan(all.clone());
        PlanFilter::allow_only(vec![
            (a.path.clone(), Action::RemoveDir),
            (b.path.clone(), Action::RemoveDir),
        ])
        .apply(&mut plan);
        assert_eq!(kept_paths(&plan), vec![a.path.clone(), b.path.clone()]);
        assert_eq!(plan.reclaim_bytes, 300, "allow 后 reclaim 只算勾过的");

        // 档三：两者同时。b 在 skip 里，勾过也动不得；a 放行。
        let mut plan = filter_plan(all);
        let both = PlanFilter {
            skip: vec![b.path.clone()],
            allow: Some(vec![
                (a.path.clone(), Action::RemoveDir),
                (b.path.clone(), Action::RemoveDir),
            ]),
            only_kind: None,
        };
        both.apply(&mut plan);
        assert_eq!(kept_paths(&plan), vec![a.path.clone()], "skip 优先于 allow");
        assert_eq!(plan.reclaim_bytes, 100);
    }

    /// allow 里放一个计划里根本不存在的路径：不 panic、不误伤——结果
    /// 等同于 allow 只含交集。幽灵路径只可能缩小名单，不可能扩大。
    #[test]
    fn plan_filter_allow里的幽灵路径不panic不误伤() {
        let a = filter_item("/h/a/cache", 100, Action::RemoveDir, true);
        let b = filter_item("/h/b/cache", 200, Action::RemoveDir, true);
        let mut plan = filter_plan(vec![a.clone(), b.clone()]);

        PlanFilter::allow_only(vec![
            (a.path.clone(), Action::RemoveDir),
            (PathBuf::from("/h/never-existed"), Action::RemoveDir),
        ])
        .apply(&mut plan);

        assert_eq!(kept_paths(&plan), vec![a.path.clone()]);
        assert_eq!(plan.reclaim_bytes, 100, "幽灵路径不该改变回收量");
    }

    /// 白名单的键是「路径 + 动作」二元组：用户勾的是 `vacuum`（无损压实），
    /// 重算后同一条路径的动作变成了 `remove_file`（真删）——只比路径就会
    /// 放行「见过的项换了刀」，fail-closed 必须整条剔除。
    #[test]
    fn allow_同路径换了动作就不放行() {
        let item = filter_item("/h/a/db.sqlite", 100, Action::RemoveFile, true);
        let mut plan = filter_plan(vec![item.clone()]);

        PlanFilter::allow_only(vec![(item.path.clone(), Action::Vacuum)]).apply(&mut plan);

        assert!(
            plan.items.is_empty(),
            "同路径动作不一致就不该放行：{:?}",
            plan.items
        );
        assert_eq!(plan.reclaim_bytes, 0, "reclaim 不能含被剔除的项");
    }

    /// `only_kind` 与 `skip` / `allow` 三者同时生效时是交集：kind 对、
    /// 不在 skip、allow 里勾过（含动作），三条全过才留。勾了但 kind 不对
    /// 的项同样出局——`only_kind` 是入口的职责，用户的勾选洗不掉它。
    #[test]
    fn only_kind_只放行同_kind_的项() {
        let mut s1 = filter_item("/h/s/one.jsonl", 100, Action::CompressFile, true);
        s1.kind = "session".into();
        let mut s2 = filter_item("/h/s/two.jsonl", 200, Action::CompressFile, true);
        s2.kind = "session".into();
        let mut s3 = filter_item("/h/s/three.jsonl", 300, Action::CompressFile, true);
        s3.kind = "session".into();
        let art = filter_item("/h/a/cache", 400, Action::RemoveDir, true);
        let all = vec![s1.clone(), s2.clone(), s3.clone(), art.clone()];
        let mut plan = filter_plan(all);
        let f = PlanFilter {
            skip: vec![s3.path.clone()],
            allow: Some(vec![
                (s1.path.clone(), Action::CompressFile),
                (s2.path.clone(), Action::CompressFile),
                // 勾进 allow 也没用：`only_kind` 是入口的职责，用户洗不掉。
                (art.path.clone(), Action::RemoveDir),
            ]),
            only_kind: Some("session"),
        };
        f.apply(&mut plan);

        assert_eq!(
            kept_paths(&plan),
            vec![s1.path.clone(), s2.path.clone()],
            "s3 被 skip 挡、artifact 被 only_kind 挡，只剩 s1/s2"
        );
        assert_eq!(plan.reclaim_bytes, 300);
    }
}
