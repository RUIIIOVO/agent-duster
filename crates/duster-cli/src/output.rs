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
//! | 4 | [`EXIT_CONFIRM_DENIED`] | 需要确认被拒绝 / dry-run 仅预览未执行（留给 clean） |
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
//! - `command`：子命令名（`scan` / `status` / `clean` ...）。
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
/// 用法错误（clap 解析失败时的默认退出码，列在这里只为文档完整）。
#[allow(dead_code)] // clap 自行返回 2,代码里不引用;保留为契约文档。
pub const EXIT_USAGE: i32 = 2;
/// 部分成功：整体流程走完，但个别子项失败（细节写进 warnings）。
pub const EXIT_PARTIAL: i32 = 3;
/// 需要确认被拒绝 / dry-run 仅预览未执行。
#[allow(dead_code)] // 留给 M1 的 `duster clean`,先占住语义。
pub const EXIT_CONFIRM_DENIED: i32 = 4;
/// 锁冲突：目标 agent 运行中或索引被其他进程占用。
pub const EXIT_LOCKED: i32 = 5;

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
/// 结果宽度不超过 `max.max(1)`——`max == 0` 是调用方笔误，退化为只剩省略号。
/// 只在纯文本上调用——先截断、后着色，别反过来，否则会剪断 ANSI 序列。
pub fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
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
/// 列间两个空格，行尾不留补齐空格。
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
        let widths = self.widths();
        let bold = Style::new().bold();
        let dim = muted();

        let mut out = String::new();
        let head_styles: Vec<Option<&Style>> = vec![Some(&bold); widths.len()];
        let row_styles: Vec<Option<&Style>> = self.color.iter().map(Option::as_ref).collect();

        self.render_line(&mut out, &self.header, &widths, &head_styles);
        // 一条贯穿的横线：总宽 = 各列宽之和 + 列间两空格。
        let rule: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
        out.push('\n');
        out.push_str(INDENT);
        out.push_str(&dim.apply_to("─".repeat(rule)).to_string());
        for row in &self.rows {
            out.push('\n');
            self.render_line(&mut out, row, &widths, &row_styles);
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

    /// 渲染单行：先按纯文本算补白，再给单元格套色，行尾不补空格。
    fn render_line(
        &self,
        out: &mut String,
        cells: &[String],
        widths: &[usize],
        styles: &[Option<&Style>],
    ) {
        let dim = muted();
        let last = widths.len().saturating_sub(1);
        out.push_str(INDENT);
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let pad = width.saturating_sub(display_width(cell));
            // 占位横杠一律置灰，让真实数字自己跳出来。
            let style = if cell == DASH {
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
        assert_eq!(truncate_width("一二", 0), "…");
    }

    #[test]
    fn output_mode_glue() {
        assert_eq!(is_json_mode(true), OutputMode::Json);
        assert_eq!(is_json_mode(false), OutputMode::Human);
    }
}
