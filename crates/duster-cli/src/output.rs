//! CLI 输出契约：全部命令的退出码、JSON 信封、表格与字节格式化都收敛到这里。
//!
//! # stdout / stderr 约定
//!
//! - **stdout 只放结果**：人类模式下是表格/摘要，`--json` 模式下是一行紧凑 JSON 信封。
//!   下游可以放心 `duster ... | jq`，stdout 永远可解析。
//! - **stderr 放过程**：进度、提示、警告、错误的人话描述全部走 stderr。
//!   `--json` 模式下出错时，信封写 stdout（机器读），同一条错误的人话写 stderr（人读）。
//!
//! # 退出码表
//!
//! | 码 | 常量 | 含义 |
//! |----|------|------|
//! | 0 | [`EXIT_OK`] | 成功 |
//! | 1 | [`EXIT_ERROR`] | 一般错误（IO 失败、解析失败等） |
//! | 2 | [`EXIT_USAGE`] | 用法错误（clap 默认，参数/子命令不合法） |
//! | 3 | [`EXIT_PARTIAL`] | 部分成功（如 scan 某 agent 失败但整体继续，细节进 warnings） |
//! | 4 | [`EXIT_CONFIRM_DENIED`] | 需要确认被拒绝 / dry-run 仅预览未执行（留给 clean / prune / uninstall） |
//! | 5 | [`EXIT_LOCKED`] | 锁冲突（agent 正在运行 / 索引被其他进程占用） |
//!
//! # `--json` 信封 schema
//!
//! 所有命令统一输出下面这一层信封，命令私有数据只填 `data`：
//!
//! ```json
//! {
//!   "ok": true,
//!   "command": "scan",
//!   "data": {},
//!   "warnings": ["某 agent 解析失败已跳过"],
//!   "error": null
//! }
//! ```
//!
//! - `ok`：`error == null` 时为 `true`；有 warnings 不影响 `ok`。
//! - `command`：子命令名（`scan` / `status` / `clean` / `prune` ...）。
//! - `data`：命令私有结构，成功时由各命令自定义；出错时为 `null`。
//! - `warnings`：非致命问题列表，永远是数组（可能为空）。
//! - `error`：`null` 或 `{"code": "机器可读短码", "message": "人话"}`。
//!   `code` 建议与退出码语义对应，如 `"partial"` / `"locked"` / `"io"`。

use console::{Style, StyledObject};
use serde::Serialize;
use std::io::Write;

// ---------------------------------------------------------------------------
// 退出码
// ---------------------------------------------------------------------------

/// 成功。
pub const EXIT_OK: i32 = 0;
/// 一般错误。
pub const EXIT_ERROR: i32 = 1;
/// 用法错误。clap 解析失败时自行返回 2;外壳在「命令根本没法执行」时也用它
/// (`prune` 缺 `--older-than`、`--older-than` 值不合法),脚本不必区分是谁发现的。
pub const EXIT_USAGE: i32 = 2;
/// 部分成功：整体流程走完，但个别子项失败（细节写进 warnings）。
pub const EXIT_PARTIAL: i32 = 3;
/// 需要确认被拒绝 / dry-run 仅预览未执行。
///
/// `clean` / `prune` / `uninstall` 默认只出计划，一个字节都不动即落这一档——
/// 脚本据此把「什么都没做」与「做完了」分开。core 明说"没动过"的拒绝
/// （`--json` 无 `--yes`、归档未表态、确认串不符、前置检查未过）同落这里。
pub const EXIT_CONFIRM_DENIED: i32 = 4;
/// 锁冲突：目标 agent 运行中或索引被其他进程占用。
pub const EXIT_LOCKED: i32 = 5;

/// 取两个退出码里更坏的那个。退出码的数值本身无序（`EXIT_PARTIAL` 是 3、
/// `EXIT_CONFIRM_DENIED` 是 4，后者反而更轻），所以不能用 `max`。
/// 未知码按「比 EXIT_ERROR 还坏」处理。
pub fn worse(a: i32, b: i32) -> i32 {
    fn rank(code: i32) -> u8 {
        match code {
            EXIT_OK => 0,
            EXIT_CONFIRM_DENIED => 1,
            EXIT_PARTIAL => 2,
            EXIT_USAGE => 3,
            EXIT_LOCKED => 4,
            EXIT_ERROR => 5,
            _ => 6, // 未知码比任何已知码都坏
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

// ---------------------------------------------------------------------------
// 输出模式
// ---------------------------------------------------------------------------

/// 输出模式：由全局 `--json` 旗标决定，命令内据此二选一，不做混合输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// 人类模式：stdout 表格/摘要，stderr 进度。
    Human,
    /// 机器模式：stdout 一行紧凑 JSON 信封。
    Json,
}

/// 把 `--json` 旗标翻译成 [`OutputMode`]，无状态小胶水。
pub fn is_json_mode(flag: bool) -> OutputMode {
    if flag {
        OutputMode::Json
    } else {
        OutputMode::Human
    }
}

// ---------------------------------------------------------------------------
// JSON 信封
// ---------------------------------------------------------------------------

/// 信封里的错误对象：`code` 机器可读短码，`message` 人话。
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

/// 统一信封。字段顺序即输出顺序（serde_json 开了 preserve_order）。
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    ok: bool,
    command: &'a str,
    data: Option<&'a T>,
    warnings: &'a [String],
    error: Option<ErrorBody<'a>>,
}

/// 渲染成功信封为一行紧凑 JSON（不含换行）。
fn render_json<T: Serialize>(command: &str, data: &T, warnings: &[String]) -> String {
    serde_json::to_string(&Envelope {
        ok: true,
        command,
        data: Some(data),
        warnings,
        error: None,
    })
    .expect("信封序列化不应失败：data 由命令自身定义且不含非法值")
}

/// 渲染错误信封为一行紧凑 JSON（不含换行）。
fn render_json_error(command: &str, code: &str, message: &str) -> String {
    serde_json::to_string(&Envelope::<'_, ()> {
        ok: false,
        command,
        data: None,
        warnings: &[],
        error: Some(ErrorBody { code, message }),
    })
    .expect("错误信封序列化不应失败")
}

