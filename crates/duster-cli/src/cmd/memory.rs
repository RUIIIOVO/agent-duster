//! `duster memory list/show/migrate`：把各家的「记忆」摆成一张表、读其中
//! 一条，或把一条的文本复制进另一个 agent 的记忆文件（哨兵块）。
//!
//! 引擎在 `duster_core::memory`（list/show 只读；migrate 只写哨兵块，
//! 合并与投影导出排 M3）。这里只做外壳：过滤、排版、把 key 递回去。
//!
//! # key 就是 PATH 列;TTY 上截断,重定向不截
//!
//! `show` 的参数是 `list` 在 PATH 列里打的那一串,逐字。旧版因此让 PATH 列
//! 永不截断——截断等于把这条命令的唯一入口切断,用户复制到的东西喂回去
//! 必然报错。代价是路径很长(qoder 的一条记忆能有 120 字符),窄终端下
//! 整张表被顶出屏幕。
//!
//! 交互菜单的逐条浏览改变了这笔账:`memory list` 在菜单里走 `browse` 下钻,
//! 选中的行把折叠后的完整 key 直接递给 `show`,不经手抄——TTY 上 PATH 列
//! 截到终端余量(`Table::flex_col`)不再切断任何东西。而重定向/管道里 stdout
//! 不是 TTY,flex 不生效,PATH 依旧完整落进文件,逐行核对照旧成立。
//!
//! 折叠仍保留:`$HOME` 前缀显示成 `~`,`show` 那头再展开回去,两种写法都收。
//! 折叠只发生在人类模式,`--json` 里恒为绝对路径(机器不折行)。
//!
//! # STORE 列存在的理由只有一个
//!
//! SQLite 里的一条记忆，key 长成 `<库路径>#<rowid>`。不告诉用户这一列是
//! 库不是文件，那个 `#31` 就是个没来由的怪东西；标出 `sqlite` 之后它才
//! 有解释。file / directory 两档同理——它们决定 M3 能不能写回去。

use std::path::Path;

use clap::Subcommand;
use console::style;

use duster_core::memory::{
    self, MemoryEntry, MemoryStore, MigrateAction, MigrateReport, RemoveKind, RemoveReport,
};

use crate::output::{
    EXIT_CONFIRM_DENIED, EXIT_OK, EXIT_PARTIAL, OutputMode, Table, accent, display_width,
    emit_json, human_bytes, muted, truncate_width,
};
use crate::{fail, render_warnings};

