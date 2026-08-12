//! skill 的横向操作：跨 agent 去重 / 漂移检测 / 链接。
//!
//! 同一份 skill 常常在三四个 agent 目录下各躺一份。三种状态要分清：
//! - **IDENTICAL**：树哈希全等。可以链接成一份，省的是体积也是维护成本。
//! - **DRIFTED**：同名不同哈希。**这是要报出来的那个**——用户以为三处一样，
//!   实际上早就各自改过了，改了哪儿得给出 diff。
//! - 只有一份：不参与。
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
use duster_model::ResourceKind;

use crate::diff::{Change, DiffOptions, diff_trees};

/// 一组同名 skill 的比较结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DupState {
    /// 树哈希全等。
    Identical,
    /// 同名不同哈希。
    Drifted,
}

/// 一份副本。
#[derive(Debug, Clone, Serialize)]
pub struct SkillCopy {
    pub agent_id: String,
    pub path: PathBuf,
    /// 剪枝后的树哈希（hex）。
    pub tree_hash: String,
    /// 剪枝后的用户内容体积。
    pub bytes: u64,
    /// 目录内嵌的 install 子路径体积（不参与哈希，单独报出来）。
    pub install_bytes: u64,
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

/// 跨 agent 扫描全部 skill，按名分组比较。只返回副本数 ≥ 2 的组。
///
/// 读索引拿路径，现场算树哈希（索引里的 `hash_content` 对 skill 恒为空——
/// 目录树哈希开销大，只在这里按需算）。
pub fn copies(index_path: Option<&std::path::Path>) -> Result<Vec<SkillGroup>> {
    let idx = open_index(index_path)?;
    let groups = duster_index::query::skill_groups(idx.conn())?;
    let prune = install_prune();

    let mut out: Vec<SkillGroup> = Vec::new();
    for (name, rows) in groups {
        // 索引里就只有一份的组不参与：没有可比对象，报出来只是噪音。
        if rows.len() < 2 {
            continue;
        }

        let mut copies: Vec<SkillCopy> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        for r in &rows {
            let root = PathBuf::from(&r.path);
            if !root.is_dir() {
                warnings.push(format!(
                    "copy for agent `{}` is gone from disk: {} (index is stale, re-run `duster scan`)",
                    r.agent_id, r.path
                ));
                continue;
            }
            match measure_copy(&root, &prune) {
                Ok(m) => {
                    copies.push(SkillCopy {
                        agent_id: r.agent_id.clone(),
                        path: root,
                        tree_hash: m.tree_hash,
                        bytes: m.bytes,
                        install_bytes: m.install_bytes,
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

        let drifted = copies.iter().any(|c| c.tree_hash != copies[0].tree_hash);
        let diff = if drifted {
            Some(build_diff(&copies, &prune, &mut warnings)?)
        } else {
            None
        };

        out.push(SkillGroup {
            name,
            state: if drifted {
                DupState::Drifted
            } else {
                DupState::Identical
            },
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
            "source skill directory is gone: {} (index is stale, re-run `duster scan`)",
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

/// 树哈希与体积统计时剪掉的 install 子路径名。
///
/// 与清单里 skill 资源的 `install_paths` 默认值保持一致——三处口径
/// （status 体积 / 归档排除 / copies 检测的树哈希）必须是同一份定义。
pub const DEFAULT_INSTALL_DIRS: [&str; 4] = ["node_modules", "dist", "bin", ".git"];

/// [`DEFAULT_INSTALL_DIRS`] 的 `Vec<String>` 形态（walk / hash 的入参口径）。
fn install_prune() -> Vec<String> {
    DEFAULT_INSTALL_DIRS.iter().map(|s| s.to_string()).collect()
}

/// 与 [`crate::status::status`] 同一套只读打开：同一默认路径、同一句提示。
fn open_index(index_path: Option<&Path>) -> Result<Index> {
    let path: PathBuf = match index_path {
        Some(p) => p.to_path_buf(),
        None => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    };
    if !path.is_file() {
        bail!(
            "index database not found: {}. Run `duster scan` first to build it.",
            path.display()
        );
    }
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

/// 逗号分隔；空集给一句人话而不是空字符串。
fn join_or_none<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<&str> = items.collect();
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

/// 一份副本的现场测量结果。
struct CopyMeasure {
    tree_hash: String,
    /// 剪枝后的用户内容体积。
    bytes: u64,
    /// 剪掉那部分的体积。
    install_bytes: u64,
}

/// 现场测量一份副本：树哈希 + 体积拆分。
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
    walk_files(root, &opts, |_p, meta| {
        if !meta.is_file() {
            return; // 与 hash_tree 一致：只认常规文件。
        }
        bytes += meta.len();
    })
    .with_context(|| format!("failed to walk skill directory: {}", root.display()))?;

    let tree_hash = hash_tree(root, prune)
        .with_context(|| format!("failed to hash skill directory: {}", root.display()))?
        .to_hex()
        .to_string();

    Ok(CopyMeasure {
        tree_hash,
        bytes,
        install_bytes: stats.pruned_bytes,
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

        let groups = copies(Some(&db)).unwrap();
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

        let groups = copies(Some(&db)).unwrap();
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

        let groups = copies(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, DupState::Drifted);
        let diff = groups[0].diff.as_deref().unwrap();
        assert!(diff.contains("SKILL.md"), "{diff}");
        assert!(
            diff.contains("claude-code") && diff.contains("codex"),
            "{diff}"
        );
    }

    #[test]
    fn 只有一份的_skill_不出现在结果里() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster").join("index.db");
        let a = home.path().join(".claude").join("skills").join("solo");
        make_skill(&a, "solo", "body");
        seed_skill_row(&db, "claude-code", "solo", &a);

        let groups = copies(Some(&db)).unwrap();
        assert!(groups.is_empty(), "{groups:#?}");
    }

    #[test]
    fn 副本目录消失只降级为警告不中断() {
        let home = tempfile::tempdir().unwrap();
        let (db, _, b) = two_copies(home.path(), "same body", "same body");
        std::fs::remove_dir_all(&b).unwrap();

        let groups = copies(Some(&db)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies.len(), 1);
        assert_eq!(groups[0].warnings.len(), 1, "{:#?}", groups[0]);
        assert!(groups[0].warnings[0].contains("codex"));
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
}
