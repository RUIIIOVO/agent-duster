//! 分页浏览一批已经渲染好的行(交互菜单的「统一列表流」原语)。
//!
//! 列表类命令在菜单里的老结局是死胡同:整张表刷完,用户得手抄一个 key
//! 再去敲第二条命令。`browse` 把同一批行交给 [`prompt_browse`]——终端多高
//! 就显示几行,超出部分按页翻(←/→,末行报「第几到第几 / 共几行」),
//! 一屏之内三件事同时成立:
//!
//! - **Enter 下钻**:一行都没勾时,Enter 打开光标行([`Browsed::Open`])。
//! - **Space 勾选**:勾了再按 Enter,交出的是勾选集合([`Browsed::Act`]),
//!   调用方拿它去出批量动作菜单。勾选位由调用方持有,下钻来回不丢。
//! - **`/` 过滤**:问一个子串(大小写不敏感,对**渲染后的整行**匹配——
//!   用户看见什么就能滤什么),命中集合当场换上;空输入清过滤。设一个
//!   **新**过滤词会清掉已有勾选:勾选是对着旧视图打的,新视图里它们可能
//!   整行不可见,一张藏着勾选的表就是一张说谎的表。清过滤只会让行变多,
//!   已有勾选仍然全部可见,所以保留。
//!
//! # Esc 的梯子
//!
//! 一次 Esc 退一层,过滤算一层:过滤态下 Esc 先清过滤回全表,再按一次才
//! 退出这一屏([`Browsed::Quit`])。这与整个菜单「任何一屏一次 Esc = 返回
//! 上一层」的契约同构。
//!
//! # 光标要带回来
//!
//! 下钻看完一条再回到列表时,光标必须还停在那一条上。[`browse`] 收调用方
//! 持有的 `cursor`(**原始**下标),回列表时还停在那一行。少了这一条,
//! 「看完第 37 条回列表」就等于「回到第 1 条,自己再翻三页」。
//!
//! # 行的契约
//!
//! 行文本由调用方**预先对齐**:列宽按 [`display_width`] 算好、宽字符不会
//! 顶歪下一列(与 `interactive.rs` 的 `checklist_rows` 同一套量法)。原语
//! 不认识列结构,也绝不自己写死列宽——塞进来的行是什么样,屏幕就是什么样。
//! 行宽预算按 `Prefix::Checkbox` 扣:每行前面画的是 `❯ [x] `(6 列)。
//!
//! # 非 TTY 直接 Quit
//!
//! 菜单只在 TTY 下存在,这里拿不到终端就说明调用方跑错了地方;返回 Quit
//! 让调用方原样收场——浏览是 TTY 上的增益,不是新契约。
//!
//! [`display_width`]: crate::output::display_width

use std::borrow::Cow;
use std::io::IsTerminal;

use console::{Term, style};
use dialoguer::theme::{ColorfulTheme, Theme};

use crate::prompt::{BrowseEvent, Browser, prompt_browse, prompt_line};

/// 交给 [`prompt_browse`] 的可用行数要从终端行数里让出几行:控件自己画问句
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
    let rows = match Term::stderr().size() {
        (0, _) => 24,
        (r, _) => usize::from(r),
    };
    viewport_of(rows, reserved)
}

/// 纯算术部分:抽出来让测试可以直接断言,不必 mock 终端尺寸。
///
/// 地板是 1 而不是 0:一行高的视窗照样翻得动,0 会让 `Viewport::new`
/// 和 `body_page` 的 `clamp(1, …)` 互相打架(而且除零也不是什么好事)。
/// 5 的地板在矮终端里自己就溢出了——`reserved=3` 时 rows=5 只剩下
/// 2 行可用,`.max(5)` 却强塞 5 行给控件,怎能不顶出屏幕。
pub(crate) fn viewport_of(rows: usize, reserved: usize) -> usize {
    rows.saturating_sub(reserved).max(1)
}

/// 一趟浏览的收场。`Open` / `Act` 里的下标都是**原始**行下标——过滤只是
/// 视图,调用方手里的数据从来不用跟着重排。
#[derive(Debug, PartialEq, Eq)]
pub enum Browsed {
    /// Enter 且一行都没勾:下钻这一行。
    Open(usize),
    /// Enter 且有勾选:对这批行动手(下标升序)。
    Act(Vec<usize>),
    /// Esc(无过滤态)或终端不可用:退出这一屏。
    Quit,
}