/// 成功输出：把命令私有数据装进统一信封，写 stdout 一行紧凑 JSON。
pub fn emit_json<T: Serialize>(command: &str, data: &T, warnings: &[String]) {
    println!("{}", render_json(command, data, warnings));
}

/// 错误输出：信封写 stdout（机器读），同一条错误的人话同时写 stderr（人读）。
pub fn emit_json_error(command: &str, code: &str, message: &str) {
    println!("{}", render_json_error(command, code, message));
    let _ = writeln!(std::io::stderr(), "duster {command}: [{code}] {message}");
}

// ---------------------------------------------------------------------------
// 配色
// ---------------------------------------------------------------------------
//
// 全部走 console 的 `Style`：它自带 tty 判定与 `NO_COLOR` / `CLICOLOR_FORCE`
// 语义，管道/重定向时自动退化成纯文本，调用方不必到处判断。
// 只用前景色与 bold/dim 两个属性——背景色在浅色主题下会瞎眼。

/// 主色：命令名、agent 名、区块标题。
pub fn accent() -> Style {
    Style::new().cyan()
}

/// 次要信息：分隔线、单位、提示语。
pub fn muted() -> Style {
    Style::new().dim()
}

/// 成功前缀 `✔`（已着色）。
pub fn ok_mark() -> StyledObject<&'static str> {
    Style::new().green().bold().apply_to("✔")
}

/// 警告前缀 `!`（已着色）。
pub fn warn_mark() -> StyledObject<&'static str> {
    Style::new().yellow().bold().apply_to("!")
}

/// 错误前缀 `✖`（已着色）。
pub fn err_mark() -> StyledObject<&'static str> {
    Style::new().red().bold().apply_to("✖")
}

// ---------------------------------------------------------------------------
// 表格
// ---------------------------------------------------------------------------

/// 字符串的终端显示宽度：东亚宽字符按 2，ANSI 转义序列按 0。
///
/// 直接复用 console 的实现（unicode-width + ANSI 解析），不再手搓码点区间表。
pub fn display_width(s: &str) -> usize {
    console::measure_text_width(s)
}

/// 按显示宽度截断，超长时尾部替换成 `…`（本身占 1 列）。
///
/// 结果宽度不超过 `max`——`max == 0` 时返回空串（省略号本身宽 1，放了
/// 就顶破预算）。
/// 只在纯文本上调用——先截断、后着色，别反过来，否则会剪断 ANSI 序列。
pub fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        // 0 列连省略号都放不下:省略号本身宽 1,放了就把「不越过预算」
        // 的不变量顶破一列。空串让这一列只剩列间空隙。
        return String::new();
    }
    let budget = max.saturating_sub(1); // 给省略号留一列
    let mut out = String::with_capacity(budget + 3);
    let mut used = 0usize;
    for c in s.chars() {
        let w = display_width(c.encode_utf8(&mut [0u8; 4]));
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// 人类模式的列表格：列宽自适应，CJK 按显示宽度 2 对齐。
///
/// 渲染成「表头一行 + 一条 `─` 横线 + 数据行若干」，整体缩进两格，
/// 列间两个空格，行尾不留补齐空格。同一个 Table 还能喂给交互控件
/// （[`Table::rows`]）——两个出口共用同一套宽度算法，不再让 interactive
/// 的手写 row builder 各算各的账。
///
/// 单元格一律存**无样式**的纯文本，颜色在 render 时按列附加——宽度计算
/// 因此永远看不见 ANSI 字节，对齐不会被着色带偏。
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// 该列是否右对齐（数字列用）。
    right: Vec<bool>,
    /// 该列数据行的前景色；表头恒为 bold。
    color: Vec<Option<Style>>,
    /// flex 列的列号：它在 stdout 是 TTY 时吃掉终端余量（见 [`Table::flex_col`]）。
    ///
    /// **没有「最小宽度」这个旋钮,余量就是硬上限。** 早先有过一个 `flex_min`
    /// （调用方给 16/8/4/4 的可读性下限），实现是 `flex_w.min(avail.max(floor))`
    /// ——余量比地板窄时按地板截，**整行越过预算**。那不是取舍,是把承重不变量
    /// 交给了一个配置旋钮:行宽碰到终端宽就折成两个物理行,而交互控件按逻辑行
    /// 计数、按物理行擦,折一行残影每按一次翻一倍(正是 [`Prefix`] 那段注释
    /// 记着的病史)。
    ///
    /// 而地板一旦夹回余量内就恒等于无操作——单 flex 列 + 固定其余列的布局里,
    /// 想真正满足地板只能去压缩别的列,那是另一套算法,这张表从来没有过。
    /// 所以不留它:余量不够时 flex 列截到认不出内容,也好过整屏残影。
    flex: Option<usize>,
}

