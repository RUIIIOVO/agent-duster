//! skill 的横向操作：跨 agent 去重 / 漂移检测 / 链接。
//!
//! 同一份 skill 常常在三四个 agent 目录下各躺一份。五种状态要分清：
//! - **IDENTICAL**：树哈希全等。可以链接成一份，省的是体积也是维护成本。
//! - **DRIFTED**：同名不同哈希。**这是要报出来的那个**——用户以为三处一样，
//!   实际上早就各自改过了，改了哪儿得给出 diff。
//! - **SINGLE**：全机只有这一份，没有可比对象。照样列出来——`skill list`
//!   回答的是「我有哪些 skill」，本机实测 34 个名字里 21 个只有一份，滤掉它们
//!   就只剩 13 行，「列出全部 skill」这句话直接是假的。
//! - **LINKED**：副本根路径是软链，或**整份目录只含软链**（`connect-chrome` /
//!   `open-gstack-browser` 就是 SKILL.md 软链指向 gstack 的目录），且目标
//!   解析成功。这类副本没有自己的内容，不该拿 0 字节的哈希去和实体副本比——
//!   本机指向 gstack 的一票软链，旧逻辑不 follow 读出 0 字节、树哈希全相同
//!   （只含软链的目录文件表为空，哈希必然全等），整组被误判成 identical。
//! - **BROKEN**：软链悬空（根软链，或只含软链的目录里存在悬空链）。skill
//!   名下这份"副本"实际不存在，是最该先修的一档。
//!
//! 树哈希一律**剪掉 install 子路径**（`node_modules` / `dist` / `bin` / `.git`）。
//! 理由和 status 体积口径是同一个：本机 `gstack` 一个 skill 就 1.1 GB，
//! 其中 721 MB 是 `node_modules`——两处 `npm install` 的产物必然不同，
//! 不剪枝的话每一组同名 skill 都会被判成 DRIFTED，检测直接失效。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::manifest;
use duster_fs::hash::hash_tree;
use duster_fs::walk::{WalkOptions, walk_files, walk_stats};
use duster_index::db::Index;
use duster_index::query;
use duster_model::ResourceKind;

use crate::delete::{DeleteOptions, DeleteReport};
use crate::diff::{Change, DiffOptions, diff_trees};
use crate::freshness;

/// 一组同名 skill 的比较结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DupState {
    /// 全机只有这一份，没有可比对象。
    ///
    /// 不并进 `Identical`：一份副本说「完全相同」是句胡话——和谁相同？
    /// 读者会以为自己漏看了另一行，回头去数表格。
    Single,
    /// 树哈希全等。
    Identical,
    /// 同名不同哈希。
    Drifted,
    /// 副本根路径是软链，或整份目录只含软链（如 SKILL.md 软链指向 gstack
    /// 的 `connect-chrome`），且目标能解析到有效位置（组内另一份或别处的
    /// 实体）。
    ///
    /// 这类副本没有自己的内容，不该参与 identical/drifted 的哈希比较——
    /// 不 follow 读出 0 字节、树哈希全相同（只含软链的目录文件表为空，
    /// 哈希必然全等），正是本机把一堆指向 gstack 的软链误判成 identical
    /// 的病根。判它只认目标能不能解析，不认内容。
    Linked,
    /// 副本根路径是软链，或整份目录只含软链，且其中存在悬空链（目标被删了 /
    /// 从没存在过）。
    ///
    /// 悬空链是坏账：skill 名下这份"副本"实际不存在。比 Drifted 更该报，
    /// 组级状态里它压过一切——先修了坏链再谈比较。
    Broken,
}

/// 一份副本。
#[derive(Debug, Clone, Serialize)]
pub struct SkillCopy {
    pub agent_id: String,
    pub path: PathBuf,
    /// 这份副本自己的状态：软链副本是 `Linked`/`Broken`，普通目录副本
    /// 跟随组级比较结果（`Identical`/`Drifted`/`Single`）。
    pub state: DupState,
    /// 软链指向的目标（根软链，或整份只含软链的副本）：`Linked` 是解析后的
    /// 绝对路径，`Broken` 是 `read_link` 原文（目标已消失、解析不了，原文是
    /// 最后一份记录）。普通目录副本为 `None`，JSON 里整个字段省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<PathBuf>,
    /// 剪枝后的树哈希（hex）。软链副本不参与比较，恒为空串。
    pub tree_hash: String,
    /// 剪枝后的用户内容体积。
    pub bytes: u64,
    /// 目录内嵌的 install 子路径体积（不参与哈希，单独报出来）。
    pub install_bytes: u64,
    /// 「上次使用」的证据：索引行 mtime 的 Unix 毫秒；`None` = scan 当时没取到
    /// 时间戳（没有证据，不是「1970 年用过」）。口径与 session 列表的
    /// LAST USED 列、prune 的陈旧判定同源（`ResourceRecord::last_used_ms`，
    /// 只有这一份实现）。
    ///
    /// 软链副本（Linked / Broken）记的是**链本身**的 mtime，不是目标的——
    /// 磁盘上这一行只有链的 stat，跟随软链去 stat 目标会把那份内容在
    /// 两个名字下各统计一次。内容归目标自己那一行管，这里只对「这一份
    /// 副本的文件系统痕迹」负责。
    pub last_used_ms: Option<i64>,
}