/// 分页浏览一批已经渲染好的行;过滤、勾选、光标都存在调用方递进来的
/// 可变引用里,下钻回来原样接着用。
///
/// - `cursor`:原始下标。进场时光标停在它上面(被过滤掉就落回可见的第一
///   行),`Open` 时更新——下一趟进来就停在上次看的那行。
/// - `checked`:长度与 `rows` 一致的勾选位。设新过滤词时这里会被清空
///   (理由见模块文档),其余时刻原样保留。
/// - `filter`:当前过滤词。`/` 设置、空输入或 Esc 清除;滤不出任何行的
///   词不会生效(说一句,保持原状)。
///
/// 非 TTY 或空表返回 [`Browsed::Quit`]:没有可选项的菜单是死胡同。
pub fn browse(
    prompt: &str,
    header: &str,
    rows: &[String],
    cursor: &mut usize,
    checked: &mut [bool],
    filter: &mut Option<String>,
) -> Browsed {
    if !std::io::stderr().is_terminal() || rows.is_empty() {
        return Browsed::Quit;
    }
    debug_assert_eq!(checked.len(), rows.len());
    let theme = ColorfulTheme::default();
    loop {
        let vis = visible_indices(rows, filter.as_deref());
        // 视图行:没过滤就借原表,不逐行克隆——过滤才付拷贝的钱。
        let view_rows: Cow<'_, [String]> = if vis.len() == rows.len() {
            Cow::Borrowed(rows)
        } else {
            Cow::Owned(vis.iter().map(|&i| rows[i].clone()).collect())
        };
        let page = viewport(BROWSE_RESERVED);
        // 键位提示写进问句:多于一页才提 ←/→,一页装得下时它们无处可翻,
        // 提一句就是撒一次谎。
        let paged = view_rows.len() + 2 > page;
        let hint = hint_line(prompt, filter.as_deref(), paged);
        let start = vis.iter().position(|&i| i == *cursor).unwrap_or(0);
        let mut vis_checked: Vec<bool> = vis.iter().map(|&i| checked[i]).collect();
        let event = prompt_browse(
            &theme,
            Browser {
                prompt: &hint,
                header: Some(header),
                items: &view_rows,
                page,
                start,
                checked: &mut vis_checked,
            },
        );
        // 控件只看得到过滤后的切片,原始勾选位由这里写回。
        for (k, &orig) in vis.iter().enumerate() {
            checked[orig] = vis_checked[k];
        }
        match event {
            Ok(BrowseEvent::Open(k)) => {
                let orig = vis[k];
                *cursor = orig;
                // 一行回执:整屏被擦掉之后,详情输出前面得留着「我选的是
                // 哪一条」。回执里不带键位提示——那是问的时候才有用的话。
                receipt(&theme, prompt, &rows[orig]);
                return Browsed::Open(orig);
            }
            Ok(BrowseEvent::Act(ks)) => {
                let sel: Vec<usize> = ks.into_iter().map(|k| vis[k]).collect();
                receipt(&theme, prompt, &format!("{} checked", sel.len()));
                return Browsed::Act(sel);
            }
            Ok(BrowseEvent::Filter) => {
                match prompt_line(&theme, "Filter", None) {
                    Ok(Some(word)) => {
                        let word = word.trim().to_string();
                        if word.is_empty() {
                            // 空输入 = 清过滤。已有勾选仍然全部可见,保留。
                            *filter = None;
                        } else if visible_indices(rows, Some(&word)).is_empty() {
                            // 滤空的词不生效:一屏零行没有任何键可按,
                            // 说一句,保持原状。
                            eprintln!(
                                "  {}",
                                style(format!("nothing matches \"{word}\"")).dim()
                            );
                        } else {
                            // 新视图,旧勾选作废(理由见模块文档)。
                            *filter = Some(word);
                            checked.iter_mut().for_each(|c| *c = false);
                        }
                    }
                    // Esc:过滤词不变,回列表。
                    Ok(None) => {}
                    Err(_) => return Browsed::Quit,
                }
            }
            Ok(BrowseEvent::Cancel) => {
                // Esc 的梯子:过滤态先清过滤,再按一次才真正退出。
                if filter.is_some() {
                    *filter = None;
                } else {
                    return Browsed::Quit;
                }
            }
            Err(_) => return Browsed::Quit,
        }
    }
}

/// 过滤视图:命中过滤词的原始下标。`None` = 全部;匹配大小写不敏感,
/// 先剥 ANSI 转义再比——色码会把词拦腰截断,而用户滤的是看得见的字。
fn visible_indices(rows: &[String], filter: Option<&str>) -> Vec<usize> {
    match filter {
        None => (0..rows.len()).collect(),
        Some(f) => {
            let needle = f.to_lowercase();
            rows.iter()
                .enumerate()
                .filter(|(_, r)| {
                    console::strip_ansi_codes(r)
                        .to_lowercase()
                        .contains(&needle)
                })
                .map(|(i, _)| i)
                .collect()
        }
    }
}

/// 键位提示行。词表与顺序全菜单统一(move · page · filter · toggle ·
/// Enter <动词> · Esc <去向>),每屏只列真有的键:一页装得下就没有 page;
/// 过滤态下 Esc 的第一层是清过滤,提示就得写清过滤——写 back 就是撒谎。
fn hint_line(prompt: &str, filter: Option<&str>, paged: bool) -> String {
    let mut s = String::with_capacity(prompt.len() + 96);
    s.push_str(prompt);
    if let Some(f) = filter {
        s.push_str(" · filter \"");
        s.push_str(f);
        s.push('"');
    }
    s.push_str(" · ↑/↓ move");
    if paged {
        s.push_str(" · ←/→ page");
    }
    s.push_str(" · / filter · Space toggle · Enter open");
    s.push_str(if filter.is_some() {
        " · Esc clear filter"
    } else {
        " · Esc back"
    });
    s
}

