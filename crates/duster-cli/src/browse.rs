//! 分页浏览一批已经渲染好的行(交互菜单的「逐条下钻」原语)。
//!
//! 列表类命令在菜单里的老结局是死胡同:整张表刷完,用户得手抄一个 key
//! 再去敲第二条命令。`browse` 把同一批行交给 [`prompt_pick`]——终端多高就
//! 显示几行,超出部分按页翻(←/→,末行报「第几到第几 / 共几行」),
//! Enter 带走选中下标,Esc 原样退回。
//!
//! # 为什么不是 dialoguer 的 `Select`
//!
//! 它会翻页,但页高由它自己从终端行数重算(`paging.rs`:
//! `max_length.clamp(3, rows) - 2`),**调用方在它上面已经印了几行它并不
//! 知道**:表头、操作提示、问句这三行不算在内,于是 20 行的表在 24 行的
//! 终端里正好顶出屏幕,滚上去的部分还带着重绘残影。而且 `pages > 1` 才认
//! ←/→ ——一页装得下时那两个键静静地什么都不做,提示行却照旧写着
//! 「←/→ page」。页高只能由知道自己印了几行的人来定,所以整屏由
//! [`prompt_pick`] 一个函数画、一个函数擦。
//!
//! # 行的契约
//!
//! 行文本由调用方**预先对齐**:列宽按 [`display_width`] 算好、宽字符不会
//! 顶歪下一列(与 `interactive.rs` 的 `checklist_rows` 同一套量法)。原语
//! 不认识列结构,也绝不自己写死列宽——塞进来的行是什么样,屏幕就是什么样。
//!
//! # 非 TTY 直接 None
//!
//! 菜单只在 TTY 下存在,这里拿不到终端就说明调用方跑错了地方;返回 None
//! 让调用方照旧打印完整表格——浏览是 TTY 上的增益,不是新契约。
//!
//! [`display_width`]: crate::output::display_width

use std::io::IsTerminal;

use console::Term;
use dialoguer::theme::{ColorfulTheme, Theme};

use crate::prompt::{Picker, prompt_pick};

/// 交给 [`prompt_pick`] 的可用行数要从终端行数里让出几行:控件自己画问句
/// 1 行、多于一页时画页脚 1 行,再留 1 行余量。表头那一行由控件内部再扣
/// (`body_page`),这里不要重复算——早先这个常量是 6,把空行、表头、提示
/// 各算了一遍,而那三行现在都由控件自己画,于是 40 行的终端白白少画三行。
///
/// banner 与上一级回执不算在内:它们滚出屏幕是无害的,只要控件自己画的
/// 那一整块不超过终端高度,退场时 `clear_last_lines` 就能原样擦干净
/// (`browse_一屏不溢出` 按真实算式逐个行数验的就是这条)。
const BROWSE_RESERVED: usize = 3;

/// 终端行数减去调用方**自己已经占掉**的行数。取不到尺寸(管道、非 TTY)
/// 按 24 行算,地板 1 行 —— 一行高的视窗照样翻得动,而一个会自己溢出的
/// 地板比没有地板更糟:旧地板 5 在 rows 很小时完全压过 `saturating_sub`,
/// `reserved=3` 时 rows=5 会算成可用 5 行,控件实际画 6 行,溢出的是地板自己。
///
/// `reserved` 由调用方申报,不在这里写死:这个原语不知道调用方在它上面
/// 印了几行,而算错的代价是整屏滚动 + dialoguer 的擦除行数对不上。
pub(crate) fn viewport(reserved: usize) -> usize {
    viewport_of(
        Term::stderr()
            .size_checked()
            .map_or(24, |(rows, _)| rows as usize),
        reserved,
    )
}

/// 纯算术部分:抽出来让测试可以直接断言,不必 mock 终端尺寸。
///
/// 地板是 1 而不是 0:一行高的视窗照样翻得动,0 会让 `Viewport::new`
/// 和 `body_page` 的 `clamp(1, …)` 互相打架(而且除零也不是什么好事)。
/// 5 的地板在矮终端里自己就溢出了——`reserved=3` 时 rows=5 只剩下
/// 2 行可用,`.max(5)` 却强塞 5 行给控件,怎能不顶出屏幕。
fn viewport_of(rows: usize, reserved: usize) -> usize {
    rows.saturating_sub(reserved).max(1)
}

