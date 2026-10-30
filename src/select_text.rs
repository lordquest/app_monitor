//! 可选中、可复制的长文本。
//!
//! GPUI 0.2.2 没有文本选中：`Text` / `StyledText` 只负责把字画出来，不认拖选，
//! 也没有选区这个概念（`grep -ri selectable` 只能查到输入框和剪贴板）。但排版
//! 结果 `WrappedLine` 里每个字形的 x 坐标都留着，`closest_index_for_position`
//! 能把鼠标位置换回字节索引 —— 也就是说选中不必 fork GPUI，自己画就行：
//!
//! 1. [`prepare`] 用与 `Text` 同一个 `shape_text` 排一次版，拿到行、段与总高度；
//! 2. [`paint`] 先画选区高亮、再逐行画字（`WrappedLine::paint` 是公开 API）；
//! 3. 命中用 [`Prepared::index_at`]，选区状态放在 `App` 里。
//!
//! 代价是这段文本不再走 GPUI 的文本元素，字号、行高、颜色都得自己给 —— 所以
//! 调用方传进来的这几项必须与周围样式一致，否则字会跳一下。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    Bounds, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString, Size,
    TextAlign, TextRun, Window, WrappedLine, canvas, div, fill, point, px, rgba, size,
};
use gpui::{IntoElement, ParentElement, Styled};

/// 选区高亮色：主色的 20% 透明，压在浅底上不刺眼，也不至于把字糊住。
const SELECTION: u32 = 0x2563EB40;

/// 排好版的文本。每帧渲染时重建 —— `shape_text` 内部按 (字体, 字号, 宽度) 缓存，
/// 重建的代价只有一次哈希查找。
pub struct Prepared {
    text: SharedString,
    lines: Vec<WrappedLine>,
    /// 每段（`\n` 分隔）在全文中的（起始字节, 字节长度）。
    segs: Vec<(usize, usize)>,
    line_height: Pixels,
    size: Size<Pixels>,
}

impl Prepared {
    /// 鼠标位置（相对文本左上角）落在第几个字节上。
    ///
    /// `closest_index_for_position` 返回的是 `Result<usize, usize>`：`Ok` 落在行内，
    /// `Err` 表示点在首行左边或末行右边、值就是该取到的索引 —— 两边都当索引用。
    pub fn index_at(&self, local: Point<Pixels>) -> usize {
        let mut top = px(0.);
        let last = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            let seg_h = line.size(self.line_height).height;
            let (seg_start, seg_len) = self.segs[i];
            if local.y < top + seg_h || i == last {
                let y = (local.y - top).max(px(0.)).min(seg_h - px(1.));
                let ix = line
                    .closest_index_for_position(point(local.x, y), self.line_height)
                    .unwrap_or_else(|e| e);
                return seg_start + ix.min(seg_len);
            }
            top += seg_h;
        }
        self.text.len()
    }

    /// 选区（绝对字节，两端顺序随意）覆盖的矩形，每显示行一个。
    fn highlights(&self, sel: (usize, usize), origin: Point<Pixels>) -> Vec<Bounds<Pixels>> {
        let (start, end) = if sel.0 <= sel.1 {
            sel
        } else {
            (sel.1, sel.0)
        };
        let mut out = Vec::new();
        let mut top = origin.y;
        for (i, line) in self.lines.iter().enumerate() {
            let seg_h = line.size(self.line_height).height;
            let (seg_start, seg_len) = self.segs[i];
            let seg_end = seg_start + seg_len;
            if end > seg_start && start < seg_end {
                let s = start.max(seg_start) - seg_start;
                let e = end.min(seg_end) - seg_start;

                // 折行边界给的是「上一行最后一个字形的字节位置」，所以下一行从
                // +1 开始 —— 这与 `position_for_index` 内部的分行判断一致。
                let mut cuts = Vec::with_capacity(line.wrap_boundaries().len());
                for b in line.wrap_boundaries() {
                    cuts.push(line.runs()[b.run_ix].glyphs[b.glyph_ix].index);
                }
                let mut ls = 0usize;
                let mut y = top;
                for k in 0..=cuts.len() {
                    let le = cuts.get(k).map(|c| c + 1).unwrap_or(seg_len);
                    let s_l = s.max(ls);
                    let e_l = e.min(le);
                    if s_l < e_l {
                        // 一句话里可能换字体（中英混排），x 得整行减行首偏移才准。
                        let base = line.unwrapped_layout.x_for_index(ls);
                        let x1 = line.unwrapped_layout.x_for_index(s_l) - base;
                        let x2 =
                            line.unwrapped_layout.x_for_index(e_l.min(seg_len)) - base;
                        if x2 > x1 {
                            // 上下各留 9% 的空隙，看起来像浏览器里的选中而不是一条色块。
                            out.push(Bounds::new(
                                point(origin.x + x1, y + self.line_height * 0.09),
                                size(x2 - x1, self.line_height * 0.82),
                            ));
                        }
                    }
                    ls = le;
                    y += self.line_height;
                }
            }
            top += seg_h;
        }
        out
    }
}