/// 一行回执,风格与 dialoguer 的 `✔` 完成标记一致。
fn receipt(theme: &ColorfulTheme, prompt: &str, chosen: &str) {
    let mut buf = String::new();
    theme
        .format_select_prompt_selection(&mut buf, prompt, chosen)
        .expect("writing to a String cannot fail");
    eprintln!("{buf}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非 TTY 时直接返回 Quit,不许挂起等按键;调用方据此收场。
    ///
    /// cargo test 把 stderr 接成管道,恰好就是这条路径;用 `--nocapture` 在
    /// 真终端上跑时前提不成立,跳过而不是挂起。
    #[test]
    fn 非_tty_时_browse_直接返回_quit() {
        if std::io::stderr().is_terminal() {
            return;
        }
        assert!(!std::io::stdout().is_terminal(), "测试前提:stdout 应是管道");
        let rows = vec!["row".to_string()];
        let mut checked = vec![false];
        assert_eq!(
            browse("Pick one", "HEADER", &rows, &mut 0, &mut checked, &mut None),
            Browsed::Quit
        );
        // 空列表同样不该进交互——没有可选项的菜单是死胡同。
        assert_eq!(
            browse("Pick one", "HEADER", &[], &mut 0, &mut [], &mut None),
            Browsed::Quit
        );
        // 越界的光标也不许把这条早退路径绕过去:非 TTY 先于一切判定。
        let mut far = 99;
        assert_eq!(
            browse("Pick one", "HEADER", &rows, &mut far, &mut checked, &mut None),
            Browsed::Quit
        );
    }

    #[test]
    fn viewport_保底一行() {
        assert_eq!(viewport_of(3, BROWSE_RESERVED), 1);
    }

    #[test]
    fn viewport_扣掉调用方占的行() {
        assert_eq!(viewport_of(40, BROWSE_RESERVED), 37);
    }

    /// 过滤是大小写不敏感的子串,对剥掉 ANSI 的整行匹配;None = 全部保留,
    /// 下标恒为原始下标——`Open`/`Act` 交出去的数字要能直接进原始数据。
    #[test]
    fn 过滤_大小写不敏感且剥_ansi() {
        let rows = vec![
            format!("{}  12 MB", console::style("codex").cyan().force_styling(true)),
            "claude-code  3 MB".to_string(),
            "omp  1 MB".to_string(),
        ];
        assert_eq!(visible_indices(&rows, None), vec![0, 1, 2]);
        assert_eq!(visible_indices(&rows, Some("CODE")), vec![0, 1]);
        assert_eq!(visible_indices(&rows, Some("omp")), vec![2]);
        assert_eq!(visible_indices(&rows, Some("qoder")), Vec::<usize>::new());
        // 色码不许挡住被它包裹的词。
        assert_eq!(visible_indices(&rows, Some("codex")), vec![0]);
    }

    /// 提示行按屏取子集、顺序固定:move · page · filter · toggle · Enter ·
    /// Esc。一页装得下就没有 page;过滤态下 Esc 写「clear filter」,并把
    /// 当前过滤词亮出来——不亮出来,用户就不知道自己看的是一角还是全部。
    #[test]
    fn 提示行_按屏取子集顺序固定() {
        let plain = hint_line("Which server", None, false);
        assert_eq!(
            plain,
            "Which server · ↑/↓ move · / filter · Space toggle · Enter open · Esc back"
        );
        let paged = hint_line("Which server", None, true);
        assert_eq!(
            paged,
            "Which server · ↑/↓ move · ←/→ page · / filter · Space toggle · Enter open · Esc back"
        );
        let filtered = hint_line("Which server", Some("codex"), true);
        assert_eq!(
            filtered,
            "Which server · filter \"codex\" · ↑/↓ move · ←/→ page · / filter · Space toggle · Enter open · Esc clear filter"
        );
    }

    /// 一屏不溢出的底线,按真实算式验:控件画的那一整块 = 问句 1 行、表头
    /// 1 行、条目若干行、页脚(多于一页才有)1 行,必须装进终端。而且因为
    /// `write_line` 每行尾巴带 `\n`,画完 k 行后光标停在第 k+1 行上,k 行
    /// 内容加 1 行光标位 = k+1 行空间,所以上限是 `drawn < rows` 而不是
    /// `drawn <= rows`。
    ///
    /// 溢出的代价不是难看:退场时 `clear_last_lines` 擦不到已经滚出屏幕的
    /// 行,屏幕上会留下半张表。
    ///
    /// 少数行数在物理上装不下:问句 + 表头 + `body_page` 保底的 1 行条目 +
    /// 页脚,这个最低开销已经吃满终端时,画得再少也救不回来(那是控件自己的
    /// 地板,viewport 管不到)。地板 1 之下,所有**物理上装得下**的组合全部
    /// `drawn < rows`。
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