/// 分页浏览一批已经渲染好的行；Enter 返回选中下标，Esc 返回 None。
/// 非 TTY 返回 None：调用方照旧打完整表格，浏览是 TTY 上的增益，不是新契约。
///
/// `header` 是列名那一行,交给 [`prompt_pick`] 画在问句下面——它必须与条目
/// 一起被擦掉,否则退场后屏幕上留着一个没有表的表头。
pub fn browse(prompt: &str, header: &str, rows: &[String]) -> Option<usize> {
    if !std::io::stderr().is_terminal() || rows.is_empty() {
        return None;
    }
    // 键位提示写进问句:多于一页才提 ←/→,一页装得下时它们无处可翻,
    // 提一句就是撒一次谎。
    let page = viewport(BROWSE_RESERVED);
    let paged = rows.len() + 2 > page;
    let hint = if paged {
        format!("{prompt} · ↑/↓ move · ←/→ page · Enter opens · Esc cancels")
    } else {
        format!("{prompt} · ↑/↓ move · Enter opens · Esc cancels")
    };
    let theme = ColorfulTheme::default();
    let picked = prompt_pick(
        &theme,
        Picker {
            prompt: &hint,
            header: Some(header),
            items: rows,
            page,
            start: 0,
        },
    )
    .ok()
    .flatten()?;
    // 一行回执:整屏被擦掉之后,详情输出前面得留着「我选的是哪一条」。
    // 回执里不带键位提示——那是问的时候才有用的话。
    let mut buf = String::new();
    theme
        .format_select_prompt_selection(&mut buf, prompt, &rows[picked])
        .expect("writing to a String cannot fail");
    eprintln!("{buf}");
    Some(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非 TTY 时直接返回 None,不许挂起等按键;调用方据此回退打印完整表格。
    ///
    /// cargo test 把 stderr 接成管道,恰好就是这条路径;用 `--nocapture` 在
    /// 真终端上跑时前提不成立,跳过而不是挂起。
    #[test]
    fn 非_tty_时_browse_直接返回_none() {
        if std::io::stderr().is_terminal() {
            return;
        }
        assert!(!std::io::stdout().is_terminal(), "测试前提:stdout 应是管道");
        assert_eq!(browse("Pick one", "HEADER", &["row".to_string()]), None);
        // 空列表同样不该进交互——没有可选项的菜单是死胡同。
        assert_eq!(browse("Pick one", "HEADER", &[]), None);
    }

    #[test]
    fn viewport_保底一行() {
        assert_eq!(viewport_of(3, BROWSE_RESERVED), 1);
    }

    #[test]
    fn viewport_扣掉调用方占的行() {
        assert_eq!(viewport_of(40, BROWSE_RESERVED), 37);
    }

    /// 一屏不溢出的底线,按真实算式验:控件画的那一整块 = 问句 1 行、表头
    /// 1 行、条目若干行、页脚(多于一页才有)1 行,必须装进终端。而且因为
    /// `write_line` 每行尾巴带 `\n`,画完 k 行后光标停在第 k+1 行上,k 行
    /// 内容加 1 行光标位 = k+1 行空间,所以上限是 `drawn < rows` 而不是
    /// `drawn <= rows`。
    ///
    /// 溢出的代价不是难看:退场时 `clear_last_lines` 擦不到已经滚出屏幕的
    /// 行,屏幕上会留下半张表。断言写成 `BROWSE_RESERVED >= 5` 是同义反复
    /// (常量比常量),而且那 5 行早已不成立——空行、表头、提示三行现在都
    /// 由控件自己画,页高由 `body_page` 内部扣。
    ///
    /// 少数行数在物理上装不下:问句 + 表头 + `body_page` 保底的 1 行条目 +
    /// 页脚,这个最低开销已经吃满终端时,画得再少也救不回来(那是控件自己的
    /// 地板,viewport 管不到)。旧地板 5 会在 rows∈{4,5,6} 变红(rows=3 的
    /// 组合被下面的 skip 略过——它们物理上装不下,地板改不改都一样);地板
    /// 降到 1 后,所有**物理上装得下**的组合全部 `drawn < rows`。
    #[test]
    fn browse_一屏不溢出() {
        for rows in 3..=60usize {
            for len in [1usize, 5, 13, 20, 200] {
                for has_header in [false, true] {
                    let avail = viewport_of(rows, BROWSE_RESERVED);
                    let body = crate::prompt::body_page(avail, has_header, len);
                    let footer = usize::from(body < len);
                    let drawn = 1 + usize::from(has_header) + body.min(len) + footer;
                    // 控件的最低开销:问句 1 + 表头 + 保底 1 行条目 + 页脚,
                    // 再加上光标那 1 行还放不下,就不是 viewport 能救的组合。
                    let min_drawn = 1 + usize::from(has_header) + 1 + footer;
                    if min_drawn + 1 > rows {
                        continue;
                    }
                    assert!(
                        drawn < rows,
                        "{rows} 行终端 / {len} 条 / 表头={has_header}:画了 {drawn} 行,溢出"
                    );
                }
            }
        }
    }
}