/// 一组同名 skill。
#[derive(Debug, Clone, Serialize)]
pub struct SkillGroup {
    pub name: String,
    pub state: DupState,
    pub copies: Vec<SkillCopy>,
    /// DRIFTED 时的差异摘要：哪些文件只在某一侧、哪些文件内容不同。
    /// 逐文件列出，不做行级 diff（行级 diff 是 M2 的通用 diff 引擎）。
    pub diff: Option<String>,
    /// 本组内**没能参与比较**的副本各自的原因（目录已消失、读不动）。
    ///
    /// 索引是派生数据、文件系统才是真相：上次扫描后被删掉的副本不该让
    /// 整轮 copies 崩掉，但也不能悄悄消失——组还在，少了谁要说清楚。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// 跨 agent 扫描全部 skill，按名分组比较。**每个名字都返回一组**，只有一份的
/// 也在内——本机实测 34 个名字里 21 个只有一份，滤掉就只剩 13 行。
///
/// 读索引拿路径，现场算树哈希（索引里的 `hash_content` 对 skill 恒为空——
/// 目录树哈希开销大，只在这里按需算）。
///
/// 单份组照走 `measure_copy`：体积要报，`tree_hash` 照填。给它另开一条
/// 「只量体积、不算哈希」的快路径，省下的是本机 0.35s → 0.5s 这一档，
/// 换来的是两条测量口径迟早走散——不值。
///
/// 分组键是**声明的 skill 名**而不是目录名：同一个 agent 里两个目录声明同一个
/// 名字是真实存在的（本机 `open-gstack-browser` 就在 claude-code 下有
/// `connect-chrome` 与同名目录两份），它们必须落进同一组才比得出漂移。
pub fn list(index_path: Option<&std::path::Path>) -> Result<Vec<SkillGroup>> {
    let idx = open_index(index_path)?;
    let groups = duster_index::query::skill_groups(idx.conn())?;
    let prune = install_prune();

    let mut out: Vec<SkillGroup> = Vec::new();
    for (name, rows) in groups {
        let mut copies: Vec<SkillCopy> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for r in &rows {
            let root = PathBuf::from(&r.path);
            // symlink_metadata 不 follow：根路径到底是软链还是真目录，只有它
            // 说了算。is_dir() 会跟着软链走，悬空链判成"消失"、有效链读出
            // 0 字节，两种都被它骗过去——软链的判定必须看链接本身。
            let meta = match std::fs::symlink_metadata(&root) {
                Ok(m) => m,
                Err(_) => {
                    warnings.push(format!(
                        "copy for agent `{}` is gone from disk: {} (something removed it \
                         outside duster after it was indexed; re-scanning only drops the row too)",
                        r.agent_id, r.path
                    ));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                // 软链副本：不 follow、不量体积、不参与哈希比较。状态只看
                // 目标能不能解析——能就是 linked（记下指向哪），不能就是
                // broken。悬空时 canonicalize 失败，退回 read_link 原文：
                // 目标已删，这份原文是"它曾经想指向哪"的最后记录。
                let resolved = std::fs::canonicalize(&root).ok();
                let state = if resolved.is_some() {
                    DupState::Linked
                } else {
                    DupState::Broken
                };
                let link_target = resolved.or_else(|| std::fs::read_link(&root).ok());
                copies.push(SkillCopy {
                    agent_id: r.agent_id.clone(),
                    path: root,
                    state,
                    link_target,
                    tree_hash: String::new(),
                    bytes: 0,
                    install_bytes: 0,
                    last_used_ms: r.last_used_ms(),
                });
                continue;
            }
            if !meta.is_dir() {
                // 存在但不是目录（普通文件占了位）：与"消失"同样降级成警告。
                warnings.push(format!(
                    "copy for agent `{}` is not a directory: {}",
                    r.agent_id, r.path
                ));
                continue;
            }
            match measure_copy(&root, &prune) {
                Ok(m) => {
                    // 只含软链的 copy：内容住在别处（通常指向另一份实体），
                    // 既不是独立副本也不该参与 identical/drifted 比较——目录
                    // 文件表为空时哈希必然全等，这正是 connect-chrome /
                    // open-gstack-browser 假 identical 的病根。整份判
                    // Linked（每条软链都能解析）或 Broken（任一悬空）。
                    let (state, link_target, tree_hash) =
                        if m.regular_count == 0 && !m.symlinks.is_empty() {
                            let mut syms = m.symlinks;
                            syms.sort_by(|a, b| a.rel.cmp(&b.rel));
                            // 解析只在需要时做：夹着常规文件的零星软链是
                            // 噪音，不判状态，省掉那几次 canonicalize。
                            let resolved: Vec<Option<PathBuf>> = syms
                                .iter()
                                .map(|s| std::fs::canonicalize(root.join(&s.rel)).ok())
                                .collect();
                            let all_resolve = resolved.iter().all(Option::is_some);
                            (
                                if all_resolve {
                                    DupState::Linked
                                } else {
                                    DupState::Broken
                                },
                                // 按相对路径排序取第一条：Linked 记解析后的
                                // 目标，Broken 记 read_link 原文（目标已消失，
                                // 原文是最后一份记录）。
                                if all_resolve {
                                    resolved[0].clone()
                                } else {
                                    syms[0].target_raw.clone()
                                },
                                // 不参与比较：树哈希置空，跟根软链副本同一口径。
                                String::new(),
                            )
                        } else {
                            (DupState::Single, None, m.tree_hash)
                        };
                    copies.push(SkillCopy {
                        agent_id: r.agent_id.clone(),
                        path: root,
                        state,
                        link_target,
                        tree_hash,
                        bytes: m.bytes,
                        install_bytes: m.install_bytes,
                        last_used_ms: r.last_used_ms(),
                    });
                }
                // 单份读不动（权限、坏链）同样降级成警告：别让一个副本
                // 拖垮整组，更别拖垮整轮。
                Err(e) => warnings.push(format!(
                    "failed to hash copy for agent `{}` at {}: {e:#}",
                    r.agent_id, r.path
                )),
            }
        }

        // 状态按**现场量到的**副本数判，不按索引行数：索引说两份、磁盘上
        // 只剩一份时（上面那条警告），说它 identical 等于拿一份副本和自己比，
        // 而用户眼前确实只有一行。
        //
        // 软链副本不参与 identical/drifted：普通目录单独拿出来比，组级状态
        // 再让悬空链压过一切——broken 是坏账，先修它；全是软链没得比就整个
        // 报 linked。
        let regular_state = {
            let regular: Vec<&SkillCopy> = copies
                .iter()
                .filter(|c| !matches!(c.state, DupState::Linked | DupState::Broken))
                .collect();
            if regular.len() < 2 {
                DupState::Single
            } else if regular.iter().any(|c| c.tree_hash != regular[0].tree_hash) {
                DupState::Drifted
            } else {
                DupState::Identical
            }
        };
        let state = if copies.iter().any(|c| c.state == DupState::Broken) {
            DupState::Broken
        } else if !copies.is_empty() && copies.iter().all(|c| c.state == DupState::Linked) {
            DupState::Linked
        } else {
            regular_state
        };
        // 普通副本的逐行状态跟随比较结果；软链副本保持 Linked/Broken 不动。
        for c in &mut copies {
            if !matches!(c.state, DupState::Linked | DupState::Broken) {
                c.state = regular_state;
            }
        }
        let diff = if state == DupState::Drifted {
            Some(build_diff(&copies, &prune, &mut warnings)?)
        } else {
            None
        };

        out.push(SkillGroup {
            name,
            state,
            copies,
            diff,
            warnings,
        });
    }
    Ok(out)
}

/// 链接方式。降级是允许的，但**降级原因必须明示**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// 首选：CAS store 里一份实体，各 agent 目录硬链过去。
    Hardlink,
    /// 跨文件系统时降级。
    Symlink,
    /// 目标 agent 不认符号链接时降级；此后靠漂移监控发现两份走散。
    Copy,
}

impl LinkMode {
    /// 降级链的下一档；已经是最后一档返回 None。
    fn next(self) -> Option<Self> {
        match self {
            LinkMode::Hardlink => Some(LinkMode::Symlink),
            LinkMode::Symlink => Some(LinkMode::Copy),
            LinkMode::Copy => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LinkMode::Hardlink => "hardlink",
            LinkMode::Symlink => "symlink",
            LinkMode::Copy => "copy",
        }
    }
}

/// 链接结果。
#[derive(Debug, Clone, Serialize)]
pub struct LinkReport {
    pub name: String,
    /// 源副本路径（CAS 入库前的实体）。
    pub from: PathBuf,
    /// 目标 agent 的 skill 目录。
    pub to: PathBuf,
    /// CAS store 里的实体路径。
    pub cas_path: PathBuf,
    pub mode: LinkMode,
    /// 非 Hardlink 时**必填**：为什么降级。
    pub downgrade_reason: Option<String>,
    /// 链接后是否实测目标 agent 能加载（SKILL.md 可读 + frontmatter 可解析）。
    pub verified: bool,
    pub warnings: Vec<String>,
}

/// CAS store 根目录：`~/.agent-duster/cas`。
///
/// 按内容树哈希分目录（`cas/<hex>/`），同一份 skill 无论被几个 agent 引用
/// 都只有一份实体。硬链的引用计数由文件系统维护，duster 不自己记账。
pub fn cas_root(home: &std::path::Path) -> PathBuf {
    home.join(".agent-duster").join("cas")
}

/// 把 `name` 这个 skill 从 `from_agent` 链接到 `to_agent`。
///
/// 步骤：
/// 1. 源副本入 CAS（内容不变则复用已有实体）；
/// 2. 目标目录已存在同名 skill → 报错，要求先 `prune` 或改名，**绝不覆盖**；
/// 3. 硬链 → 失败（跨设备 EXDEV）降级软链 → 再失败降级副本，逐级记原因；
/// 4. **链接后实测**：目标路径下 `SKILL.md` 可读且 frontmatter 可解析。
///    验证不过就回滚这次链接，不留半个坏 skill。
pub fn link(
    index_path: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
    name: &str,
    from_agent: &str,
    to_agent: &str,
    dry_run: bool,
) -> Result<LinkReport> {
    let home = resolve_home(home)?;
    let idx = open_index(index_path)?;
    let groups = duster_index::query::skill_groups(idx.conn())?;

    let Some(rows) = groups.get(name) else {
        bail!(
            "unknown skill `{name}`; indexed skills: {}",
            join_or_none(groups.keys().map(|s| s.as_str()))
        );
    };
    let Some(src) = rows.iter().find(|r| r.agent_id == from_agent) else {
        bail!(
            "skill `{name}` is not installed for agent `{from_agent}`; it exists in: {}",
            join_or_none(rows.iter().map(|r| r.agent_id.as_str()))
        );
    };
    let src_root = PathBuf::from(&src.path);
    if !src_root.is_dir() {
        bail!(
            "source skill directory is gone: {} (something removed it outside duster \
             after it was indexed; there is nothing left to link from)",
            src.path
        );
    }

    let prune = install_prune();
    let hex = hash_tree(&src_root, &prune)
        .with_context(|| format!("failed to hash source skill: {}", src_root.display()))?
        .to_hex()
        .to_string();
    let cas_dir = cas_root(&home).join(&hex);

    let skill_root = agent_skill_root(&home, to_agent)?;
    let target = skill_root.join(name);
    // 存在即拒绝，连"内容一样就跳过"这种体贴都不做：目标目录里那份
    // 可能正是用户改过的那份，覆盖掉就找不回来了。
    if std::fs::symlink_metadata(&target).is_ok() {
        bail!(
            "target already exists: {} — run `duster prune` or rename it first; \
             duster never overwrites an existing skill",
            target.display()
        );
    }

    if dry_run {
        // 干跑必须**一个字节都不落盘**：CAS 目录也不建。
        return Ok(LinkReport {
            name: name.to_string(),
            from: src_root,
            to: target,
            cas_path: cas_dir,
            mode: LinkMode::Hardlink,
            downgrade_reason: None,
            verified: false,
            warnings: vec![
                "dry run: nothing was created; re-run without --dry-run to apply".to_string(),
            ],
        });
    }

    let mut warnings: Vec<String> = Vec::new();
    ingest_cas(&src_root, &cas_dir, &prune, &hex, &mut warnings)?;
    std::fs::create_dir_all(&skill_root).with_context(|| {
        format!(
            "failed to create skill directory for agent `{to_agent}`: {}",
            skill_root.display()
        )
    })?;

    let mut mode = LinkMode::Hardlink;
    let mut reasons: Vec<String> = Vec::new();
    loop {
        let attempt = match mode {
            LinkMode::Hardlink => hardlink_tree(&cas_dir, &target),
            LinkMode::Symlink => symlink_dir(&cas_dir, &target),
            LinkMode::Copy => copy_tree(&cas_dir, &target, &[]),
        };
        // 建完立刻实测加载：软链能不能被目标 agent 跟随，只有这一步说了算。
        match attempt.and_then(|()| verify_loadable(&target, name)) {
            Ok(()) => break,
            Err(e) => {
                // 半个坏 skill 比没有 skill 更糟，降级前先把残留清干净。
                remove_link(&target);
                let reason = format!("{} failed: {e:#}", mode.label());
                match mode.next() {
                    Some(next) => {
                        reasons.push(reason);
                        mode = next;
                    }
                    None => {
                        reasons.push(reason);
                        bail!(
                            "failed to link skill `{name}` into agent `{to_agent}`; \
                             rolled back, nothing left at {}. Attempts: {}",
                            target.display(),
                            reasons.join("; ")
                        );
                    }
                }
            }
        }
    }

    Ok(LinkReport {
        name: name.to_string(),
        from: src_root,
        to: target,
        cas_path: cas_dir,
        mode,
        // 只在真降级过才有值；Hardlink 成功时 reasons 必空。
        downgrade_reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        verified: true,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// 一份副本的现场形态。删除方式由它决定。
///
/// 软链副本（Linked / Broken）**没有自己的内容**——内容住在链接指向的地方
/// （另一家 agent 的副本，或 `duster skill link` 建的 CAS 实体）。删这种
/// 副本只准删链接本身，**绝不跟随链去删目标**：目标可能是另一家正在用的
/// 那一份，也可能是几家共享的本体。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    /// 根路径是软链。只 unlink 链接本身。
    Symlink,
    /// 目录但没有任何常规文件（只含软链，或空目录）。内容同样住在别处：
    /// 删目录（里面的软链原样随删，不跟随）。
    NoContent,
    /// 目录且含常规文件：真实内容，整棵删。
    RealDir,
}

/// `duster skill rm`：删除一个 skill 的一份副本。
///
/// # 指名与「别默认删全部」
///
/// 同一个 skill 名装在多家 agent 里时，`--agent` **必选**：缺了就报错并
/// 列出候选，**绝不默认删全部**——用户刚看完 `skill list` 想删 claude-code
/// 那份，duster 却把他三家全删了，没有比这更糟的默认值。全机只有一份时
/// 不必指名（没什么可歧义的）。
///
/// **同一 agent 名下同名多份**（目录不同，本机 `open-gstack-browser` 在
/// claude-code 里就有 `connect-chrome` 与 `open-gstack-browser` 两个目录）
/// 走同一条铁律：`--agent` 收窄之后仍剩多份，就要 `path` 再指一次，缺了
/// 报错列路径。理由与上一段逐字相同——「指名一家」不等于「同意删掉那家
/// 的每一份」，而这两份的内容可以完全不同。
///
/// # 为什么真删、不归档
///
/// `skill rm` 是对着**一个点名的东西**下手：`--agent` 必给，同一 agent
/// 名下多份时 `--path` 还必给，交互里还有 y/N——用户已经指名道姓同意删
/// 这一份了。显式的删除不需要暗中留副本；批量按龄清理的 `duster prune`
/// 仍然会给 skill 打包进 `~/agent-duster-exports/`，那才是需要退路的场合。
/// 所以这里真删：目录整棵删，一个字节都不留。
///
/// # 三种形态，三种删法
///
/// - **RealDir**（普通目录，有真实内容）：整棵删，**不归档**——删的是
///   点名的副本，不是批量清扫，理由见上。
/// - **Symlink / NoContent**（根软链，或只含软链的目录）：只删链接本身，
///   不归档。没自己的内容可归——归一个断链或归一条指向别处的链，都是
///   假安全感；内容在别处，删链不丢东西。
///
/// 删完清索引行（Contract 3：不留幽灵行，下一次 `skill list` 不再列出）。
pub fn remove(
    opts: &DeleteOptions,
    name: &str,
    agent: Option<&str>,
    path: Option<&str>,
) -> Result<DeleteReport> {
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = match &opts.index_path {
        Some(p) => p.clone(),
        None => crate::scan::default_index_path(&home),
    };
    let idx = Index::open(&index_path)
        .with_context(|| format!("failed to open index for writing: {}", index_path.display()))?;
    let conn = idx.conn();

    let groups = duster_index::query::skill_groups(conn)?;
    let Some(rows) = groups.get(name) else {
        bail!(
            "unknown skill `{name}`; indexed skills: {}",
            join_or_none(groups.keys().map(|s| s.as_str()))
        );
    };

    // 候选：先按 --agent 收窄，再按 path 收窄。每一步收窄之后仍剩多份就
    // 报错列候选，绝不默认删全部——收窄的粒度是「agent 有几家」与「这一家
    // 有几份」两个独立问题，各要用户答一次。
    let by_agent: Vec<&duster_index::query::ResourceRecord> = match agent {
        Some(a) => {
            let hits: Vec<_> = rows.iter().filter(|r| r.agent_id == a).collect();
            if hits.is_empty() {
                bail!(
                    "skill `{name}` is not installed for agent `{a}`; it exists in: {}",
                    join_or_none(distinct_agents(rows).into_iter())
                );
            }
            hits
        }
        None => {
            let agents = distinct_agents(rows);
            if agents.len() > 1 {
                bail!(
                    "skill `{name}` is installed for {} agents ({}). Pass --agent to pick \
                     one — duster never deletes all copies of a shared skill",
                    agents.len(),
                    join_or_none(agents.into_iter())
                );
            }
            rows.iter().collect::<Vec<_>>()
        }
    };

    let copies: Vec<&duster_index::query::ResourceRecord> = match path {
        Some(p) => {
            // 两种写法都认:列表把 PATH 折成 `~/...` 印出来,而索引里存的是
            // 绝对路径。只认绝对路径的话,用户从表里复制一行粘过来必然失配
            // ——「表里印什么就能拿来当参数」是这一列存在的前提。
            let want = expand_tilde_at(&home, p);
            let hits: Vec<_> = by_agent
                .iter()
                .copied()
                .filter(|r| r.path == p || Path::new(&r.path) == want)
                .collect();
            if hits.is_empty() {
                bail!(
                    "no copy of skill `{name}` at `{p}`; candidates: {}",
                    join_or_none(by_agent.iter().map(|r| r.path.as_str()))
                );
            }
            hits
        }
        None => {
            if by_agent.len() > 1 {
                bail!(
                    "agent `{}` keeps {} copies of skill `{name}` in different directories \
                     ({}). Name the one to delete with --path — picking an agent is not the \
                     same as agreeing to delete every copy it keeps",
                    by_agent[0].agent_id,
                    by_agent.len(),
                    join_or_none(by_agent.iter().map(|r| r.path.as_str()))
                );
            }
            by_agent
        }
    };

    let mut report = DeleteReport::default();
    let mut targets: Vec<(i64, PathBuf, CopyKind)> = Vec::new();

    // 第一遍：现场分类。分类失败只作废那一条。
    for rec in &copies {
        let root = PathBuf::from(&rec.path);
        match classify_copy(&root) {
            Ok(kind) => targets.push((rec.rid, root, kind)),
            Err(e) => report
                .warnings
                .push(format!("skill `{name}` for agent `{}`: {e:#}; skipped", rec.agent_id)),
        }
    }

    if targets.is_empty() {
        return Ok(report);
    }

    // 干跑：只报将删什么。`removed` 在这条路径上读作「将删」。skill 删除
    // 不归档（见函数文档），所以这里没有什么「将归档到哪」要报。
    if opts.dry_run {
        report.removed = targets.iter().map(|(_, p, _)| p.clone()).collect();
        report.freed_bytes = targets
            .iter()
            .filter(|(_, _, k)| *k == CopyKind::RealDir)
            .map(|(_, p, _)| dir_bytes(p))
            .sum();
        return Ok(report);
    }

    // 第二遍：真删。每条的失败只作废它自己。
    for (rid, root, kind) in &targets {
        match delete_one_copy(root, *kind) {
            Ok(freed) => {
                report.removed.push(root.clone());
                report.freed_bytes += freed;
                if let Err(e) = query::delete_resource(conn, *rid) {
                    report.warnings.push(format!(
                        "skill `{name}` was deleted but the index entry could not be \
                         removed: {e:#} (a rescan will clean it up)"
                    ));
                }
            }
            Err(e) => report
                .warnings
                .push(format!("skill `{name}` at {}: {e:#}; kept", root.display())),
        }
    }
    Ok(report)
}

/// 现场判定一份副本的形态。读不到路径（被外力删了）按 NoContent 处理——
/// 没有真实内容可删，删的动作会落空，索引行照清。
fn classify_copy(root: &Path) -> Result<CopyKind> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(_) => return Ok(CopyKind::NoContent),
    };
    if meta.file_type().is_symlink() {
        // 根软链：Linked 或 Broken 都只删链接本身。
        return Ok(CopyKind::Symlink);
    }
    if !meta.is_dir() {
        // 存在但不是目录（普通文件占了位）：不是一份 skill，别删内容，
        // 按「没有可删的真实内容」处理。
        return Ok(CopyKind::NoContent);
    }
    // 目录：有没有常规文件？只含软链的目录（connect-chrome 这类转发副本）
    // 内容住在别处，删掉只是摘链接。软链按链接本身计，不跟随。
    let mut has_regular = false;
    let mut bytes = 0u64;
    walk_files(
        root,
        &WalkOptions {
            follow_links: false,
            prune_dirs: Vec::new(),
        },
        |_, m| {
            if m.is_file() {
                has_regular = true;
                bytes += m.len();
            }
        },
    )
    .with_context(|| format!("failed to walk skill directory: {}", root.display()))?;
    let _ = bytes;
    if has_regular {
        Ok(CopyKind::RealDir)
    } else {
        Ok(CopyKind::NoContent)
    }
}

/// 删一份副本，返回释放的字节数。
///
/// - Symlink：unlink 链接本身，**绝不跟随**（`remove_file` 只删目录项，
///   目标一个字节都不碰）。
/// - NoContent：删目录（内含的软链原样随删）。已经不在盘上 = 视为已删。
/// - RealDir：整棵删（`remove_dir_all` 不跟随内部软链，它们按链接删掉）。
fn delete_one_copy(root: &Path, kind: CopyKind) -> Result<u64> {
    match kind {
        CopyKind::Symlink => {
            let freed = 0;
            match std::fs::remove_file(root) {
                Ok(()) => Ok(freed),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(freed),
                Err(e) => Err(e)
                    .with_context(|| format!("failed to unlink skill symlink: {}", root.display())),
            }
        }
        CopyKind::NoContent => {
            let freed = 0;
            match std::fs::remove_dir_all(root) {
                Ok(()) => Ok(freed),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(freed),
                Err(e) => Err(e).with_context(|| {
                    format!("failed to remove skill directory: {}", root.display())
                }),
            }
        }
        CopyKind::RealDir => {
            let freed = dir_bytes(root);
            std::fs::remove_dir_all(root)
                .with_context(|| format!("failed to remove skill directory: {}", root.display()))?;
            Ok(freed)
        }
    }
}

/// 目录内常规文件的体积和。读不到的路径按 0 计（与 uninstall 的实测口径
/// 同一侧：报告里的 freed_bytes 必须和用户拿 du 量出来的一致）。
fn dir_bytes(root: &Path) -> u64 {
    let mut bytes = 0u64;
    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: Vec::new(),
    };
    let _ = walk_files(root, &opts, |_, m| {
        if m.is_file() {
            bytes += m.len();
        }
    });
    bytes
}