/// 交互控件画在每行左边的前缀。行宽预算必须把它算进去——交互区按逻辑行
/// 计数、按物理行擦，行宽碰到终端宽就折行，残影每按一次翻一倍，所以前缀
/// 宽度由每个变体自己给，不许调用方手数（interactive.rs 旧版把 `❯ [x] `
/// 按 4 列算，少的 2 列全进了路径列，行宽恰好 = 终端宽 + 1，每一行都折，
/// 这就是交互式 clean 残影翻倍的根源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    /// 纯表格，无控件前缀，预算不扣。
    ///
    /// 生产代码不构造它——命令行表格走 [`Table::render`]，交互控件走
    /// `Cursor` / `Checkbox`。它存在是为了给「两个出口同一真相」那条测试
    /// 一个零前缀的对照点：没有它，`rows_at` 与 `render_at` 就没法在同一
    /// 个宽度口径下逐字比对。
    #[allow(dead_code)]
    None,
    /// prompt_pick 的 `❯ `：`❯` 1 列 + 空格 1 列 = 2 列。
    Cursor,
    /// prompt_checklist 的 `❯ [x] `：`❯` 1 + 空格 1 + `[x]` 3 + 空格 1 = 6 列。
    Checkbox,
}

impl Prefix {
    /// 前缀占用的显示列数。宽度是这套行宽预算里唯一的承重点，数值钉死在
    /// 这里（变体注释里逐项算过账），测试逐值断言。
    pub fn width(self) -> usize {
        match self {
            Prefix::None => 0,
            Prefix::Cursor => 2,
            Prefix::Checkbox => 6,
        }
    }
}

/// 表格缩进：给终端留出呼吸感，也把表格和摘要行区分开。
const INDENT: &str = "  ";

/// 空计数占位符，渲染时自动置灰。
const DASH: &str = "-";

impl Table {
    /// 以表头建表。列数由表头决定，行短于表头的列按空串补齐。
    pub fn new<S: Into<String>>(header: Vec<S>) -> Self {
        let header: Vec<String> = header.into_iter().map(Into::into).collect();
        let cols = header.len();
        Self {
            header,
            rows: Vec::new(),
            right: vec![false; cols],
            color: vec![None; cols],
            flex: None,
        }
    }

    /// 把这些列改成右对齐。越界下标忽略。
    pub fn right_align(&mut self, cols: &[usize]) {
        for &c in cols {
            if let Some(flag) = self.right.get_mut(c) {
                *flag = true;
            }
        }
    }

    /// 给某列的数据行上色。越界下标忽略。
    pub fn color_col(&mut self, col: usize, style: Style) {
        if let Some(slot) = self.color.get_mut(col) {
            *slot = Some(style);
        }
    }

    /// 指定一列吃终端余量：其余列按内容取宽，这一列截到剩下的宽度。
    ///
    /// 只在 stdout 是 TTY 时生效——重定向到文件时截掉路径会毁掉逐行核对，
    /// 而「人类模式的表格能完整落进文件」是文档化契约。越界下标忽略。
    /// 余量不够时这一列会被截到很窄甚至只剩省略号——不给「最小宽度」这种
    /// 旋钮,理由见 [`Table`] 的 `flex` 字段注释。
    pub fn flex_col(&mut self, col: usize) {
        self.flex = Some(col);
    }

    /// 追加一行。多出表头的列会被忽略。
    pub fn push_row<S: Into<String>>(&mut self, row: Vec<S>) {
        let mut cells: Vec<String> = row.into_iter().map(Into::into).collect();
        cells.truncate(self.header.len());
        self.rows.push(cells);
    }

    /// 是否一行数据都没有。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 渲染为多行字符串（最后一行无多余换行）。
    pub fn render(&self) -> String {
        self.render_at(stdout_term_cols())
    }