#[derive(Subcommand)]
pub enum MemoryCmd {
    /// Show every memory your agents keep, and how big it is
    List {
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
    },
    /// Print one memory. The key is the PATH column of `duster memory list`
    Show {
        /// Key exactly as `duster memory list` prints it
        #[arg(value_name = "KEY")]
        key: String,
    },
    /// Copy one memory into another agent's memory file, wrapped in a marked block
    Migrate {
        /// Source agent id, e.g. --from claude-code
        #[arg(long, value_name = "AGENT")]
        from: String,
        /// Target agent id, e.g. --to codex
        #[arg(long, value_name = "AGENT")]
        to: String,
        /// Which memory to copy when the source agent keeps several
        /// (the PATH column of `duster memory list`)
        #[arg(long, value_name = "KEY")]
        key: Option<String>,
        /// Print the block that would be written without touching anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete a memory: cut one duster block, delete a duster-created file,
    /// or delete a whole file you wrote yourself
    Rm {
        /// Key exactly as `duster memory list` prints it
        #[arg(value_name = "KEY")]
        key: String,
        /// Cut the block migrated from this agent (e.g. --from codex)
        #[arg(long, value_name = "AGENT")]
        from: Option<String>,
        /// Delete the whole file instead of cutting a block (two confirmations
        /// in the menu; the file is archived first)
        #[arg(long)]
        whole_file: bool,
        /// Delete without packing an archive first. Refused for files you
        /// wrote yourself — duster never deletes those without a way back.
        #[arg(long)]
        no_archive: bool,
        /// Show what would be deleted without touching anything
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(mode: OutputMode, index: Option<&Path>, action: &MemoryCmd) -> i32 {
    match action {
        MemoryCmd::List { agents } => list(mode, index, agents),
        MemoryCmd::Show { key } => show(mode, index, key),
        MemoryCmd::Migrate {
            from,
            to,
            key,
            dry_run,
        } => migrate(mode, index, from, to, key.as_deref(), *dry_run),
        MemoryCmd::Rm {
            key,
            from,
            whole_file,
            no_archive,
            dry_run,
        } => rm(mode, index, key, from.as_deref(), *whole_file, *no_archive, *dry_run),
    }
}

/// 列出全部记忆。
///
/// 有 warning 就落退出码 3：一条记忆读不出来时这张表是**不全**的，
/// 而"不全"和"这就是全部"对用户是两回事，脚本也该能分开。
///
/// `--agent` 是在**结果上**过滤的：`memory::list` 没有 agent 参数，
/// 一次调用就是全量扫描。于是别家的 warning 也会出现在 `--agent codex`
/// 的输出里，退出码同样落 3。留着是刻意的——那条 warning 描述的是这次
/// 扫描里真实发生的失败，按 agent 猜着丢掉它，等于用一个我证明不了的
/// "完整"去换一行清静。
fn list(mode: OutputMode, index: Option<&Path>, agents: &[String]) -> i32 {
    let all = match memory::list(index, None) {
        Ok(l) => l,
        Err(e) => return fail(mode, "memory-list", &e),
    };
    let entries: Vec<&MemoryEntry> = all
        .entries
        .iter()
        .filter(|e| agents.is_empty() || agents.contains(&e.agent_id))
        .collect();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();

    match mode {
        OutputMode::Json => emit_json(
            "memory-list",
            &serde_json::json!({
                "entries": entries,
                "total_bytes": total,
            }),
            &all.warnings,
        ),
        OutputMode::Human => {
            render_list(&entries, total, agents, &all.entries);
            render_warnings(&all.warnings);
        }
    }
    if all.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// SCOPE 列的宽度上限。项目段是**有损编码**串（`Users-x-Code-a-b`），
/// 不是给人复制的地址，只是个分组标签，所以这一列可以截。
///
/// 17 是 80 列预算里挤出来的：AGENT 11 + STORE 9 + SIZE 6 + 列间空隙 10 +
/// 缩进 2 已占 38 列，SCOPE 与 TITLE 合起来只能拿 38 列，余下 4 列给 PATH。
/// `flex_col` 只截 PATH，其余列必须自己先放得下，PATH 表头才不会在窄终端上
/// 被截成省略号。
const SCOPE_MAX: usize = 17;

/// TITLE 列的宽度上限。
///
/// 标题是散文（qoder 的一条能有 66 个字符），截了还认得出是哪条。PATH 列
/// 的宽账记在 flex_col 头上（见模块文档）：TTY 上吃终端余量，重定向里不截。
/// 21 与 SCOPE_MAX 的 17 合起来正好填满 80 列预算里剩下的 38 列——
/// 多给 TITLE 四列，因为它才是认得出哪条记忆的那一列。
const TITLE_MAX: usize = 21;

/// 空表的说明文本:说清是「哪都没有」还是「这个 agent 没有」——后者附上
/// 有哪些 agent 真有记忆,省得用户挨个试 agent id。命令与交互菜单共用一份
/// 措辞:同一件事只有一句问法。
pub(crate) fn empty_list_message(agents: &[String], all: &[MemoryEntry]) -> String {
    match agents {
        [a] if !all.is_empty() => {
            let mut ids: Vec<&str> = all.iter().map(|e| e.agent_id.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            format!(
                "No memories for {a}. Agents that do keep memories: {}",
                ids.join(", ")
            )
        }
        _ => "No memories indexed. Run `duster scan` first, or your agents keep none.".to_string(),
    }
}

fn render_list(entries: &[&MemoryEntry], total: u64, agents: &[String], all: &[MemoryEntry]) {
    println!();
    if entries.is_empty() {
        println!("  {}", muted().apply_to(empty_list_message(agents, all)));
        return;
    }

    let mut t = Table::new(vec!["AGENT", "SCOPE", "TITLE", "STORE", "SIZE", "PATH"]);
    t.color_col(0, accent());
    t.right_align(&[4]);
    t.color_col(5, muted());
    // 路径列吃终端余量:交互菜单里选中即 `show`(key 直接递,不手抄),
    // TTY 上截断不再切断入口;重定向不是 TTY,这里不生效,路径完整落文件。
    t.flex_col(5);
    for e in entries {
        t.push_row(vec![
            e.agent_id.clone(),
            scope_cell(e),
            truncate_width(&e.title, TITLE_MAX),
            store_label(e.store).to_string(),
            human_bytes(e.bytes),
            // key 列:只折 `$HOME`,折叠形态就是 `show` 接受的参数。
            fold_home(&e.path.display().to_string()),
        ]);
    }
    println!("{}", t.render());

    println!();
    println!(
        "  {} {}",
        style(plural_memories(entries.len())).bold(),
        muted().apply_to(format!(
            "· {} · read one with `duster memory show <path>`",
            human_bytes(total)
        ))
    );
}

/// 复数：memory → memories，`main.rs` 的 `plural` 那份只会加 `s`。
///
/// 不去改它：那份服务的是 item / skill / credential 一类的规则复数，
/// 为一个特例给它加一张不规则名词表，是把复杂度放错了地方。
fn plural_memories(n: usize) -> String {
    if n == 1 {
        "1 memory".to_string()
    } else {
        format!("{n} memories")
    }
}

/// SCOPE 单元格：全局记忆写 `global`，项目级写项目段。
///
/// 截断保**尾**：项目段形如 `Users-laibu-Documents-Code-<项目名>`，
/// 前半截家家一样，砍掉头才留得住能区分彼此的那一截。
fn scope_cell(e: &MemoryEntry) -> String {
    match &e.project {
        None => "global".to_string(),
        Some(p) => keep_tail(p, SCOPE_MAX),
    }
}

/// 按显示宽度保留尾部，前面用 `…` 顶掉。
fn keep_tail(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut rev = String::new();
    let mut width = 0;
    // 从右往左收字符，留一列给省略号。
    for c in s.chars().rev() {
        let w = display_width(&c.to_string());
        if width + w > max.saturating_sub(1) {
            break;
        }
        width += w;
        rev.push(c);
    }
    let tail: String = rev.chars().rev().collect();
    format!("…{tail}")
}

/// STORE 列的人话。三档的区别是**载体**，不是格式。
fn store_label(store: MemoryStore) -> &'static str {
    match store {
        MemoryStore::Markdown => "file",
        MemoryStore::MarkdownDir => "directory",
        MemoryStore::Sqlite => "sqlite",
    }
}

/// 给交互菜单的逐条浏览:表头 + 对齐行 + 每行对应的 `show` key。
///
/// 列与 [`render_list`] 一致;PATH 列按终端余量截——菜单里选中即 `show`
/// (key 是折叠后的完整路径,与显示文本无关),手抄不再是唯一入口,截断的
/// 代价消失了。行文本是纯文本,宽度按 [`display_width`] 算,宽字符不顶歪。
pub(crate) fn browse_rows(entries: &[&MemoryEntry]) -> (String, Vec<String>, Vec<String>) {
    const HEAD: [&str; 6] = ["AGENT", "SCOPE", "TITLE", "STORE", "SIZE", "PATH"];
    let cells: Vec<[String; 6]> = entries
        .iter()
        .map(|e| {
            [
                e.agent_id.clone(),
                scope_cell(e),
                truncate_width(&e.title, TITLE_MAX),
                store_label(e.store).to_string(),
                human_bytes(e.bytes),
                fold_home(&e.path.display().to_string()),
            ]
        })
        .collect();
    // 前五列定宽(表头 + 本批最宽);PATH 吃余量。
    let mut w = [0usize; 5];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .chain(std::iter::once(HEAD[c].len()))
            .max()
            .unwrap_or(0);
    }
    // 控件前缀(`❯ [x] `,浏览表带勾选)+ 五列间各两空格 + 尾部留一列,
    // 余下的全给 PATH。
    let fixed: usize =
        crate::output::Prefix::Checkbox.width() + w.iter().sum::<usize>() + 2 * 5 + 1;
    let path_w = terminal_cols().saturating_sub(fixed).max(16);
    let line = |r: &[String; 6]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {:>w4$}  {}",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            truncate_width(&r[5], path_w),
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4],
        )
    };
    let header = line(&HEAD.map(str::to_string));
    let keys = cells.iter().map(|r| r[5].clone()).collect();
    (header, cells.iter().map(line).collect(), keys)
}