/// 树哈希与体积统计时剪掉的 install 子路径名。
///
/// 与清单里 skill 资源的 `install_paths` 默认值保持一致——三处口径
/// （status 体积 / 归档排除 / `skill list` 检测的树哈希）必须是同一份定义。
pub const DEFAULT_INSTALL_DIRS: [&str; 4] = ["node_modules", "dist", "bin", ".git"];

/// [`DEFAULT_INSTALL_DIRS`] 的 `Vec<String>` 形态（walk / hash 的入参口径）。
fn install_prune() -> Vec<String> {
    DEFAULT_INSTALL_DIRS.iter().map(|s| s.to_string()).collect()
}

/// 与 [`crate::status::status`] 同一套只读打开：同一默认路径、同一条
/// [`crate::freshness::ensure_exists`]（库还没建过就先建一次）。
fn open_index(index_path: Option<&Path>) -> Result<Index> {
    let path = freshness::ensure_exists(index_path)?;
    Index::open_readonly(&path)
        .with_context(|| format!("failed to open index read-only: {}", path.display()))
}

/// 显式 home 优先；缺省取真实 home，取不到就报错而不是拿 `~` 当目录用。
fn resolve_home(home: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = home {
        return Ok(h.to_path_buf());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("cannot determine the home directory; pass an explicit home");
    }
    Ok(h)
}