    /// 按给定终端宽度渲染。`None` = 拿不到宽度（管道 / 非 TTY），flex 列
    /// 不收缩，表格维持旧的全宽行为。
    fn render_at(&self, total: Option<usize>) -> String {
        let widths = self.widths_for(total, display_width(INDENT));
        let bold = Style::new().bold();
        let dim = muted();

        let mut out = String::new();
        let head_styles: Vec<Option<&Style>> = vec![Some(&bold); widths.len()];
        let row_styles: Vec<Option<&Style>> = self.color.iter().map(Option::as_ref).collect();

        self.render_line(&mut out, &self.header, &widths, &head_styles, INDENT, false);
        // 一条贯穿的横线：总宽 = 各列宽之和 + 列间两空格。
        let rule: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
        out.push('\n');
        out.push_str(INDENT);
        out.push_str(&dim.apply_to("─".repeat(rule)).to_string());
        for row in &self.rows {
            out.push('\n');
            self.render_line(&mut out, row, &widths, &row_styles, INDENT, false);
        }
        out
    }

    /// 各列取内容（含表头）的最大显示宽度。
    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.header.iter().map(|h| display_width(h)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
        widths
    }

    /// 渲染列宽：拿到终端宽度且设了 flex 列时，把它截到余量（可能为 0，
    /// 这时该列渲染成省略号），其余列保持内容宽；拿不到宽度就原样返回。
    ///
    /// `indent` 同 [`flex_avail`]：命令行表格 2（[`INDENT`]），交互行 0
    /// （前缀已经算进预算了）。**余量是上限,没有任何东西能顶破它**——整行
    /// 不越过预算是防残影的承重不变量,见 [`Table`] 的 `flex` 字段注释。
    ///
    /// flex 列截到 0 之后固定列仍可能合计超预算（窄终端 + 宽内容,如 60 列
    /// 下的会话表:ID/AGENT/PROJECT/TURNS/SIZE 自己就把预算吃穿),这时从
    /// 最宽的列开始逐列削,地板是各自表头宽——表头削了列就没名字,而任何
    /// 一列都不该为别列的内容让出自己的名字。全列到地板还不够就只能超,
    /// 但那是终端窄过表头之和的极端,正常 60 列起步到不了。
    fn widths_for(&self, total: Option<usize>, indent: usize) -> Vec<usize> {
        let widths = self.widths();
        let (Some(flex), Some(total)) = (self.flex, total) else {
            return widths;
        };
        let Some(flex_w) = widths.get(flex).copied() else {
            return widths; // 越界下标当没设过。
        };
        let mut widths = widths;
        widths[flex] = flex_w.min(flex_avail(&widths, flex, total, indent));
        // 固定列超预算的兜底:削最宽列到表头宽为止。
        let floors: Vec<usize> = self.header.iter().map(|h| display_width(h)).collect();
        let gaps = 2 * widths.len().saturating_sub(1);
        let budget = total.saturating_sub(indent + gaps);
        while widths.iter().sum::<usize>() > budget {
            let Some((i, _)) = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| **w > floors[*i])
                .max_by_key(|(_, w)| **w)
            else {
                break; // 全列到地板,预算实在装不下。
            };
            widths[i] -= 1;
        }
        widths
    }

    /// 渲染单行：先按纯文本算补白，再给单元格套色，行尾不补空格。
    ///
    /// `indent` 由调用方给（命令行表格 [`INDENT`]，交互行 `""`）；`plain`
    /// 为 true 时整个行不着色——交互控件的选中态自己上色，表格再上色会
    /// 打架，占位横杠也一样（见 [`Table::rows_at`]）。
    fn render_line(
        &self,
        out: &mut String,
        cells: &[String],
        widths: &[usize],
        styles: &[Option<&Style>],
        indent: &str,
        plain: bool,
    ) {
        let dim = muted();
        let last = widths.len().saturating_sub(1);
        out.push_str(indent);
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let raw = cells.get(i).map(String::as_str).unwrap_or("");
            // 任何列都按最终列宽截断——列宽平时就是内容宽,截了也是原样;
            // 只有 flex 收缩或固定列被削(widths_for 的预算兜底)时才真会截。
            // 先截断后着色,否则会剪断 ANSI 序列。
            let truncated;
            let cell = if display_width(raw) > *width {
                truncated = truncate_width(raw, *width);
                truncated.as_str()
            } else {
                raw
            };
            let pad = width.saturating_sub(display_width(cell));
            // 占位横杠一律置灰，让真实数字自己跳出来；plain 模式整个行不着色。
            let style = if plain {
                None
            } else if cell == DASH {
                Some(&dim)
            } else {
                styles.get(i).copied().flatten()
            };
            let right = self.right.get(i).copied().unwrap_or(false);
            if right {
                out.extend(std::iter::repeat_n(' ', pad));
            }
            match style {
                Some(s) => out.push_str(&s.apply_to(cell).to_string()),
                None => out.push_str(cell),
            }
            // 行尾不留补齐空格。
            if !right && i < last {
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
    }

    /// 渲染成 `(表头行, 数据行)`，喂给交互控件（菜单、勾选表）。`cols` 是
    /// 终端列数，由调用方给——交互区的菜单画在 **stderr** 上，宽度必须从那里
    /// 量（`interactive::term_width`），而 [`Table::render`] 量的是 stdout；
    /// 两者在管道下会分道扬镳，所以这里不提供「自己去问终端」的重载，免得
    /// 有人顺手用错那一头。测试直接喂 60 / 80 / 200，也不必 mock 终端。
    ///
    /// 与 [`Table::render`] 的差异只有三点，其余口径（列宽、对齐、截断）
    /// 逐字一致，保证同一份数据两个出口渲染出同一张表：
    /// - 不加 [`INDENT`] 两格缩进——控件自己画前缀（[`Prefix`]），行宽
    ///   预算里已经给它留了位置；
    /// - 不画 `─` 横线；
    /// - 不着色——选中态由控件自己上色，表格再上色会打架，占位横杠也一样。
    ///
    /// 行宽预算 = `cols` - `prefix.width()` - 1：末尾 1 列是余量，行宽碰到
    /// 终端宽就会被折成两个物理行，而控件按逻辑行计数、按物理行擦，折一行
    /// 残影每按一次翻一倍。
    pub(crate) fn rows_at(&self, prefix: Prefix, cols: usize) -> (String, Vec<String>) {
        let budget = cols.saturating_sub(prefix.width() + 1);
        let widths = self.widths_for(Some(budget), 0);
        let mut header = String::new();
        self.render_line(&mut header, &self.header, &widths, &[], "", true);
        let body = self
            .rows
            .iter()
            .map(|row| {
                let mut line = String::new();
                self.render_line(&mut line, row, &widths, &[], "", true);
                line
            })
            .collect();
        (header, body)
    }
}