/// 排版一段文本。`width` 必须是确定值（换行宽度），与传给 div 的宽度一致。
pub fn prepare(
    text: &str,
    width: Pixels,
    font_size: Pixels,
    line_height: Pixels,
    family: &str,
    color: Hsla,
    window: &mut Window,
) -> Rc<Prepared> {
    let text: SharedString = text.to_string().into();
    let run = TextRun {
        len: text.len(),
        font: Font {
            // `&str` 进 SharedString 要一份拥有所有权的拷贝：run 会被排版系统留下。
            family: SharedString::from(family.to_string()),
            features: FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        },
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(text.clone(), font_size, &[run], Some(width), None)
        .map(|lines| lines.into_iter().collect())
        // 排版失败（字体缺失之类）时给空集合，画出来是空白而不是 panic。
        .unwrap_or_default();

    let mut segs = Vec::with_capacity(lines.len());
    let mut offset = 0;
    let mut height = px(0.);
    let count = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let len = line.len();
        segs.push((offset, len));
        // 段与段之间那个 `\n` 占一个字节，不补上后面的偏移就全错位了。
        offset += len + if i + 1 == count { 0 } else { 1 };
        height += line.size(line_height).height;
    }

    Rc::new(Prepared {
        text,
        lines,
        segs,
        line_height,
        size: size(width, height),
    })
}

/// 画出这段文本（含选区高亮），并把本次的 bounds 记到 `geom` 里供鼠标命中用。
///
/// `geom` 由调用方持有：鼠标事件在 div 上，拿不到元素自己的 bounds，只能靠
/// 绘制阶段回填一次。
pub fn paint(
    prep: Rc<Prepared>,
    sel: Option<(usize, usize)>,
    geom: Rc<RefCell<Option<Bounds<Pixels>>>>,
) -> impl IntoElement {
    let for_paint = prep.clone();
    div()
        .w(prep.size.width)
        .h(prep.size.height)
        .child(
            canvas(
                move |bounds, _window, _cx| {
                    *geom.borrow_mut() = Some(bounds);
                },
                move |bounds, _, window, cx| {
                    if let Some(sel) = sel {
                        for quad in for_paint.highlights(sel, bounds.origin) {
                            window.paint_quad(fill(quad, rgba(SELECTION)));
                        }
                    }
                    let mut top = bounds.origin;
                    for line in &for_paint.lines {
                        let seg_h = line.size(for_paint.line_height).height;
                        line.paint(
                            top,
                            for_paint.line_height,
                            TextAlign::Left,
                            Some(bounds),
                            window,
                            cx,
                        )
                        .ok();
                        top.y += seg_h;
                    }
                },
            )
            .size_full(),
        )
}

/// 把字节索引挪到最近的字符边界。命中给的索引理论上都在边界上，但
/// `slices` 的 `+1` 运算可能落在多字节字符中间，复制时会 panic，所以统一过一遍。
pub fn snap_to_char(text: &str, mut ix: usize) -> usize {
    ix = ix.min(text.len());
    while ix > 0 && !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

#[cfg(test)]
mod tests {
    use super::snap_to_char;

    #[test]
    fn snaps_into_the_middle_of_a_multibyte_char() {
        let s = "中文abc";
        assert_eq!(snap_to_char(s, 0), 0);
        assert_eq!(snap_to_char(s, 3), 3, "「中」后面是边界");
        assert_eq!(snap_to_char(s, 1), 0, "「中」中间要退到 0");
        assert_eq!(snap_to_char(s, 2), 0);
        assert_eq!(snap_to_char(s, 99), s.len(), "越界夹到末尾");
    }

    #[test]
    fn snapping_keeps_ascii_untouched() {
        let s = "hello";
        for i in 0..=s.len() {
            assert_eq!(snap_to_char(s, i), i);
        }
    }
}