/// 把清单里的 `~` 路径按**给定 home**展开（不碰进程真实 HOME，测试才能隔离）。
fn expand_tilde_at(home: &Path, raw: &str) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

/// 目标 agent 的 skill 根目录：取它清单里 `kind = "skill"` 那条资源的路径。
///
/// 不从索引反推父目录：目标 agent 可能一个 skill 都没装，索引里根本没有行，
/// 而清单永远知道该往哪儿放。
fn agent_skill_root(home: &Path, agent: &str) -> Result<PathBuf> {
    let user_dir = home.join(".agent-duster").join("adapters");
    let manifests = manifest::load_all(Some(&user_dir))?;
    let Some(m) = manifests.iter().find(|m| m.agent.id == agent) else {
        bail!(
            "unknown agent `{agent}`; known agents: {}",
            join_or_none(manifests.iter().map(|m| m.agent.id.as_str()))
        );
    };
    let Some(r) = m.resources.iter().find(|r| r.kind == ResourceKind::Skill) else {
        bail!("agent `{agent}` declares no skill directory; cannot link a skill into it");
    };
    Ok(expand_tilde_at(home, &r.path))
}

/// 一组副本涉及的 agent，按出现顺序去重。
///
/// 错误文案里必须报「几**家**」而不是「几**份**」：同一家装了两份时,
/// 拿副本数当家数会印出 `installed for 2 agents (claude-code, claude-code)`
/// ——数字和名单自相矛盾,用户会当成 duster 算错了。
fn distinct_agents(rows: &[duster_index::query::ResourceRecord]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for r in rows {
        if !out.contains(&r.agent_id.as_str()) {
            out.push(&r.agent_id);
        }
    }
    out
}

/// 逗号分隔；空集给一句人话而不是空字符串。
fn join_or_none<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<&str> = items.collect();
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

/// 树内一条软链：相对路径 + `read_link` 原文（不 resolve）。
struct SymlinkEntry {
    /// 相对 root 的路径（`/` 分隔，与 hash_tree 同一编码）。
    rel: String,
    /// `read_link` 原文（目标是否存在与 read_link 无关）；读不到（竞态 /
    /// 权限）为 `None`。
    target_raw: Option<PathBuf>,
}

/// 一份副本的现场测量结果。
struct CopyMeasure {
    tree_hash: String,
    /// 剪枝后的用户内容体积。
    bytes: u64,
    /// 剪掉那部分的体积。
    install_bytes: u64,
    /// 递归遍历到的常规文件数（`gstack/` 那种嵌了真文件的子目录也算）。
    /// 为 0 且 [`Self::symlinks`] 非空时，这份 copy 是「只含软链」的转发
    /// 副本，整份判 Linked/Broken、不参与 identical/drifted 比较。
    regular_count: u64,
    /// 树内软链条目（相对路径 + 原文）。夹着常规文件时是噪音，不改变状态，
    /// 解析成败留给 [`list`] 在真正需要时再查。
    symlinks: Vec<SymlinkEntry>,
}

/// 现场测量一份副本：树哈希 + 体积拆分 + 软链条目。调用方保证 `root` 不是
/// 软链（根软链副本在 [`list`] 里单独处理，不走进这里——它们没有自己的内容
/// 可量）。三个事实在同一次 [`walk_files`] 里收齐，不为软链判定再走第二遍。
///
/// **不 follow 软链读目标内容**：这是磁盘工具，软链本身几乎不占盘；跟随会
/// 把 gstack 的字节在 benchmark 和 gstack 两处重复计数。所以「只含软链」
/// 的副本在 [`list`] 里走状态规则 2（Linked/Broken），根本不进哈希比较——
/// 它们的内容住在别处，拿空文件表哈希判 identical 是最严重的谎。
fn measure_copy(root: &Path, prune: &[String]) -> Result<CopyMeasure> {
    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: prune.to_vec(),
    };
    // `pruned_bytes` 就是剪枝子树那部分，直接读它，不要拿
    // `total_bytes - 逐文件累加` 去凑：逐文件遍历只认常规文件，
    // 而 `total_bytes` 把符号链接按自身长度算进去了。本机 `connect-chrome`
    // 这类「整个 skill 只有一条指向 gstack 的 SKILL.md 软链」的目录，
    // 相减会把那 58 字节的链接算成"软件本体"。
    let stats = walk_stats(root, &opts)
        .with_context(|| format!("failed to stat skill directory: {}", root.display()))?;

    let mut bytes = 0u64;
    let mut regular_count = 0u64;
    let mut symlinks: Vec<SymlinkEntry> = Vec::new();
    walk_files(root, &opts, |p, meta| {
        if meta.is_file() {
            regular_count += 1;
            bytes += meta.len();
            return;
        }
        if meta.file_type().is_symlink() {
            // 只记链接本身（相对路径 + read_link 原文），不跟随、不读目标
            // 内容（见函数注释：重复计数）；解析成败由 list 按需再查。
            let rel = p
                .strip_prefix(root)
                .expect("walk 产出的路径必在 root 之下")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            symlinks.push(SymlinkEntry {
                rel,
                target_raw: std::fs::read_link(p).ok(),
            });
        }
    })
    .with_context(|| format!("failed to walk skill directory: {}", root.display()))?;

    // 没有常规文件的目录（只含软链，或全空）不哈希：文件表为空时哈希是
    // 个与内容无关的常量，正是假 identical 的病根，这类 copy 的树哈希
    // 一律置空（[`list`] 里也只对常规副本用 tree_hash 比较）。
    let tree_hash = if regular_count == 0 {
        String::new()
    } else {
        hash_tree(root, prune)
            .with_context(|| format!("failed to hash skill directory: {}", root.display()))?
            .to_hex()
            .to_string()
    };

    Ok(CopyMeasure {
        tree_hash,
        bytes,
        install_bytes: stats.pruned_bytes,
        regular_count,
        symlinks,
    })
}