/// 终端列数(stderr——菜单渲染在 stderr 上);取不到按 100 列算。
/// 这一屏只在 TTY 下出现,取不到宽度是异常而不是常态。
fn terminal_cols() -> usize {
    console::Term::stderr()
        .size_checked()
        .map_or(100, |(_, cols)| cols as usize)
}

/// 打印一条记忆的正文。
fn show(mode: OutputMode, index: Option<&Path>, key: &str) -> i32 {
    let expanded = expand_home(key);
    match memory::show(index, None, &expanded) {
        Ok(body) => {
            match mode {
                OutputMode::Json => emit_json(
                    "memory-show",
                    &serde_json::json!({ "key": expanded, "markdown": body }),
                    &[],
                ),
                // 正文原样落 stdout:它是结果本身,加缩进/加边框都会让
                // `duster memory show x > x.md` 出来的东西不是原文。
                OutputMode::Human => print!("{}", ensure_newline(&body)),
            }
            EXIT_OK
        }
        Err(e) => {
            let e = if is_unknown_key(&e) {
                with_suggestions(index, key, e)
            } else {
                e
            };
            fail(mode, "memory-show", &e)
        }
    }
}

/// 正文末尾补一个换行。少了它，下一个 shell 提示符会贴在最后一行字上。
fn ensure_newline(body: &str) -> String {
    if body.ends_with('\n') {
        body.to_string()
    } else {
        format!("{body}\n")
    }
}