/// flex 列能拿到的宽度：总预算 - 行首缩进 - 其余列的内容宽 - 列间空隙。
///
/// `indent` 由调用方按出口给：命令行表格是 [`INDENT`]（2 列），交互行是 0
/// （前缀已经算进预算了）。可能为 0（预算比固定列还窄），这时 flex 列渲染
/// 成省略号而不是撑破屏幕；只要余量 ≥ 1，整行就不会越过预算——命令行表格
/// 的「横线 + 缩进 ≤ 终端宽」与交互行的「行宽 + 前缀 < 终端宽」都靠这条
/// 成立。
fn flex_avail(widths: &[usize], flex: usize, total: usize, indent: usize) -> usize {
    let other: usize = widths
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != flex)
        .map(|(_, w)| *w)
        .sum();
    let gaps = 2 * widths.len().saturating_sub(1);
    total.saturating_sub(indent + other + gaps)
}

/// stdout 的终端列数；非 TTY 或取不到尺寸时返回 None。
///
/// 管道 / 重定向时 flex 列不收缩——表格完整落进文件是输出契约的一部分，
/// 截路径只发生在真的有人在看屏幕的时候。
fn stdout_term_cols() -> Option<usize> {
    let term = console::Term::stdout();
    if !term.is_term() {
        return None;
    }
    term.size_checked().map(|(_, cols)| cols as usize)
}

// ---------------------------------------------------------------------------
// 数值格式化
// ---------------------------------------------------------------------------

/// 1024 进制的人类可读字节数，如 `0 B` / `1023 B` / `1.5 GB`。
///
/// 单位以上保留一位小数，`.0` 会被去掉（`1024` → `1 KB`）。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let s = format!("{value:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s} {}", UNITS[unit])
}

/// 毫秒 → 人话时长：不足 1 秒说 `820 ms`，否则说 `1.3 s`。
pub fn human_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        let s = format!("{:.1}", ms as f64 / 1000.0);
        format!("{} s", s.strip_suffix(".0").unwrap_or(&s))
    }
}