/// DRIFTED 的差异摘要：对每一对树哈希不同的副本，列出只在一侧的路径与
/// 两侧都有但内容不同的路径。
///
/// 比对本身走 [`crate::diff::diff_trees`]——这里只负责把 `Diff` 渲染成
/// [`SkillGroup::diff`] 那句概览。**不在这儿再写一遍逐文件哈希**：
/// 第二份实现迟早和 `duster diff` 的口径走散。行级明细在这条路径上
/// 显式关掉（`line_level: false`）：一组 skill 可能有几十个副本对，
/// 概览要的是「哪些文件」，不是几千行 `-`/`+`。
fn build_diff(
    copies: &[SkillCopy],
    prune: &[String],
    warnings: &mut Vec<String>,
) -> Result<String> {
    // 软链副本没有可比内容，先滤掉：不滤的话它们的空 tree_hash 会和任何
    // 普通副本不同，被拖进 diff_trees 对一条软链路径做树遍历，产出整屏
    // "only in …" 垃圾。
    let copies: Vec<&SkillCopy> = copies
        .iter()
        .filter(|c| !matches!(c.state, DupState::Linked | DupState::Broken))
        .collect();
    let opts = DiffOptions {
        line_level: false,
        prune_dirs: prune.to_vec(),
        ..Default::default()
    };

    let mut out = String::new();
    for i in 0..copies.len() {
        for j in (i + 1)..copies.len() {
            if copies[i].tree_hash == copies[j].tree_hash {
                continue;
            }
            let (a, b) = (&copies[i], &copies[j]);
            let d = diff_trees(&a.path, &b.path, &a.agent_id, &b.agent_id, &opts)?;
            warnings.extend(d.warnings);

            let pick = |want: Change| -> Vec<&str> {
                d.entries
                    .iter()
                    .filter(|e| e.change == want)
                    .map(|e| e.key.as_str())
                    .collect()
            };
            let only_a = pick(Change::OnlyLeft);
            let only_b = pick(Change::OnlyRight);
            let changed = pick(Change::Changed);

            out.push_str(&format!("{} vs {}:\n", a.agent_id, b.agent_id));
            if !only_a.is_empty() {
                out.push_str(&format!(
                    "  only in {}: {}\n",
                    a.agent_id,
                    only_a.join(", ")
                ));
            }
            if !only_b.is_empty() {
                out.push_str(&format!(
                    "  only in {}: {}\n",
                    b.agent_id,
                    only_b.join(", ")
                ));
            }
            if !changed.is_empty() {
                out.push_str(&format!("  differs: {}\n", changed.join(", ")));
            }
            if d.identical {
                // 树哈希不同却逐条目全同：文件在两次遍历之间变了，
                // 或差异落在 hash_tree 认、而这里的遍历不认的地方。
                out.push_str("  differs in entries outside the file walk, or changed mid-scan\n");
            }
        }
    }
    Ok(out)
}