/// 把 `--from <agent> [--key]` 解析成唯一的源 key，再执行迁移。
///
/// 解析要一张列表打底，所以先跑 `memory::list`；真正的归属判定在 core
/// （哨兵块的 from= 是索引反查的结果），这里的校验只为把错误提早说清。
fn migrate(
    mode: OutputMode,
    index: Option<&Path>,
    from: &str,
    to: &str,
    key: Option<&str>,
    dry_run: bool,
) -> i32 {
    let all = match memory::list(index, None) {
        Ok(l) => l,
        Err(e) => return fail(mode, "memory-migrate", &e),
    };
    let expanded = key.map(expand_home);
    let key = match resolve_source(&all.entries, from, expanded.as_deref()) {
        Ok(k) => k,
        Err(msg) => return fail(mode, "memory-migrate", &anyhow::anyhow!(msg)),
    };
    migrate_execute(mode, index, &key, to, dry_run)
}

/// 从 `--from <agent> [--key <k>]` 定出唯一的源 key。
///
/// 恰好一条时 `--key` 可省；多条必须点名，候选直接列进错误里——一句
/// 光秃秃的「请加 --key」等于把用户赶回去再跑一遍 list。`--key` 顺手做
/// 归属校验：拿别家的 key 配 `--from`，报出来的是「不是 <from> 的记忆」，
/// 而不是迁完才发现哨兵里的 from= 不是想要的那个。
fn resolve_source(
    entries: &[MemoryEntry],
    from: &str,
    key: Option<&str>,
) -> Result<String, String> {
    let mine: Vec<&MemoryEntry> = entries.iter().filter(|e| e.agent_id == from).collect();
    if mine.is_empty() {
        // agent 拼错和 agent 没记忆在这张列表上分不出来（list 只有有记忆
        // 的 agent）；把有记忆的列出来，两种情况都够用户自查。
        let mut who: Vec<&str> = entries.iter().map(|e| e.agent_id.as_str()).collect();
        who.sort_unstable();
        who.dedup();
        return Err(if who.is_empty() {
            format!("{from} has no migratable memory (no agent has any)")
        } else {
            format!(
                "{from} has no migratable memory. Agents that do: {}",
                who.join(", ")
            )
        });
    }
    let keys = || {
        mine.iter()
            .map(|e| format!("  {}", fold_home(&e.path.display().to_string())))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match key {
        None if mine.len() == 1 => Ok(mine[0].path.display().to_string()),
        None => Err(format!(
            "{from} keeps {} memories; pick one with --key:\n{}",
            mine.len(),
            keys()
        )),
        Some(k) => mine
            .iter()
            .find(|e| e.path.display().to_string() == k)
            .map(|e| e.path.display().to_string())
            .ok_or_else(|| {
                format!("--key does not match any {from} memory. Its keys:\n{}", keys())
            }),
    }
}

/// 执行迁移并渲染报告。交互菜单选完目标后也走这里：同一份报告只有
/// 一种长相。dry-run 与 clean/prune 的仅预览同一档退出码（4）。
pub(crate) fn migrate_execute(
    mode: OutputMode,
    index: Option<&Path>,
    key: &str,
    to: &str,
    dry_run: bool,
) -> i32 {
    let report = match memory::migrate(index, None, key, to, dry_run) {
        Ok(r) => r,
        Err(e) => {
            let e = if is_unknown_key(&e) {
                with_suggestions(index, key, e)
            } else {
                e
            };
            return fail(mode, "memory-migrate", &e);
        }
    };
    let warnings: Vec<String> = report.refresh_hint.clone().into_iter().collect();
    match mode {
        OutputMode::Json => emit_json("memory-migrate", &report, &warnings),
        OutputMode::Human => {
            render_migrate(&report);
            render_warnings(&warnings);
        }
    }
    if report.dry_run {
        EXIT_CONFIRM_DENIED
    } else if warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// 迁移报告的人话。dry-run 把将写入的块原样落 stdout——它就是结果本身，
/// 加缩进会让「复制出来自己贴」不再成立。
fn render_migrate(r: &MigrateReport) {
    let target = fold_home(&r.target.display().to_string());
    if r.dry_run {
        let verb = match r.action {
            MigrateAction::Replaced => format!("replace the from={} block in", r.from_agent),
            MigrateAction::Appended => "append to".to_string(),
            MigrateAction::Created => "create".to_string(),
        };
        println!("Would {verb} {target}:");
        println!();
        print!("{}", r.block);
        return;
    }
    println!(
        "Migrated {} ({}) -> {} [{}]",
        fold_home(&r.source),
        r.from_agent,
        accent().apply_to(target),
        r.action.as_str()
    );
}

/// 执行 `duster memory rm` 并渲染报告。交互菜单的删除动作也走这里：
/// 同一份报告只有一种长相。dry-run 与 clean/prune 的仅预览同一档退出码（4）。
pub(crate) fn rm_execute(
    mode: OutputMode,
    index: Option<&Path>,
    key: &str,
    from: Option<&str>,
    whole_file: bool,
    archive: bool,
    dry_run: bool,
) -> i32 {
    let report = match memory::remove(index, None, key, from, whole_file, archive, dry_run) {
        Ok(r) => r,
        Err(e) => {
            let e = if is_unknown_key(&e) {
                with_suggestions(index, key, e)
            } else {
                e
            };
            return fail(mode, "memory-rm", &e);
        }
    };
    let warnings = report.warnings.clone();
    match mode {
        OutputMode::Json => emit_json("memory-rm", &report, &warnings),
        OutputMode::Human => {
            render_remove(&report);
            render_warnings(&warnings);
        }
    }
    if report.dry_run {
        EXIT_CONFIRM_DENIED
    } else if warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// `rm` 命令入口：key 折叠展开后直接执行。命令本身没有交互确认——
/// 敲下这条命令就是确认；两道 y/N 的门在菜单里（见 interactive.rs）。
fn rm(
    mode: OutputMode,
    index: Option<&Path>,
    key: &str,
    from: Option<&str>,
    whole_file: bool,
    no_archive: bool,
    dry_run: bool,
) -> i32 {
    let expanded = expand_home(key);
    rm_execute(mode, index, &expanded, from, whole_file, !no_archive, dry_run)
}

/// 删除报告的人话。三类目标各说各的：切块亮出 from= 与块所在文件，
/// 整删亮出被删的文件；归档包单独一行报出来（dry-run 报"将归档到哪"）。
pub(crate) fn render_remove(r: &RemoveReport) {
    render_remove_line(r);
    render_remove_archive(r);
}

/// 每份报告那一行：删了什么、释放了多少。
pub(crate) fn render_remove_line(r: &RemoveReport) {
    let verb = if r.dry_run { "Would delete" } else { "Deleted" };
    let where_ = if r.removed.is_empty() {
        r.modified.first().cloned().unwrap_or_default()
    } else {
        r.removed[0].clone()
    };
    let shown = fold_home(&where_.display().to_string());
    match r.kind {
        RemoveKind::DusterBlock => {
            let from = r.from_agent.as_deref().unwrap_or("?");
            println!(
                "{} the from={} duster block in {} ({} freed)",
                verb,
                from,
                accent().apply_to(shown),
                human_bytes(r.freed_bytes),
            );
        }
        RemoveKind::DusterFile => {
            println!(
                "{} the duster-created memory file {} ({} freed)",
                verb,
                accent().apply_to(shown),
                human_bytes(r.freed_bytes),
            );
        }
        RemoveKind::UserFile => {
            println!(
                "{} your memory file {} ({} freed)",
                verb,
                accent().apply_to(shown),
                human_bytes(r.freed_bytes),
            );
        }
    }
}

/// 归档那一行：真跑了报实际落点，dry-run 报"将归档到哪"（没落盘，但
/// 用户该知道真跑时会去哪）。
fn render_remove_archive(r: &RemoveReport) {
    match &r.archived {
        Some(a) => println!(
            "  {}",
            muted().apply_to(format!(
                "archived to {}",
                fold_home(&a.display().to_string())
            ))
        ),
        None if r.dry_run => {
            if let Ok(dest) = memory::remove_archive_dest(None) {
                println!(
                    "  {}",
                    muted().apply_to(format!(
                        "would archive to {}",
                        fold_home(&dest.display().to_string())
                    ))
                );
            }
        }
        None => {}
    }
}

/// core 判定"这个 key 不指向任何已索引的记忆"时的稳定文案。
///
/// 分层铁律禁止 CLI import 下层 crate、downcast 不可达，只能匹配 `Display`
/// 的稳定子串（`exit_code_for` 对锁与拒绝文案是同一套做法），视作跨层契约。
const UNKNOWN_KEY_PHRASE: &str = "not a known memory location";

fn is_unknown_key(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains(UNKNOWN_KEY_PHRASE)
}

/// 最多给几条候选。三条是因为它要能一眼扫完：列出二十条近似 key
/// 和不给建议一样没用。
const SUGGEST_MAX: usize = 3;

/// 把「最接近的几个 key」贴到错误后面。
///
/// 一句光秃秃的"没这个 key"把用户推回去重跑 `list`、再从上百行里找那一条。
/// 差一个字符的时候，正确答案就该直接摆在报错里。
///
/// 取不到候选（索引读不了、一条记忆都没有）就原样返回：为了给建议而把
/// 原来的错误换成另一个错误，只会让人更迷惑。
fn with_suggestions(index: Option<&Path>, key: &str, err: anyhow::Error) -> anyhow::Error {
    let Ok(all) = memory::list(index, None) else {
        return err;
    };
    let keys: Vec<String> = all
        .entries
        .iter()
        .map(|e| fold_home(&e.path.display().to_string()))
        .collect();
    let near = nearest(&keys, key, SUGGEST_MAX);
    if near.is_empty() {
        return err;
    }
    anyhow::anyhow!(
        "{err:#}\n    Did you mean:\n{}",
        near.iter()
            .map(|k| format!("      {k}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// 按「像不像」挑前 `max` 个候选。
///
/// 排序键两段：**先看包含关系**——用户十有八九是把标题或文件名贴了进来，
/// 那种情况下编辑距离很大（长路径 vs 短片段）却明显是同一条；
/// 包含关系相同再比编辑距离。同分按 key 字典序，保证同一次输入两次运行
/// 给同一批建议。
fn nearest(keys: &[String], query: &str, max: usize) -> Vec<String> {
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &String)> = keys
        .iter()
        .map(|k| {
            let lower = k.to_lowercase();
            let apart = u8::from(!(lower.contains(&q) || q.contains(&lower)));
            (apart, levenshtein(&lower, &q), k)
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(max)
        .map(|(_, _, k)| k.clone())
        .collect()
}

/// 编辑距离（两行滚动数组）。按字符走，不按字节——记忆标题里有中文，
/// 按字节算会把一个汉字记成三次编辑。
fn levenshtein(a: &str, b: &str) -> usize {
    let bc: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=bc.len()).collect();
    let mut cur: Vec<usize> = vec![0; bc.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in bc.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bc.len()]
}

/// `$HOME/x` → `~/x`。取不到 `HOME` 就原样返回。
fn fold_home(path: &str) -> String {
    fold_under(path, home_prefix().as_deref())
}

/// `~/x` → `$HOME/x`。取不到 `HOME` 就原样返回。
fn expand_home(key: &str) -> String {
    expand_under(key, home_prefix().as_deref())
}

/// 两个折叠函数的纯形态,`home` 显式传入。
///
/// 抽出来是为了能测:靠 `set_var("HOME", ...)` 去测,既要在 edition 2024 里
/// 写 `unsafe`,又会和同进程并行跑的别的测试抢同一个环境变量。
fn fold_under(path: &str, home: Option<&str>) -> String {
    match home.and_then(|h| path.strip_prefix(h)) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// `~/x` → `<home>/x`。只认前导 `~/`——路径中间的 `~` 是合法文件名字符。
fn expand_under(key: &str, home: Option<&str>) -> String {
    match (key.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => format!("{h}/{rest}"),
        _ => key.to_string(),
    }
}

/// 主目录（末尾不带 `/`）。空值当没有——`strip_prefix("")` 会把每条路径
/// 都折成 `~<绝对路径>`。
fn home_prefix() -> Option<String> {
    let h = std::env::var("HOME").ok()?;
    let h = h.trim_end_matches('/');
    if h.is_empty() {
        None
    } else {
        Some(h.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<String> {
        [
            "~/.claude/CLAUDE.md",
            "~/.codex/AGENTS.md",
            "~/.codex/memories_1.sqlite#31",
            "~/.gemini/GEMINI.md",
            "~/.qoder/memories/abc/global/coding/style.md",
            "~/.qoder/memories/abc/projects/Users-x-Code-duster/coding/rust.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// 打错一个字符时，正确答案要直接出现在报错里，而不是把人推回去重跑 list。
    #[test]
    fn 未知_key_给出最接近的几条() {
        let near = nearest(&keys(), "~/.codex/AGENT.md", SUGGEST_MAX);
        assert_eq!(near[0], "~/.codex/AGENTS.md");
        assert_eq!(near.len(), SUGGEST_MAX);
    }

    /// 贴进来的是文件名或一段路径（编辑距离很大但明显是同一条）时，
    /// 包含关系必须压过编辑距离。
    #[test]
    fn 只贴片段时按包含关系优先() {
        assert_eq!(
            nearest(&keys(), "GEMINI.md", 1),
            ["~/.gemini/GEMINI.md".to_string()]
        );
        assert_eq!(
            nearest(&keys(), "memories_1.sqlite", 1),
            ["~/.codex/memories_1.sqlite#31".to_string()]
        );
    }

    /// 一条记忆都没有时不硬凑建议——那只会把用户引向不存在的 key。
    #[test]
    fn 没有候选时不给建议() {
        assert!(nearest(&[], "anything", SUGGEST_MAX).is_empty());
    }

    /// 编辑距离按字符走：一个汉字算一次编辑，不是三次。
    #[test]
    fn 编辑距离按字符不按字节() {
        assert_eq!(levenshtein("记忆", "记录"), 1);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    /// 折叠与展开必须互为逆运算，否则 `list` 打出来的 key 喂回 `show` 会失败——
    /// 那条链路是这个命令组唯一的用法。
    #[test]
    fn home_折叠与展开互逆() {
        let home = Some("/Users/tester");
        let abs = "/Users/tester/.codex/AGENTS.md";
        assert_eq!(fold_under(abs, home), "~/.codex/AGENTS.md");
        assert_eq!(expand_under(&fold_under(abs, home), home), abs);
        // 不在 home 下的路径两个方向都原样透出。
        assert_eq!(fold_under("/opt/x.md", home), "/opt/x.md");
        assert_eq!(expand_under("/opt/x.md", home), "/opt/x.md");
        // 路径中间的 `~` 是合法文件名字符，不许被当成主目录。
        assert_eq!(expand_under("/tmp/a~/b.md", home), "/tmp/a~/b.md");
        // 取不到 home 就谁也不动——绝不能把绝对路径折成 `~<绝对路径>`。
        assert_eq!(fold_under(abs, None), abs);
        assert_eq!(expand_under("~/x", None), "~/x");
    }

    /// SCOPE 截断保尾：项目段的前半截家家一样，能区分彼此的在末尾。
    #[test]
    fn scope_截断保留尾部() {
        let long = "Users-laibu-Documents-Code-agent-duster";
        let cell = keep_tail(long, 20);
        assert!(cell.ends_with("agent-duster"), "{cell}");
        assert!(cell.starts_with('…'), "{cell}");
        assert!(display_width(&cell) <= 20, "{cell}");
        // 够短就原样，不加省略号。
        assert_eq!(keep_tail("global", 20), "global");
    }

    /// STORE 三档各有各的意思：`#rowid` 形状的 key 只可能来自 sqlite 那档。
    #[test]
    fn store_三档标签互不相同() {
        let labels = [
            store_label(MemoryStore::Markdown),
            store_label(MemoryStore::MarkdownDir),
            store_label(MemoryStore::Sqlite),
        ];
        assert_eq!(labels, ["file", "directory", "sqlite"]);
    }

    /// 正文末尾补换行：少了它下一个提示符会贴在最后一行字上。
    #[test]
    fn 正文补足结尾换行() {
        assert_eq!(ensure_newline("x"), "x\n");
        assert_eq!(ensure_newline("x\n"), "x\n");
    }

    /// 复数走不规则形式："1 memorys" 这种小破绽会让人连带怀疑其余数字。
    #[test]
    fn 复数形式是_memories() {
        assert_eq!(plural_memories(1), "1 memory");
        assert_eq!(plural_memories(0), "0 memories");
        assert_eq!(plural_memories(12), "12 memories");
    }

    /// `--from` 只有一条记忆时 `--key` 可省；多条时错误里列全候选。
    #[test]
    fn migrate_源解析_单条免_key_多条点名() {
        let entries = vec![
            entry("claude-code", "/h/.claude/CLAUDE.md"),
            entry("qoder", "/h/.qoder/memories/a.md"),
            entry("qoder", "/h/.qoder/memories/b.md"),
        ];
        assert_eq!(
            resolve_source(&entries, "claude-code", None).unwrap(),
            "/h/.claude/CLAUDE.md"
        );
        let err = resolve_source(&entries, "qoder", None).unwrap_err();
        assert!(err.contains("pick one with --key"), "{err}");
        assert!(err.contains("a.md") && err.contains("b.md"), "{err}");
        assert_eq!(
            resolve_source(&entries, "qoder", Some("/h/.qoder/memories/b.md")).unwrap(),
            "/h/.qoder/memories/b.md"
        );
    }

    /// 拿别家的 key 配 `--from` 要被拦下（归属校验）；没记忆的 agent
    /// 报错里列出真有记忆的那些。
    #[test]
    fn migrate_源解析_归属与存在性都要对() {
        let entries = vec![
            entry("claude-code", "/h/.claude/CLAUDE.md"),
            entry("codex", "/h/.codex/AGENTS.md"),
        ];
        let err = resolve_source(&entries, "codex", Some("/h/.claude/CLAUDE.md")).unwrap_err();
        assert!(err.contains("does not match any codex memory"), "{err}");
        let err = resolve_source(&entries, "gemini-cli", None).unwrap_err();
        assert!(err.contains("gemini-cli has no migratable memory"), "{err}");
        assert!(err.contains("claude-code") && err.contains("codex"), "{err}");
    }

    /// resolve_source 的夹具行：只有归属和 key 参与判定，其余字段填零值。
    fn entry(agent: &str, path: &str) -> MemoryEntry {
        MemoryEntry {
            agent_id: agent.to_string(),
            project: None,
            title: path.to_string(),
            category: None,
            store: MemoryStore::Markdown,
            path: std::path::PathBuf::from(path),
            bytes: 0,
            mtime_ms: 0,
        }
    }
}