// ---------------------------------------------------------------------------
// 测试：只守护可观察契约（信封 schema、CJK 对齐、字节边界）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn json_envelope_schema() {
        #[derive(Serialize)]
        struct Data {
            count: u32,
        }
        let line = render_json("scan", &Data { count: 3 }, &["跳过 foo".to_string()]);
        assert!(!line.contains('\n'), "信封必须是一行紧凑 JSON");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], Value::Bool(true));
        assert_eq!(v["command"], "scan");
        assert_eq!(v["data"]["count"], 3);
        assert_eq!(v["warnings"][0], "跳过 foo");
        assert!(v["error"].is_null());
        // 字段顺序即契约顺序（preserve_order）。
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["ok", "command", "data", "warnings", "error"]);
    }

    #[test]
    fn json_error_envelope_schema() {
        let line = render_json_error("clean", "locked", "索引被占用");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], Value::Bool(false));
        assert_eq!(v["command"], "clean");
        assert!(v["data"].is_null());
        assert_eq!(v["warnings"].as_array().unwrap().len(), 0);
        assert_eq!(v["error"]["code"], "locked");
        assert_eq!(v["error"]["message"], "索引被占用");
    }

    #[test]
    fn table_cjk_alignment() {
        let mut t = Table::new(vec!["名称", "size"]);
        t.push_row(vec!["中文技能包", "12"]);
        t.push_row(vec!["ascii", "3456"]);
        let rendered = t.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4);
        // 每行第二列起始处的显示列必须一致（中文列不错位）。
        let col_starts: Vec<usize> = [(lines[0], "size"), (lines[2], "12"), (lines[3], "3456")]
            .iter()
            .map(|(line, needle)| {
                let idx = line.rfind(needle).unwrap();
                display_width(&line[..idx])
            })
            .collect();
        assert!(
            col_starts.windows(2).all(|w| w[0] == w[1]),
            "第二列起始显示列不一致: {col_starts:?}\n{rendered}"
        );
        // 行尾不留补齐空格。
        assert!(rendered.lines().all(|l| !l.ends_with(' ')));
    }

    /// 右对齐列：数字末位对齐到列右缘，且行尾不留补齐空格。
    #[test]
    fn table_right_align_numbers() {
        let mut t = Table::new(vec!["agent", "size"]);
        t.right_align(&[1]);
        t.push_row(vec!["a", "7"]);
        t.push_row(vec!["bbbb", "1024"]);
        let rendered = t.render();
        let lines: Vec<&str> = rendered.lines().collect();
        // 末列右对齐 ⇒ 每行显示宽度相同（都顶到列右缘）。
        let widths: Vec<usize> = [lines[0], lines[2], lines[3]]
            .iter()
            .map(|l| display_width(l))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "右对齐后各行宽度应一致: {widths:?}\n{rendered}"
        );
        assert!(lines[3].ends_with("1024"));
        assert!(rendered.lines().all(|l| !l.ends_with(' ')));
    }

    #[test]
    fn human_bytes_boundaries() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1_610_612_736), "1.5 GB"); // 1.5 GiB
    }

    #[test]
    fn human_ms_boundaries() {
        assert_eq!(human_ms(0), "0 ms");
        assert_eq!(human_ms(999), "999 ms");
        assert_eq!(human_ms(1000), "1 s");
        assert_eq!(human_ms(1349), "1.3 s");
    }

    /// 截断按显示宽度算：宽字符不劈开，结果永不超过上限。
    #[test]
    fn truncate_width_不超上限且不劈开宽字符() {
        assert_eq!(truncate_width("short", 10), "short");
        assert_eq!(truncate_width("abcdefghij", 5), "abcd…");
        // 中文各占 2 列：上限 5 只放得下 2 个字 + 省略号。
        let cut = truncate_width("一二三四五", 5);
        assert_eq!(cut, "一二…");
        assert!(display_width(&cut) <= 5);
        // 边界：上限 1 只剩省略号，上限 0 直接空。
        assert_eq!(truncate_width("一二", 1), "…");
        assert_eq!(truncate_width("一二", 0), "");
    }

    /// flex 列在窄终端下：最后一列被截断、横线不超出给定宽度。这是「表格
    /// 不溢出终端」那条契约的测试形态——render_at 收到的宽度就是终端宽度。
    #[test]
    fn flex_列在窄宽下截断且横线不超宽() {
        let path = "~/.codex/skills/a-very-long-skill-name-that-would-overflow-any-narrow-terminal";
        let mut t = Table::new(vec!["AGENT", "SIZE", "PATH"]);
        t.right_align(&[1]);
        t.push_row(vec!["codex", "1.2 GB", path]);
        t.flex_col(2);

        let rendered = t.render_at(Some(40));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        // 横线那行含缩进也不超过 40 列。
        assert!(
            display_width(lines[1]) <= 40,
            "横线超宽 {}: {lines:?}",
            display_width(lines[1])
        );
        // 路径单元格被截断：原文不在，尾部是省略号。
        let path_line = lines[2];
        assert!(path_line.contains('…'), "路径应被截断: {path_line}");
        assert!(!path_line.contains("would-overflow"), "{path_line}");
        // 整行（含缩进）同样不超过 40 列。
        assert!(display_width(path_line) <= 40, "{path_line}");
    }

    /// 不设 flex 列时表格逐字不变（含给一个任意宽度）：守卫所有现存的列表格，
    /// 免得 flex 的宽度账把哪一列挤掉。
    #[test]
    fn 无_flex_列时表格与旧行为逐字一致() {
        let mut t = Table::new(vec!["AGENT", "SIZE"]);
        t.right_align(&[1]);
        t.push_row(vec!["codex", "1.2 GB"]);
        t.push_row(vec!["gemini-cli", "3.1 MB"]);
        let rendered = t.render_at(None);
        // 没有 flex 列时,给不给宽度结果一模一样——flex 是唯一会动宽度的东西。
        assert_eq!(rendered, t.render_at(Some(40)));
        // 旧形状:表头 + 横线 + 数据行,行尾无补齐空格。
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[1].starts_with("  ─"));
        assert!(rendered.lines().all(|l| !l.ends_with(' ')));
        // 越界 flex 下标当没设过,表格原样。
        let mut u = Table::new(vec!["A", "B"]);
        u.push_row(vec!["x", "y"]);
        u.flex_col(7);
        assert_eq!(u.render_at(Some(10)), u.render_at(None));
    }

    #[test]
    fn output_mode_glue() {
        assert_eq!(is_json_mode(true), OutputMode::Json);
        assert_eq!(is_json_mode(false), OutputMode::Human);
    }

    /// 退出码是脚本可见契约：数值一旦漂移，`|| echo nothing-was-done` 这类
    /// 判断会静默变成别的意思。六个码钉死在这里。
    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(
            [
                EXIT_OK,
                EXIT_ERROR,
                EXIT_USAGE,
                EXIT_PARTIAL,
                EXIT_CONFIRM_DENIED,
                EXIT_LOCKED,
            ],
            [0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn worse_partial_比_confirm_denied_更坏() {
        // `max` 会在这里算错(3 < 4),但语义上 partial 比 confirm denied 坏。
        assert_eq!(worse(EXIT_PARTIAL, EXIT_CONFIRM_DENIED), EXIT_PARTIAL);
    }

    #[test]
    fn worse_ok_永远让位() {
        let codes = [
            EXIT_OK,
            EXIT_ERROR,
            EXIT_LOCKED,
            EXIT_USAGE,
            EXIT_PARTIAL,
            EXIT_CONFIRM_DENIED,
        ];
        for &c in &codes {
            assert_eq!(worse(EXIT_OK, c), c, "EXIT_OK 不应覆盖任何非零码");
        }
    }

    #[test]
    fn worse_未知码最坏() {
        assert_eq!(worse(EXIT_ERROR, 99), 99);
        assert_eq!(worse(EXIT_OK, 99), 99);
    }

    #[test]
    fn worse_可交换() {
        let codes = [
            EXIT_OK,
            EXIT_ERROR,
            EXIT_LOCKED,
            EXIT_USAGE,
            EXIT_PARTIAL,
            EXIT_CONFIRM_DENIED,
        ];
        for a in &codes {
            for b in &codes {
                assert_eq!(
                    worse(*a, *b),
                    worse(*b, *a),
                    "worse 必须可交换: a={a}, b={b}"
                );
            }
        }
    }

    /// 前缀宽度是行宽预算里唯一的承重点,数值由变体自己给——interactive 的
    /// 旧 row builder 手数错过一次(把 6 算成 4)就让每行宽 = 终端宽 + 1,
    /// 交互式 clean 残影翻倍。三个值逐字钉死。
    #[test]
    fn prefix_width_数值钉死() {
        assert_eq!(Prefix::None.width(), 0);
        assert_eq!(Prefix::Cursor.width(), 2); // `❯` 1 + 空格 1
        assert_eq!(Prefix::Checkbox.width(), 6); // `❯` 1 + 空格 1 + `[x]` 3 + 空格 1
    }

    /// 防残影核心不变量:rows_at 的每一行(含表头)加上前缀后必须严格小于
    /// 终端宽——行宽碰到终端宽就被折成两个物理行,而控件按逻辑行计数、按
    /// 物理行擦,折一行残影每按一次翻一倍。60 / 80 / 200 三种宽度 × 三种
    /// 前缀,逐行断言,不是抽查。用超长路径逼 flex 列截断,让它真的吃满
    /// 预算(只截到预算内的情形断言不了什么)。
    #[test]
    fn rows_at_每行加前缀严格小于终端宽() {
        let long = "~/.cache/codex/sessions/2026-08-13/1e2d3c4b5a-this-path-is-long-enough-to-overflow-any-narrow-terminal";
        for cols in [60usize, 80, 200] {
            for prefix in [Prefix::None, Prefix::Cursor, Prefix::Checkbox] {
                let mut t = Table::new(vec!["AGENT", "SIZE", "PATH"]);
                t.right_align(&[1]);
                t.push_row(vec!["codex", "1.2 GB", long]);
                t.push_row(vec!["gemini-cli", "3.1 MB", "~/.gemini"]);
                t.flex_col(2);

                let (header, rows) = t.rows_at(prefix, cols);
                let all = std::iter::once(&header).chain(rows.iter());
                for line in all {
                    let w = prefix.width() + display_width(line);
                    assert!(
                        w < cols,
                        "{cols} 列终端、前缀 {prefix:?}:行宽 {w} 超限: {line}"
                    );
                }
            }
        }
    }

    /// 两个出口不许是两个真相:同一张表,`rows_at(Prefix::None, T-1)` 的表头
    /// 与数据行,在剥掉 render 的缩进 / 横线 / 颜色之后与 `render_at(Some(T))`
    /// 逐字一致。
    ///
    /// 为什么是 `T-1`:render 留 2 列缩进、rows 留前缀 + 1 列余量,预算口径
    /// 差 1 列,所以两者的 flex 余量恰好相等。两段都走 `_at`,不碰真实终端
    /// ——`rows()` 与 `render()` 的宽度兜底本就不同(前者 100、后者不收缩),
    /// 拿它们对齐只在内容短到不触顶时碰巧成立,那不是契约。
    #[test]
    fn rows_与_render_同一真相() {
        // 内容不触顶:两个出口直接对齐。
        let mut t = Table::new(vec!["AGENT", "SIZE", "PATH"]);
        t.right_align(&[1]);
        t.push_row(vec!["codex", "1.2 GB", "~/.codex"]);
        t.push_row(vec!["gemini-cli", "3.1 MB", "~/.gemini"]);
        t.flex_col(2);

        let (h1, r1) = t.rows_at(Prefix::None, 79);
        let rendered = t.render_at(Some(80));
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4, "表头 + 横线 + 两行数据");
        let strip = |l: &str| {
            console::strip_ansi_codes(l.strip_prefix(INDENT).unwrap_or(l)).to_string()
        };
        assert_eq!(strip(lines[0]), h1, "表头不一致");
        let rows_expected: Vec<String> = lines[2..].iter().map(|l| strip(l)).collect();
        assert_eq!(r1, rows_expected, "数据行不一致");

        // 截断路径:长路径把 flex 列真的截短。
        let mut u = Table::new(vec!["AGENT", "SIZE", "PATH"]);
        u.right_align(&[1]);
        u.push_row(vec![
            "codex",
            "1.2 GB",
            "~/.codex/skills/a-very-long-skill-name-that-overflows",
        ]);
        u.push_row(vec!["gemini-cli", "3.1 MB", "~/.gemini"]);
        u.flex_col(2);

        let (h2, r2) = u.rows_at(Prefix::None, 39);
        let rendered2 = u.render_at(Some(40));
        let lines2: Vec<&str> = rendered2.lines().collect();
        assert_eq!(strip(lines2[0]), h2, "截断路径表头不一致");
        let rows2_expected: Vec<String> = lines2[2..].iter().map(|l| strip(l)).collect();
        assert_eq!(r2, rows2_expected, "截断路径数据行不一致");
        assert!(r2[0].contains('…'), "长路径应被截断: {}", r2[0]);
    }

    /// CJK 场景:宽字符按显示宽度 2 对齐,rows 的第二列不能顶歪;行宽加前缀
    /// 同样不越终端。
    #[test]
    fn rows_cjk_宽字符不顶歪下一列() {
        let mut t = Table::new(vec!["名称", "size"]);
        t.push_row(vec!["中文技能包", "12"]);
        t.push_row(vec!["ascii", "3456"]);
        t.flex_col(1);

        let (header, rows) = t.rows_at(Prefix::Checkbox, 60);
        let all = std::iter::once(&header).chain(rows.iter());
        // 每行第二列起始处的显示列必须一致(中文列不错位)。
        let col_starts: Vec<usize> = [
            (&header, "size"),
            (&rows[0], "12"),
            (&rows[1], "3456"),
        ]
        .iter()
        .map(|(line, needle)| {
            let idx = line.rfind(needle).unwrap();
            display_width(&line[..idx])
        })
        .collect();
        assert!(
            col_starts.windows(2).all(|w| w[0] == w[1]),
            "第二列起始显示列不一致: {col_starts:?}"
        );
        // 行宽 + 前缀不越终端。
        for line in all {
            let w = Prefix::Checkbox.width() + display_width(line);
            assert!(w < 60, "行宽 {w} 超限: {line}");
        }
    }

    /// 余量比内容窄时 flex 列被截,而**整行绝不越过预算**——这条不变量不许
    /// 任何配置旋钮顶破(旧版的 flex_min 地板就会,那是被删掉的原因)。
    /// 内容本身比余量短时不强制补白,行保持内容宽。
    #[test]
    fn flex列按余量截断且整行不越预算() {
        let long = "~/.codex/skills/a-very-long-skill-name-that-overflows-any-terminal";
        let mut t = Table::new(vec!["AGENT", "SIZE", "PATH"]);
        t.right_align(&[1]);
        t.push_row(vec!["codex", "1.2 GB", long]);
        t.push_row(vec!["gemini-cli", "3.1 MB", "~/.gemini"]);
        t.flex_col(2);

        // 窄到 36 列:路径按余量截,整行严格窄于终端宽(留 1 列余量不折行)。
        let (header, rows) = t.rows_at(Prefix::None, 36);
        for line in std::iter::once(&header).chain(rows.iter()) {
            assert!(
                display_width(line) < 36,
                "行宽 {} 越过预算: {line}",
                display_width(line)
            );
        }
        assert!(rows[0].contains('…'), "长路径应被截断: {}", rows[0]);
        // 短内容:不强制补白,行尾就是内容本身。
        assert!(rows[1].ends_with("~/.gemini"), "{}", rows[1]);

        // 极窄终端:flex 列压到 0 也不许越界,退化成省略号而不是撑破屏幕。
        let (_, tiny) = t.rows_at(Prefix::Checkbox, 30);
        for line in &tiny {
            let w = Prefix::Checkbox.width() + display_width(line);
            assert!(w < 30, "行宽 {w} 越过预算: {line}");
        }
    }
}