/// 源副本入 CAS：`cas/<hex>/`。已存在且内容对得上就复用，绝不重复落盘。
fn ingest_cas(
    src: &Path,
    cas_dir: &Path,
    prune: &[String],
    hex: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if cas_dir.is_dir() {
        let actual = hash_tree(cas_dir, prune)
            .with_context(|| format!("failed to hash CAS entry: {}", cas_dir.display()))?
            .to_hex()
            .to_string();
        if actual == hex {
            return Ok(()); // 同内容同实体，引用计数交给文件系统。
        }
        // 不自动重建：已有硬链正指着里头的 inode，悄悄换掉等于让别的 agent
        // 的 skill 内容凭空变。让用户看见并自己决定。
        bail!(
            "CAS entry {} is corrupted: content hashes to {actual}, expected {hex}. \
             Remove that directory and retry.",
            cas_dir.display()
        );
    }

    let parent = cas_dir
        .parent()
        .with_context(|| format!("CAS path has no parent: {}", cas_dir.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create CAS root: {}", parent.display()))?;

    // 先落临时目录再整体 rename：中途失败不会在 CAS 里留下半份实体，
    // 而半份实体会被后续调用当成"已存在"复用。
    let tmp = parent.join(format!(".tmp-{hex}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = copy_tree(src, &tmp, prune) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e).with_context(|| format!("failed to ingest {} into CAS", src.display()));
    }
    match std::fs::rename(&tmp, cas_dir) {
        Ok(()) => Ok(()),
        Err(_) if cas_dir.is_dir() => {
            let _ = std::fs::remove_dir_all(&tmp);
            warnings.push(format!(
                "CAS entry {} appeared concurrently; reusing it",
                cas_dir.display()
            ));
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e).with_context(|| format!("failed to publish CAS entry: {}", cas_dir.display()))
        }
    }
}

/// 递归复制目录树，跳过 `prune` 里的目录名，保留权限位。
///
/// 符号链接原样重建（不跟随）：跟随会把 install 目录从后门带进来，
/// 也会在自指链接上转圈。
fn copy_tree(src: &Path, dst: &Path, prune: &[String]) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory: {}", dst.display()))?;
    copy_dir_mode(src, dst)?;

    for entry in std::fs::read_dir(src)
        .with_context(|| format!("failed to read directory: {}", src.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let from = entry.path();
        let to = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("failed to recreate symlink: {}", to.display()))?;
        } else if ft.is_dir() {
            if prune.iter().any(|p| OsStr::new(p) == name) {
                continue;
            }
            copy_tree(&from, &to, prune)?;
        } else {
            // fs::copy 自带权限位复制。
            std::fs::copy(&from, &to).with_context(|| {
                format!("failed to copy {} -> {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

/// 把 `src` 目录的权限位抄到 `dst`。
fn copy_dir_mode(src: &Path, dst: &Path) -> Result<()> {
    let perm = std::fs::metadata(src)
        .with_context(|| format!("failed to stat: {}", src.display()))?
        .permissions();
    std::fs::set_permissions(dst, perm)
        .with_context(|| format!("failed to set permissions: {}", dst.display()))?;
    Ok(())
}

/// 逐文件硬链：目录照建，文件硬链，符号链接原样重建。
///
/// 跨设备时 `hard_link` 报 `EXDEV`，由调用方降级。
fn hardlink_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory: {}", dst.display()))?;
    copy_dir_mode(src, dst)?;

    for entry in std::fs::read_dir(src)
        .with_context(|| format!("failed to read directory: {}", src.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("failed to recreate symlink: {}", to.display()))?;
        } else if ft.is_dir() {
            hardlink_tree(&from, &to)?;
        } else {
            std::fs::hard_link(&from, &to).with_context(|| {
                format!("failed to hard link {} -> {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

/// 整个 skill 目录做成一条软链。
fn symlink_dir(src: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src, dst)
        .with_context(|| format!("failed to symlink {} -> {}", src.display(), dst.display()))
}

/// 实测目标 agent 能否加载：SKILL.md 可读、frontmatter 可解析、名字对得上。
fn verify_loadable(target: &Path, name: &str) -> Result<()> {
    let meta = duster_adapter::mapper::skill::parse_skill_md(target)
        .with_context(|| format!("linked skill is not loadable at {}", target.display()))?;
    if meta.name != name {
        bail!(
            "linked skill at {} reports name `{}`, expected `{name}`",
            target.display(),
            meta.name
        );
    }
    Ok(())
}

/// 回滚：把这次建出来的东西（软链或目录）删干净。失败不再上抛——
/// 此时已经在错误路径上，能清多少清多少。
fn remove_link(target: &Path) {
    match std::fs::symlink_metadata(target) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(target);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(target);
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    use duster_index::upsert::{self, ResourceRow};

    /// 造一个 skill 目录：SKILL.md + 一个正文文件。
    fn make_skill(root: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture\n---\n\n{body}\n"),
        )
        .unwrap();
        std::fs::write(root.join("run.sh"), "echo hi\n").unwrap();
    }

    /// 往索引里写一行 skill 资源。
    fn seed_skill_row(db: &Path, agent: &str, name: &str, root: &Path) {
        let idx = Index::open(db).unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: agent.to_string(),
                kind: "skill".into(),
                scope: "global".into(),
                key: name.to_string(),
                path: root.display().to_string(),
                size: 0,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
    }

    /// claude-code / codex 两个 agent 各一份 `foo`，正文分别由入参决定。
    /// 返回 (home, 库路径, claude 侧 root, codex 侧 root)。
    fn two_copies(home: &Path, claude_body: &str, codex_body: &str) -> (PathBuf, PathBuf, PathBuf) {
        let db = home.join(".agent-duster").join("index.db");
        let a = home.join(".claude").join("skills").join("foo");
        let b = home.join(".codex").join("skills").join("foo");
        make_skill(&a, "foo", claude_body);
        make_skill(&b, "foo", codex_body);
        seed_skill_row(&db, "claude-code", "foo", &a);
        seed_skill_row(&db, "codex", "foo", &b);
        (db, a, b)
    }

    #[test]
    fn 两份字节相同的副本判定为_identical() {
        let home = tempfile::tempdir().unwrap();
        let (db, _, _) = two_copies(home.path(), "same body", "same body");

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        assert_eq!(groups[0].name, "foo");
        assert_eq!(groups[0].state, DupState::Identical);
        assert!(groups[0].diff.is_none());
        assert_eq!(groups[0].copies.len(), 2);
        assert_eq!(groups[0].copies[0].tree_hash, groups[0].copies[1].tree_hash);
    }

    /// 剪枝是这条检测的命根子：两处 `npm install` 的产物必然不同，
    /// 不剪掉的话每一组同名 skill 都会被判成 DRIFTED，检测直接失效。
    #[test]
    fn 只有_node_modules_不同仍判定为_identical() {
        let home = tempfile::tempdir().unwrap();
        let (db, a, b) = two_copies(home.path(), "same body", "same body");

        std::fs::create_dir_all(a.join("node_modules").join("left")).unwrap();
        std::fs::write(a.join("node_modules").join("left").join("x.js"), "aaaa").unwrap();
        std::fs::create_dir_all(b.join("node_modules").join("right")).unwrap();
        std::fs::write(
            b.join("node_modules").join("right").join("y.js"),
            "bbbbbbbbbbbb",
        )
        .unwrap();

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, DupState::Identical, "{:#?}", groups[0]);
        // 剪掉的那部分要单独报出来，且两侧确实各有内容（证明真走到了）。
        for c in &groups[0].copies {
            assert!(c.install_bytes > 0, "install 体积应单独统计: {c:#?}");
            assert!(c.bytes > 0);
        }
        assert_ne!(
            groups[0].copies[0].install_bytes, groups[0].copies[1].install_bytes,
            "两侧 node_modules 本就不同"
        );
    }

    #[test]
    fn skill_md_不同判定为_drifted_并指名文件() {
        let home = tempfile::tempdir().unwrap();
        let (db, _, _) = two_copies(home.path(), "left body", "right body");

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, DupState::Drifted);
        let diff = groups[0].diff.as_deref().unwrap();
        assert!(diff.contains("SKILL.md"), "{diff}");
        assert!(
            diff.contains("claude-code") && diff.contains("codex"),
            "{diff}"
        );
    }

    /// 只有一份的 skill 也要列出来：本机实测 34 个名字里 21 个只有一份，滤掉
    /// 它们，`skill list` 剩下 13 行，名不副实。状态是 `Single` 而不是
    /// `Identical`——没有可比对象，也就永远没有 diff。
    #[test]
    fn 只有一份的_skill_照样列出且判定为_single() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("solo");
        make_skill(&a, "solo", "body");
        seed_skill_row(&db, "claude-code", "solo", &a);

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        assert_eq!(groups[0].name, "solo");
        assert_eq!(groups[0].state, DupState::Single);
        assert!(groups[0].diff.is_none(), "没有可比对象就不该有 diff");
        assert_eq!(groups[0].copies.len(), 1);
        // 体积照量、哈希照填：单份组走的是同一条 measure_copy，没有快路径。
        assert!(groups[0].copies[0].bytes > 0);
        assert!(!groups[0].copies[0].tree_hash.is_empty());
    }

    /// 单份组与多份组同表返回，且**按名字混排**——单份的不排到末尾另成一区。
    /// 用户是来找名字的，按名字排才扫得动；「哪几个重复」由状态列回答。
    #[test]
    fn 单份组与多份组按名字混排() {
        let home = tempfile::tempdir().unwrap();
        // foo 两份；aaa 一份，名字排在 foo 前面。
        let (db, _, _) = two_copies(home.path(), "same body", "same body");
        let solo = home.path().join(".claude").join("skills").join("aaa");
        make_skill(&solo, "aaa", "body");
        seed_skill_row(&db, "claude-code", "aaa", &solo);

        let groups = list(Some(&db)).unwrap();
        let seen: Vec<(&str, DupState)> =
            groups.iter().map(|g| (g.name.as_str(), g.state)).collect();
        assert_eq!(
            seen,
            vec![("aaa", DupState::Single), ("foo", DupState::Identical)],
            "{groups:#?}"
        );
    }

    #[test]
    fn 副本目录消失只降级为警告不中断() {
        let home = tempfile::tempdir().unwrap();
        let (db, _, b) = two_copies(home.path(), "same body", "same body");
        std::fs::remove_dir_all(&b).unwrap();

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies.len(), 1);
        // 索引说两份、磁盘只剩一份：状态按现场算，否则就是拿这一份和自己比。
        assert_eq!(groups[0].state, DupState::Single);
        assert_eq!(groups[0].warnings.len(), 1, "{:#?}", groups[0]);
        assert!(groups[0].warnings[0].contains("codex"));
    }

    /// 软链副本的判定：根路径是软链时不 follow、不量体积、不参与哈希比较。
    /// resolve 得了 → linked（记下指向哪），悬空 → broken。0 字节软链
    /// 绝不能因为树哈希相同就并进 identical——本机 bark-notify 四条悬空链
    /// 与 benchmark/browse/careful 六条有效链正是这个 bug 的现场。
    #[test]
    fn 软链判linked悬空判broken普通副本照旧() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");

        // 有效目标：别的 skill 的实体目录（gstack 场景）。
        let real_target = home.path().join("gstack-real");
        make_skill(&real_target, "gstack", "gstack body");

        // 同名组 foo：两份普通副本（同内容）+ 一条有效软链 + 一条悬空软链。
        let a = home.path().join(".claude").join("skills").join("foo");
        make_skill(&a, "foo", "foo body");
        let b = home.path().join(".codex").join("skills").join("foo");
        make_skill(&b, "foo", "foo body");

        let linked = home.path().join(".omp").join("skills").join("foo");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real_target, &linked).unwrap();

        let dangling = home.path().join(".qoder").join("skills").join("foo");
        std::fs::create_dir_all(dangling.parent().unwrap()).unwrap();
        let deleted = home.path().join("deleted-target");
        std::os::unix::fs::symlink(&deleted, &dangling).unwrap();

        seed_skill_row(&db, "claude-code", "foo", &a);
        seed_skill_row(&db, "codex", "foo", &b);
        seed_skill_row(&db, "omp", "foo", &linked);
        seed_skill_row(&db, "qoder", "foo", &dangling);

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        let g = &groups[0];

        // 三态各归各：普通副本照旧 identical，软链各按能否解析归位，
        // 0 字节软链绝不判 identical。
        let by_agent = |agent: &str| g.copies.iter().find(|c| c.agent_id == agent).unwrap();
        assert_eq!(by_agent("claude-code").state, DupState::Identical, "{g:#?}");
        assert_eq!(by_agent("codex").state, DupState::Identical, "{g:#?}");
        assert_eq!(by_agent("omp").state, DupState::Linked, "{g:#?}");
        assert_eq!(by_agent("qoder").state, DupState::Broken, "{g:#?}");

        // linked 记下指向哪（canonicalize 后的绝对路径）。
        assert_eq!(
            by_agent("omp").link_target.as_deref(),
            Some(real_target.canonicalize().unwrap().as_path()),
            "linked 要带目标: {g:#?}"
        );
        // broken 记 read_link 原文——目标已消失，这就是最后一份记录。
        assert_eq!(
            by_agent("qoder").link_target.as_deref(),
            Some(deleted.as_path()),
            "broken 要带链接原文: {g:#?}"
        );
        // 普通副本没有 link_target。
        assert_eq!(by_agent("claude-code").link_target, None);

        // 软链副本不量体积、不填哈希：它们没有自己的内容。
        assert_eq!(by_agent("omp").bytes, 0);
        assert!(by_agent("omp").tree_hash.is_empty());
        assert_eq!(by_agent("qoder").bytes, 0);

        // 悬空链压过组级状态：先修坏账再谈比较，也就没有 diff。
        assert_eq!(g.state, DupState::Broken, "{g:#?}");
        assert!(g.diff.is_none(), "组级是 broken，不该有 diff: {:#?}", g.diff);

        // 序列化名小写（--json 的契约）。
        assert_eq!(serde_json::to_value(DupState::Linked).unwrap(), "linked");
        assert_eq!(serde_json::to_value(DupState::Broken).unwrap(), "broken");
    }

    /// 全软链组（benchmark/browse/careful 这类指向 gstack 的）判 linked 而
    /// 不是 identical：旧逻辑不 follow 读出 0 字节、树哈希全相同，整组被
    /// 误判成 identical，链接建议全部放到了本来就不存在的"实体"上。
    #[test]
    fn 全软链组判linked而不是identical() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");

        let target = home.path().join("gstack-real");
        make_skill(&target, "gstack", "body");

        let a = home.path().join(".claude").join("skills").join("benchmark");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &a).unwrap();
        let b = home.path().join(".codex").join("skills").join("benchmark");
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &b).unwrap();

        seed_skill_row(&db, "claude-code", "benchmark", &a);
        seed_skill_row(&db, "codex", "benchmark", &b);

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        let g = &groups[0];
        assert_eq!(g.state, DupState::Linked, "{g:#?}");
        for c in &g.copies {
            assert_eq!(c.state, DupState::Linked, "{c:#?}");
            assert!(c.link_target.is_some(), "linked 必带目标: {c:#?}");
        }
        assert!(g.diff.is_none());
    }

    /// 嵌套软链（SKILL.md 是目录**里**的软链，而不是目录本身）：只含软链的
    /// 目录，内容住在别处，整份判 Linked/Broken，绝不进哈希比较。
    /// connect-chrome / open-gstack-browser 就是现场——两个目录各只有一条
    /// 指向**不同**目标的 SKILL.md 软链，旧逻辑读出空文件表哈希、互相判成
    /// identical。
    #[test]
    fn 只含软链的目录按目标能否解析判linked_broken() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");

        // 两个不同的有效目标（gstack 场景：各 skill 软链指向自己的实体）。
        let target_a = home.path().join("gstack-a");
        make_skill(&target_a, "gstack", "gstack a body");
        let target_b = home.path().join("gstack-b");
        make_skill(&target_b, "gstack", "gstack b body");
        std::fs::write(target_b.join("notes.md"), "extra").unwrap();

        // 副本目录里只有软链，没有常规文件。
        let a = home.path().join(".claude").join("skills").join("browser");
        std::fs::create_dir_all(&a).unwrap();
        std::os::unix::fs::symlink(target_a.join("SKILL.md"), a.join("SKILL.md")).unwrap();

        // 两条软链指向不同目标；`0-first.md` 按相对路径排第一，link_target
        // 必须记它——证明「取第一条」真是按排序取，不是碰巧拿 SKILL.md。
        let b = home.path().join(".codex").join("skills").join("browser");
        std::fs::create_dir_all(&b).unwrap();
        std::os::unix::fs::symlink(target_b.join("notes.md"), b.join("0-first.md")).unwrap();
        std::os::unix::fs::symlink(target_b.join("SKILL.md"), b.join("SKILL.md")).unwrap();

        seed_skill_row(&db, "claude-code", "browser", &a);
        seed_skill_row(&db, "codex", "browser", &b);

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        let g = &groups[0];

        // 两份都 Linked（目标各自可解析），组绝不是 identical——指向不同目标
        // 的两份说"完全相同"是最严重的谎。
        let by_agent = |agent: &str| g.copies.iter().find(|c| c.agent_id == agent).unwrap();
        assert_eq!(by_agent("claude-code").state, DupState::Linked, "{g:#?}");
        assert_eq!(by_agent("codex").state, DupState::Linked, "{g:#?}");
        assert_ne!(g.state, DupState::Identical, "{g:#?}");
        assert_eq!(g.state, DupState::Linked, "{g:#?}");
        assert!(g.diff.is_none());

        // 不量内容、不填哈希：0 字节的软链类 copy 绝不判 identical。
        assert_eq!(by_agent("claude-code").bytes, 0);
        assert!(by_agent("claude-code").tree_hash.is_empty());
        assert_eq!(by_agent("codex").bytes, 0);
        assert!(by_agent("codex").tree_hash.is_empty());

        // link_target：Linked 记第一条（按相对路径排序）解析后的目标。
        assert_eq!(
            by_agent("claude-code").link_target.as_deref(),
            Some(target_a.join("SKILL.md").canonicalize().unwrap().as_path()),
            "{g:#?}"
        );
        assert_eq!(
            by_agent("codex").link_target.as_deref(),
            Some(target_b.join("notes.md").canonicalize().unwrap().as_path()),
            "取的是排序后第一条: {g:#?}"
        );
    }

    /// 只含软链的目录里**任一**悬空 → 整份 Broken（不只是"唯一那条"悬空才报）。
    #[test]
    fn 只含软链的目录有悬空链即broken() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");

        let target = home.path().join("real");
        make_skill(&target, "real", "body");

        let dir = home.path().join(".claude").join("skills").join("gone");
        std::fs::create_dir_all(&dir).unwrap();
        let deleted = home.path().join("deleted-target").join("SKILL.md");
        // 排序第一的是悬空链 → 状态 Broken，link_target 记它的 read_link 原文。
        std::os::unix::fs::symlink(&deleted, dir.join("0-dangling.md")).unwrap();
        std::os::unix::fs::symlink(target.join("SKILL.md"), dir.join("SKILL.md")).unwrap();

        seed_skill_row(&db, "claude-code", "gone", &dir);

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        let g = &groups[0];
        assert_eq!(g.state, DupState::Broken, "{g:#?}");
        assert_eq!(g.copies[0].state, DupState::Broken, "{g:#?}");
        assert!(g.copies[0].tree_hash.is_empty());
        assert_eq!(
            g.copies[0].link_target.as_deref(),
            Some(deleted.as_path()),
            "broken 记 read_link 原文: {g:#?}"
        );
        assert!(g.diff.is_none(), "broken 组不该有 diff: {g:#?}");
    }

    /// 夹着常规文件的零星软链是噪音：不改变 Identical/Drifted 判定。
    #[test]
    fn 常规文件里的零星软链不改状态() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");

        // 每份常规副本里塞一条悬空软链——resolve 都不该被问起，状态照旧。
        let mk = |agent: &str, name: &str, body: &str| {
            let dir = home.path().join(agent).join("skills").join(name);
            make_skill(&dir, name, body);
            // 真目录里嵌真文件（gstack 形态）也要正常计为"有常规文件"。
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub").join("inner.txt"), "x").unwrap();
            std::os::unix::fs::symlink(
                home.path().join("elsewhere").join("x.txt"),
                dir.join("stray-link"),
            )
            .unwrap();
            seed_skill_row(&db, agent, name, &dir);
        };
        mk(".claude", "foo", "same body");
        mk(".codex", "foo", "same body");

        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        let g = &groups[0];
        assert_eq!(g.state, DupState::Identical, "{g:#?}");
        assert_eq!(g.copies.len(), 2);
        for c in &g.copies {
            assert_eq!(c.state, DupState::Identical, "{c:#?}");
            // 常规文件照常量体积、照常哈希；软链字节不计入（不跟随）。
            assert!(c.bytes > 0, "常规文件字节要照量: {c:#?}");
            assert!(!c.tree_hash.is_empty(), "{c:#?}");
        }

        // 内容不同时照旧 drifted，软链同样不改状态。
        mk(".omp", "bar", "left");
        mk(".qoder", "bar", "right");
        let groups = list(Some(&db)).unwrap();
        let bar = groups.iter().find(|g| g.name == "bar").unwrap();
        assert_eq!(bar.state, DupState::Drifted, "{bar:#?}");
        assert!(bar.diff.is_some());
        for c in &bar.copies {
            assert_eq!(c.state, DupState::Drifted, "{c:#?}");
        }
    }

    #[test]
    fn link_建出硬链目标并实测可加载() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("foo");
        make_skill(&a, "foo", "body");
        seed_skill_row(&db, "claude-code", "foo", &a);

        let rep = link(
            Some(&db),
            Some(home.path()),
            "foo",
            "claude-code",
            "codex",
            false,
        )
        .unwrap();

        assert_eq!(rep.mode, LinkMode::Hardlink, "{rep:#?}");
        assert!(rep.downgrade_reason.is_none());
        assert!(rep.verified);

        let target = home.path().join(".codex").join("skills").join("foo");
        assert_eq!(rep.to, target);
        assert!(target.join("SKILL.md").is_file());

        // CAS 里只有一份实体。
        let entries: Vec<_> = std::fs::read_dir(cas_root(home.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");

        // 硬链的证据：目标与 CAS 实体同 inode。
        let cas_ino = std::fs::metadata(rep.cas_path.join("SKILL.md"))
            .unwrap()
            .ino();
        let tgt_ino = std::fs::metadata(target.join("SKILL.md")).unwrap().ino();
        assert_eq!(cas_ino, tgt_ino, "目标应与 CAS 共享 inode");
    }

    #[test]
    fn link_不覆盖已存在的目标() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("foo");
        make_skill(&a, "foo", "source body");
        seed_skill_row(&db, "claude-code", "foo", &a);

        let target = home.path().join(".codex").join("skills").join("foo");
        make_skill(&target, "foo", "PRECIOUS LOCAL EDIT");
        let before = std::fs::read(target.join("SKILL.md")).unwrap();

        let err = link(
            Some(&db),
            Some(home.path()),
            "foo",
            "claude-code",
            "codex",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        assert_eq!(
            std::fs::read(target.join("SKILL.md")).unwrap(),
            before,
            "已存在的目标必须一个字节都不变"
        );
    }

    #[test]
    fn link_干跑不落任何盘() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("foo");
        make_skill(&a, "foo", "body");
        seed_skill_row(&db, "claude-code", "foo", &a);

        let rep = link(
            Some(&db),
            Some(home.path()),
            "foo",
            "claude-code",
            "codex",
            true,
        )
        .unwrap();

        assert!(!rep.verified);
        assert!(!rep.warnings.is_empty());
        assert!(!rep.to.exists(), "干跑不该建目标");
        assert!(!cas_root(home.path()).exists(), "干跑不该建 CAS");
        assert!(!home.path().join(".codex").exists());
    }

    #[test]
    fn link_未知_skill_列出已知项() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("foo");
        make_skill(&a, "foo", "body");
        seed_skill_row(&db, "claude-code", "foo", &a);

        let err = link(
            Some(&db),
            Some(home.path()),
            "nope",
            "claude-code",
            "codex",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("foo"), "{err:#}");
    }

    /// 降级链的后两级不能是死代码：软链和整份复制各自建出来的目标，
    /// 都必须通过同一套"实测可加载"验收——否则降级只是把坏 skill
    /// 换个姿势留在用户目录里。
    #[test]
    fn 降级用的软链与复制同样通得过实测() {
        let home = tempfile::tempdir().unwrap();
        let cas = home.path().join("cas-entry");
        make_skill(&cas, "foo", "body");

        let via_symlink = home.path().join("via-symlink");
        symlink_dir(&cas, &via_symlink).unwrap();
        verify_loadable(&via_symlink, "foo").unwrap();

        let via_copy = home.path().join("via-copy");
        copy_tree(&cas, &via_copy, &[]).unwrap();
        verify_loadable(&via_copy, "foo").unwrap();
        // 复制是真复制：内容一致但不共享 inode。
        assert_ne!(
            std::fs::metadata(cas.join("SKILL.md")).unwrap().ino(),
            std::fs::metadata(via_copy.join("SKILL.md")).unwrap().ino()
        );

        // 名字对不上要判失败，回滚才有触发条件。
        assert!(verify_loadable(&via_copy, "bar").is_err());
        // SKILL.md 不见了同样判失败。
        std::fs::remove_file(via_copy.join("SKILL.md")).unwrap();
        assert!(verify_loadable(&via_copy, "foo").is_err());
    }

    /// CAS 入库幂等：同内容第二次入库复用同一实体，不新增目录。
    #[test]
    fn cas_同内容只落一份实体() {
        let home = tempfile::tempdir().unwrap();
        let src = home.path().join("src");
        make_skill(&src, "foo", "body");
        std::fs::create_dir_all(src.join("node_modules")).unwrap();
        std::fs::write(src.join("node_modules").join("junk"), "x").unwrap();

        let prune = install_prune();
        let hex = hash_tree(&src, &prune).unwrap().to_hex().to_string();
        let cas_dir = cas_root(home.path()).join(&hex);

        let mut warnings = Vec::new();
        ingest_cas(&src, &cas_dir, &prune, &hex, &mut warnings).unwrap();
        ingest_cas(&src, &cas_dir, &prune, &hex, &mut warnings).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");

        assert_eq!(std::fs::read_dir(cas_root(home.path())).unwrap().count(), 1);
        // install 子路径不进 CAS：链接过去的是内容，不是别人的构建产物。
        assert!(cas_dir.join("SKILL.md").is_file());
        assert!(!cas_dir.join("node_modules").exists());

        // 实体被外力改坏就明说，绝不悄悄重建（已有硬链正指着里头的 inode）。
        std::fs::write(cas_dir.join("SKILL.md"), "tampered").unwrap();
        let err = ingest_cas(&src, &cas_dir, &prune, &hex, &mut warnings).unwrap_err();
        assert!(err.to_string().contains("corrupted"), "{err:#}");
    }

    // -----------------------------------------------------------------------
    // rm
    // -----------------------------------------------------------------------

    /// skill 删除不归档（见 [`remove`] 的文档）：helper 不再收 archive 参数，
    /// 字段填 false 只是占住共享结构体里属于 session/memory 的那一位。
    fn rm_opts(db: &Path, home: &Path, dry_run: bool) -> DeleteOptions {
        DeleteOptions {
            index_path: Some(db.to_path_buf()),
            home: Some(home.to_path_buf()),
            archive: false,
            dry_run,
        }
    }

    /// 悬空软链：只 unlink 链接本身（归一个断链是假安全感——它连内容都
    /// 没有），索引行照清。skill 删除不归档，`archived` 恒为 None。
    #[test]
    fn rm_悬空软链只unlink_且不归档() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let dangling = home.path().join(".qoder").join("skills").join("gone");
        std::fs::create_dir_all(dangling.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.path().join("deleted-target"), &dangling).unwrap();
        seed_skill_row(&db, "qoder", "gone", &dangling);

        let report = remove(&rm_opts(&db, home.path(), false), "gone", None, None).unwrap();
        assert!(
            std::fs::symlink_metadata(&dangling).is_err(),
            "悬空链接应被 unlink"
        );
        assert_eq!(report.removed, vec![dangling.clone()], "{:?}", report.removed);
        assert!(report.archived.is_none(), "skill 删除不归档,archived 恒为 None");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // 索引无幽灵行。
        assert!(list(Some(&db)).unwrap().is_empty(), "删完不该再列出来");
    }

    /// 有效软链：只 unlink 链接本身，**目标必须原封不动**——目标可能是
    /// 另一家 agent 正在用的那一份，也可能是 `skill link` 建的共享本体。
    #[test]
    fn rm_有效软链删后目标仍在() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let target = home.path().join("real-skill");
        make_skill(&target, "foo", "precious body");
        let link = home.path().join(".claude").join("skills").join("foo");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        seed_skill_row(&db, "claude-code", "foo", &link);

        let report = remove(&rm_opts(&db, home.path(), false), "foo", None, None).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "链接应被 unlink");
        assert!(report.archived.is_none(), "skill 删除不归档,archived 恒为 None");
        // 目标仍在、内容原封不动。
        assert!(target.is_dir(), "目标目录必须还在");
        assert!(
            std::fs::read_to_string(target.join("SKILL.md"))
                .unwrap()
                .contains("precious body"),
            "目标内容必须原封不动"
        );
        assert!(list(Some(&db)).unwrap().is_empty());
    }

    /// 真实目录：整棵真删，**不归档**——`skill rm` 是对着点名的副本下手
    /// （`--agent`/`--path`/y/N 三道指名），显式删除不需要暗中留副本；
    /// 批量按龄清理的 `duster prune` 才会给 skill 打包。删完 `<home>/
    /// agent-duster-exports` 必须不存在：一个字节的归档都不要有。
    #[test]
    fn rm_真实目录真删_导出目录不创建() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let dir = home.path().join(".claude").join("skills").join("foo");
        make_skill(&dir, "foo", "body");
        std::fs::write(dir.join("notes.md"), "extra notes").unwrap();
        seed_skill_row(&db, "claude-code", "foo", &dir);

        let report = remove(&rm_opts(&db, home.path(), false), "foo", None, None).unwrap();
        assert!(!dir.exists(), "目录应被整棵删除");
        assert!(report.removed.contains(&dir), "{:?}", report.removed);
        assert!(report.freed_bytes > 0);
        assert!(report.archived.is_none(), "真删不归档,archived 恒为 None");
        assert!(
            !home.path().join("agent-duster-exports").exists(),
            "真删之后导出目录根本不该存在"
        );
        assert!(list(Some(&db)).unwrap().is_empty());
    }

    /// 同名装在多家、没指名：报错列出候选，一份都不许删。
    #[test]
    fn rm_多家未指名报错列出候选_一份不删() {
        let home = tempfile::tempdir().unwrap();
        let (db, a, b) = two_copies(home.path(), "left body", "right body");

        let err = remove(&rm_opts(&db, home.path(), false), "foo", None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--agent"), "缺 --agent 必须明说: {msg}");
        assert!(msg.contains("claude-code") && msg.contains("codex"), "列出候选: {msg}");
        assert!(a.is_dir() && b.is_dir(), "报错不许删任何一份");
    }

    /// 指名 agent：只删那一家，另一家分毫不动。
    #[test]
    fn rm_指名agent只删那一家() {
        let home = tempfile::tempdir().unwrap();
        let (db, a, b) = two_copies(home.path(), "left body", "right body");

        let report =
            remove(&rm_opts(&db, home.path(), false), "foo", Some("claude-code"), None).unwrap();
        assert!(!a.exists(), "被点名的那份要删");
        assert!(b.is_dir(), "没点名的那份分毫不动");
        assert!(report.removed.contains(&a));
        // 索引里只剩 codex 那行。
        let groups = list(Some(&db)).unwrap();
        assert_eq!(groups[0].copies.len(), 1, "{groups:#?}");
        assert_eq!(groups[0].copies[0].agent_id, "codex");
    }

    /// 指名了没装过的 agent：报错列出候选。
    #[test]
    fn rm_指名未装的agent报错() {
        let home = tempfile::tempdir().unwrap();
        let (db, a, _) = two_copies(home.path(), "left body", "right body");
        let err = remove(&rm_opts(&db, home.path(), false), "foo", Some("qoder"), None).unwrap_err();
        assert!(err.to_string().contains("qoder"), "{err:#}");
        assert!(a.is_dir());
    }
    /// 同一 agent 名下同名多份（目录不同）：`--agent` 收窄之后仍剩多份，
    /// 必须再要 `path`，缺了报错列路径、一份不删。
    ///
    /// 这是真机形状：`open-gstack-browser` 在 claude-code 下就有
    /// `connect-chrome` 与 `open-gstack-browser` 两个目录，内容可以完全不同。
    /// 「指名一家」不等于「同意删掉那家的每一份」。
    #[test]
    fn rm_同一家多份必须再指名路径() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let one = home.path().join(".claude").join("skills").join("dir-a");
        let two = home.path().join(".claude").join("skills").join("dir-b");
        make_skill(&one, "foo", "body a");
        make_skill(&two, "foo", "body b");
        // 真机形状：同一 agent 同名两份靠 `name@目录名` 降级键共存
        // （见 `scan::scan_skills` 的去重），`skill_groups` 再按最后一个
        // `@` 切回同一组。直接用裸 name 播两行会撞 UNIQUE 被覆盖成一行。
        seed_skill_row(&db, "claude-code", "foo@dir-a", &one);
        seed_skill_row(&db, "claude-code", "foo@dir-b", &two);

        // 缺 path：报错，两份都还在。
        let err = remove(
            &rm_opts(&db, home.path(), false),
            "foo",
            Some("claude-code"),
            None,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--path"), "必须明说要 --path: {msg}");
        assert!(one.is_dir() && two.is_dir(), "一份都不许删");

        // 指名 path：只删那一份。
        let report = remove(
            &rm_opts(&db, home.path(), false),
            "foo",
            Some("claude-code"),
            Some(&one.display().to_string()),
        )
        .unwrap();
        assert!(!one.exists(), "被点名的那份要删");
        assert!(two.is_dir(), "没点名的那份分毫不动");
        assert_eq!(report.removed, vec![one]);
    }

    /// 未指名 agent 时的错误要报「几**家**」而不是「几**份**」：同一家两份
    /// 时拿副本数当家数会印出自相矛盾的 `2 agents (claude-code, claude-code)`。
    #[test]
    fn rm_未指名时按家数报错不重复列同一家() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let one = home.path().join(".claude").join("skills").join("dir-a");
        let two = home.path().join(".claude").join("skills").join("dir-b");
        make_skill(&one, "foo", "a");
        make_skill(&two, "foo", "b");
        seed_skill_row(&db, "claude-code", "foo@dir-a", &one);
        seed_skill_row(&db, "claude-code", "foo@dir-b", &two);

        // 只有一家 → 不该报「多家」，而是走到 path 那一关。
        let err = remove(&rm_opts(&db, home.path(), false), "foo", None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--path"), "只有一家时该问 path: {msg}");
        assert!(!msg.contains("claude-code, claude-code"), "同一家不许列两次: {msg}");
    }

    /// 干跑：不删、不动索引；只报将删什么。skill 删除没有归档这回事
    /// （见 [`remove`] 的文档），所以干跑里也没有「将归档到哪」可报——
    /// `archived` 恒为 None，导出目录不许被创建。
    #[test]
    fn rm_干跑不动盘() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let dir = home.path().join(".claude").join("skills").join("foo");
        make_skill(&dir, "foo", "body");
        seed_skill_row(&db, "claude-code", "foo", &dir);

        let report = remove(&rm_opts(&db, home.path(), true), "foo", None, None).unwrap();
        assert!(dir.is_dir(), "预览不许删");
        assert!(report.archived.is_none(), "干跑同样没有归档可报");
        assert!(
            !home.path().join("agent-duster-exports").exists(),
            "预览不许写导出目录"
        );
        assert!(report.removed.contains(&dir), "预览要报将删什么");
        assert!(list(Some(&db)).unwrap().len() == 1, "索引行不许动");
    }
}
