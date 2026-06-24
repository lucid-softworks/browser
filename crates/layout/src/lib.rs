//! Box-model layout. Turns the styled DOM into a tree of positioned boxes.
//!
//! This file defines the *public contract* (geometry types, the paint-facing [`LayoutBox`],
//! the [`TextMeasurer`] trait, and [`layout_document`]). The block/inline layout algorithm
//! that fills it in is implemented against these types. Consumers (the engine's painter)
//! depend only on the shapes here, never on how layout is computed.

use std::collections::HashMap;

mod block;
mod build;
mod flex;
mod float;
mod grid;
mod inline;
mod intrinsic;
mod sizing;
mod table;
mod types;

pub(crate) use block::*;
pub(crate) use build::*;
pub(crate) use float::*;

/// Run `f` with a guarantee of at least ~1 MiB of stack headroom, allocating a fresh stack segment
/// if the current one is nearly exhausted. Wrap the per-level recursive call in the two layout
/// descents (box-tree build, block layout) with this so a pathologically deep DOM grows the stack
/// instead of overflowing it — debug frames are large enough that a few hundred levels would
/// otherwise abort. The fast path (plenty of headroom) is just a stack-pointer check.
#[inline]
pub(crate) fn grow_stack<R>(f: impl FnOnce() -> R) -> R {
    // 64 KiB red zone: if less remains, allocate a 1 MiB segment before recursing.
    stacker::maybe_grow(64 * 1024, 1024 * 1024, f)
}
pub(crate) use flex::*;
pub(crate) use grid::*;
pub(crate) use inline::*;
pub(crate) use intrinsic::*;
pub(crate) use sizing::*;
pub(crate) use table::*;
pub use types::*;

/// Lay out `doc` (with its computed `styles`) into a tree of positioned boxes that fits a
/// viewport `viewport_width` pixels wide. Height is driven by content. The returned root box
/// is positioned at (0, 0); the painter walks it.
pub fn layout_document(
    doc: &dom::Document,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
    viewport_width: f32,
    viewport_height: f32,
    measurer: &dyn TextMeasurer,
    intrinsic_sizes: &HashMap<dom::NodeId, (f32, f32)>,
    focused: Option<dom::NodeId>,
) -> LayoutBox {
    // 1. Build the box tree from the DOM (skipping hidden / non-rendered subtrees), inserting
    //    anonymous blocks where block and inline siblings mix. Image boxes are sized from their
    //    intrinsic dimensions (and any CSS width/height) during layout. `focused` is the node id
    //    of the focused text field, which gets a `BoxContent::Caret` bar after its value text.
    // Snapshot every table cell's colspan/rowspan from the DOM so `layout_table` (which only sees
    // the box tree + styles) can honor them without a new threaded parameter.
    capture_table_spans(doc);
    let mut root = LayoutBox::new(BoxContent::Block, PaintStyle::default(), None);
    let bx_ctx = BuildCtx {
        styles,
        intrinsic_sizes,
        focused,
    };
    root.children = build_children(doc, doc.root(), &bx_ctx);

    // 2. The root is the viewport block. Lay it out against a containing block that is the
    //    viewport: origin (0,0), width = viewport_width.
    let viewport = Rect {
        x: 0.0,
        y: 0.0,
        width: viewport_width,
        height: viewport_height,
    };
    let containing = Rect {
        x: 0.0,
        y: 0.0,
        width: viewport_width,
        height: 0.0,
    };
    // The initial containing block (for absolutes with no positioned ancestor) is the viewport.
    let ctx = Ctx {
        positioned: viewport,
        viewport,
    };
    layout_block(&mut root, containing, ctx, styles, measurer);
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stub measurer: each char is `0.6 * px` wide; line height is `1.3 * px`.
    struct Stub;
    impl TextMeasurer for Stub {
        fn text_width(&self, text: &str, px: f32, _bold: bool, _family: Option<&str>) -> f32 {
            text.chars().count() as f32 * px * 0.6
        }
        fn line_height(&self, px: f32, _family: Option<&str>) -> f32 {
            px * 1.3
        }
    }

    /// Build a styled document: returns the doc plus the computed-style map. `setup` populates
    /// the DOM and returns nothing; styles are supplied directly per node id.
    fn block_style(display_block: bool) -> style::ComputedStyle {
        style::ComputedStyle {
            display_block,
            ..Default::default()
        }
    }

    /// Find the first descendant box (DFS) matching `pred`.
    fn find_box<'a>(b: &'a LayoutBox, pred: &dyn Fn(&LayoutBox) -> bool) -> Option<&'a LayoutBox> {
        if pred(b) {
            return Some(b);
        }
        for c in &b.children {
            if let Some(f) = find_box(c, pred) {
                return Some(f);
            }
        }
        None
    }

    /// A floated block of explicit content size.
    fn floated(width: f32, height: f32, side: style::Float) -> style::ComputedStyle {
        style::ComputedStyle {
            display_block: true,
            float: side,
            width: Some(width),
            height: Some(height),
            ..Default::default()
        }
    }

    /// A floated block whose width is a percentage of its containing block.
    fn floated_pct(pct: f32, height: f32, side: style::Float) -> style::ComputedStyle {
        style::ComputedStyle {
            display_block: true,
            float: side,
            width_pct: Some(pct),
            height: Some(height),
            ..Default::default()
        }
    }

    /// An inline-block of explicit content size.
    fn inline_block(width: f32, height: f32) -> style::ComputedStyle {
        style::ComputedStyle {
            display: style::Display::InlineBlock,
            width: Some(width),
            height: Some(height),
            ..Default::default()
        }
    }

    /// The border-box rect of the box whose node id is `n`.
    fn rect_of(root: &LayoutBox, n: dom::NodeId) -> Rect {
        find_box(root, &|b| b.node == Some(n))
            .unwrap_or_else(|| panic!("box for node {n:?} not found"))
            .dimensions
            .border_box()
    }

    fn count_boxes(b: &LayoutBox, pred: &dyn Fn(&LayoutBox) -> bool) -> usize {
        let mut n = if pred(b) { 1 } else { 0 };
        for c in &b.children {
            n += count_boxes(c, pred);
        }
        n
    }

    /// Collect every Text box (DFS) into a flat list.
    fn collect_text_box_list(b: &LayoutBox) -> Vec<&LayoutBox> {
        let mut v = Vec::new();
        fn go<'a>(b: &'a LayoutBox, v: &mut Vec<&'a LayoutBox>) {
            if matches!(b.content, BoxContent::Text(_)) {
                v.push(b);
            }
            for c in &b.children {
                go(c, v);
            }
        }
        go(b, &mut v);
        v
    }

    #[test]
    fn br_splits_text_onto_two_lines() {
        // body > p > "first" <br> "second"
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("first".into()));
        let br = doc.append_element(p, "br");
        doc.append_child(p, dom::NodeData::Text("second".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        // <br> is inline; the build path special-cases the tag, so any style works.
        styles.insert(br, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let texts = collect_text_box_list(&root_box);
        let first = texts
            .iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "first"))
            .unwrap();
        let second = texts
            .iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "second"))
            .unwrap();
        // The <br> forces "second" onto the next line: strictly greater y.
        assert!(
            second.dimensions.content.y > first.dimensions.content.y,
            "second.y={} should be below first.y={}",
            second.dimensions.content.y,
            first.dimensions.content.y
        );
        // And both start at the same x (left edge), confirming it's a line break, not a wrap mid-line.
        assert_eq!(first.dimensions.content.x, second.dimensions.content.x);
    }

    #[test]
    fn pre_preserves_spaces_and_newline() {
        // body > pre  with text "a   b\nc": 3 spaces preserved, newline → two lines.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let pre = doc.append_element(body, "pre");
        doc.append_child(pre, dom::NodeData::Text("a   b\nc".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            pre,
            style::ComputedStyle {
                display_block: true,
                white_space: style::WhiteSpace::Pre,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let texts = collect_text_box_list(&root_box);
        // Two runs: "a   b" (line 1) and "c" (line 2).
        let l1 = texts
            .iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "a   b"))
            .expect("first pre line preserved with 3 spaces");
        let l2 = texts
            .iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "c"))
            .expect("second pre line after the newline");
        // The newline put them on different lines.
        assert!(
            l2.dimensions.content.y > l1.dimensions.content.y,
            "newline should drop 'c' to a new line"
        );
        // The first run's width reflects the preserved spaces: "a   b" = 5 chars at 0.6*16.
        let expected = Stub.text_width("a   b", 16.0, false, None);
        assert!(
            (l1.dimensions.content.width - expected).abs() < 0.01,
            "width {} != {}",
            l1.dimensions.content.width,
            expected
        );
    }

    #[test]
    fn ul_generates_bullet_markers_and_ol_numbers() {
        // body > ul > li,li   and   body > ol > li,li
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let ul = doc.append_element(body, "ul");
        let li1 = doc.append_element(ul, "li");
        doc.append_child(li1, dom::NodeData::Text("one".into()));
        let li2 = doc.append_element(ul, "li");
        doc.append_child(li2, dom::NodeData::Text("two".into()));
        let ol = doc.append_element(body, "ol");
        let oli1 = doc.append_element(ol, "li");
        doc.append_child(oli1, dom::NodeData::Text("x".into()));
        let oli2 = doc.append_element(ol, "li");
        doc.append_child(oli2, dom::NodeData::Text("y".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        // ul: disc markers; padding-left 40 like the UA sheet.
        let mut ul_s = block_style(true);
        ul_s.list_style_type = style::ListStyleType::Disc;
        ul_s.padding = style::Edges {
            left: 40.0,
            ..Default::default()
        };
        styles.insert(ul, ul_s);
        // ol: decimal markers (li inherit list_style_type).
        let mut ol_s = block_style(true);
        ol_s.list_style_type = style::ListStyleType::Decimal;
        ol_s.padding = style::Edges {
            left: 40.0,
            ..Default::default()
        };
        styles.insert(ol, ol_s);
        for (id, lst) in [
            (li1, style::ListStyleType::Disc),
            (li2, style::ListStyleType::Disc),
            (oli1, style::ListStyleType::Decimal),
            (oli2, style::ListStyleType::Decimal),
        ] {
            let mut s = block_style(true);
            s.list_style_type = lst;
            styles.insert(id, s);
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        // Collect markers.
        fn markers(b: &LayoutBox, out: &mut Vec<String>) {
            if let BoxContent::Marker(s) = &b.content {
                out.push(s.to_string());
            }
            for c in &b.children {
                markers(c, out);
            }
        }
        let mut ms = Vec::new();
        markers(&root_box, &mut ms);
        // Two bullets (•) for the ul, then "1." and "2." for the ol.
        assert!(
            ms.iter().filter(|m| *m == "\u{2022}").count() == 2,
            "expected two disc bullets, got {ms:?}"
        );
        assert!(
            ms.contains(&"1.".to_string()) && ms.contains(&"2.".to_string()),
            "expected 1. and 2. ol markers, got {ms:?}"
        );
        // The first ul li's marker sits to the LEFT of the li content (in the 40px padding).
        let li1_box = find_box(&root_box, &|x| {
            x.node == Some(li1) && matches!(x.content, BoxContent::Block)
        })
        .unwrap();
        let marker_box = find_box(&root_box, &|x| {
            matches!(x.content, BoxContent::Marker(_)) && x.node == Some(li1)
        })
        .unwrap();
        assert!(
            marker_box.dimensions.content.x < li1_box.dimensions.content.x,
            "marker should be left of li content"
        );
    }

    #[test]
    fn two_stacked_blocks_increasing_y_and_heights() {
        // body > div#a (height 30), div#b (height 50)
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");
        let b = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                height: Some(30.0),
                ..Default::default()
            },
        );
        styles.insert(
            b,
            style::ComputedStyle {
                display_block: true,
                height: Some(50.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        assert_eq!(abox.dimensions.content.y, 0.0);
        assert_eq!(abox.dimensions.content.height, 30.0);
        // b stacks below a (a's margin box height = 30).
        assert_eq!(bbox.dimensions.content.y, 30.0);
        assert_eq!(bbox.dimensions.content.height, 50.0);
    }

    #[test]
    fn padding_and_border_offset_content_rect() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                height: Some(20.0),
                padding: style::Edges::all(5.0),
                border: style::Edges::all(2.0),
                margin: style::Edges::all(10.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let c = abox.dimensions.content;
        // content x = 0 (containing) + margin.left 10 + border.left 2 + padding.left 5 = 17.
        assert_eq!(c.x, 17.0);
        assert_eq!(c.y, 17.0);
        // border box = content expanded by padding (5) + border (2): origin shifts by 7.
        let bb = abox.dimensions.border_box();
        assert_eq!(bb.x, 10.0); // content.x 17 - padding 5 - border 2
                                // margin box origin is at the containing origin.
        let mb = abox.dimensions.margin_box();
        assert_eq!(mb.x, 0.0);
        assert_eq!(mb.y, 0.0);
        // content width = 800 - (margin 20 + border 4 + padding 10) = 766.
        assert_eq!(c.width, 766.0);
    }

    #[test]
    fn max_width_clamps_box_in_wide_container() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                max_width: Some(style::SizeConstraint::Px(200.0)),
                ..Default::default()
            },
        );

        // Container is 1000 wide; the box must be clamped to 200.
        let root_box = layout_document(&doc, &styles, 1000.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.width, 200.0);
    }

    #[test]
    fn min_width_raises_small_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                width: Some(50.0),
                min_width: Some(style::SizeConstraint::Px(120.0)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        // min-width raises the 50px width to 120.
        assert_eq!(abox.dimensions.content.width, 120.0);
    }

    #[test]
    fn max_height_clamps_box_height() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                height: Some(300.0),
                max_height: Some(style::SizeConstraint::Px(100.0)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.height, 100.0);
    }

    #[test]
    fn text_transform_uppercases_text_box_content() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("hello world".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            p,
            style::ComputedStyle {
                display_block: true,
                text_transform: style::TextTransform::Uppercase,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let texts = collect_text_boxes(pbox);
        let joined: String = texts
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("HELLO"), "got {joined:?}");
        assert!(joined.contains("WORLD"), "got {joined:?}");
    }

    #[test]
    fn line_height_changes_line_advance() {
        // A single line of text with line-height 40px → the block's content height is 40 (one
        // line), versus the default ~20.8 font metric.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("one".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            p,
            style::ComputedStyle {
                display_block: true,
                line_height: Some(40.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        assert!(
            (pbox.dimensions.content.height - 40.0).abs() < 0.01,
            "expected line advance 40, got {}",
            pbox.dimensions.content.height
        );
    }

    #[test]
    fn explicit_width_is_respected() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display_block: true,
                width: Some(200.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert_eq!(abox.dimensions.content.width, 200.0);
    }

    #[test]
    fn floats_pack_left_to_right_then_wrap() {
        // Three float:left divs of width 300 in an 800px body: two fit on the first row, the third
        // wraps below. The container grows to contain them. (wikipedia.org footer project grid.)
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let a = doc.append_element(wrap, "div");
        let b = doc.append_element(wrap, "div");
        let c = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(wrap, block_style(true));
        let fl = |w: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            float: style::Float::Left,
            width: Some(w),
            height: Some(50.0),
            ..Default::default()
        };
        styles.insert(a, fl(300.0));
        styles.insert(b, fl(300.0));
        styles.insert(c, fl(300.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ab = find_box(&root_box, &|x| x.node == Some(a))
            .unwrap()
            .dimensions
            .content;
        let bb = find_box(&root_box, &|x| x.node == Some(b))
            .unwrap()
            .dimensions
            .content;
        let cb = find_box(&root_box, &|x| x.node == Some(c))
            .unwrap()
            .dimensions
            .content;
        let wb = find_box(&root_box, &|x| x.node == Some(wrap))
            .unwrap()
            .dimensions
            .content;

        // a and b share the first row, packed left to right.
        assert!((ab.y - bb.y).abs() < 0.01, "a.y={} b.y={}", ab.y, bb.y);
        assert!(
            bb.x > ab.x + 0.01,
            "b should sit right of a: a.x={} b.x={}",
            ab.x,
            bb.x
        );
        // c doesn't fit (900 > 800) so it wraps to the next row, back at the left.
        assert!(
            cb.y > ab.y + 0.01,
            "c should wrap below: a.y={} c.y={}",
            ab.y,
            cb.y
        );
        assert!(
            (cb.x - ab.x).abs() < 0.01,
            "c should align under a: a.x={} c.x={}",
            ab.x,
            cb.x
        );
        // The wrapper grew to contain both float rows (2 × 50).
        assert!(wb.height >= 100.0 - 0.01, "wrapper height = {}", wb.height);
    }

    #[test]
    fn clear_drops_block_below_floats() {
        // A float:left followed by a clear:left block: the cleared block starts below the float.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let f = doc.append_element(wrap, "div");
        let cleared = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(wrap, block_style(true));
        styles.insert(
            f,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                float: style::Float::Left,
                width: Some(100.0),
                height: Some(80.0),
                ..Default::default()
            },
        );
        styles.insert(
            cleared,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                clear: style::Clear::Left,
                height: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let fb = find_box(&root_box, &|x| x.node == Some(f))
            .unwrap()
            .dimensions
            .content;
        let cb = find_box(&root_box, &|x| x.node == Some(cleared))
            .unwrap()
            .dimensions
            .content;
        // The cleared block sits at or below the float's bottom edge.
        assert!(
            cb.y >= fb.y + fb.height - 0.01,
            "cleared.y={} float.bottom={}",
            cb.y,
            fb.y + fb.height
        );
    }

    #[test]
    fn text_wraps_to_multiple_lines_at_narrow_width() {
        // A paragraph with several words; narrow width forces wrapping.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("one two three four five six".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true)); // font_size 16 default

        // word "three" = 5 chars * 16 * 0.6 = 48px. Width 60 fits ~one word per line.
        let root_box = layout_document(&doc, &styles, 60.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let lines = count_boxes(pbox, &|x| matches!(x.content, BoxContent::Text(_)));
        assert!(lines > 1, "expected multiple wrapped lines, got {lines}");
        // Total height = lines * line_height(16) = lines * 20.8.
        let expected_h = lines as f32 * 16.0 * 1.3;
        assert!((pbox.dimensions.content.height - expected_h).abs() < 0.01);
    }

    #[test]
    fn inline_anchor_text_box_carries_node() {
        // p > "foo " <a>"bar baz qux"</a> " end"  — the <a> wraps; its emitted line Text box(es)
        // must carry the <a>'s text node id so clicks map back to the link.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("foo ".into()));
        let a = doc.append_element(p, "a");
        let a_text = doc.append_child(a, dom::NodeData::Text("bar baz qux".into()));
        doc.append_child(p, dom::NodeData::Text(" end".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        // <a> is inline by default.
        styles.insert(a, style::ComputedStyle::default());

        // Narrow width forces wrapping so we exercise multi-line runs.
        let root_box = layout_document(&doc, &styles, 80.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();

        // Some emitted Text box must carry the <a>'s text node.
        let carries_a_text = count_boxes(pbox, &|x| {
            matches!(x.content, BoxContent::Text(_)) && x.node == Some(a_text)
        });
        assert!(
            carries_a_text >= 1,
            "expected at least one Text box carrying the <a>'s text node id"
        );

        // The <a>'s text boxes only contain the anchor's words (no "foo"/"end" leakage).
        for tb in collect_text_boxes(pbox) {
            if tb.node == Some(a_text) {
                if let BoxContent::Text(t) = &tb.content {
                    for w in t.split_whitespace() {
                        assert!(
                            ["bar", "baz", "qux"].contains(&w),
                            "anchor text run leaked non-anchor word: {w}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vertical_align_sub_super_offsets_the_run() {
        // p > "base" <sup>"hi"</sup> <sub>"lo"</sub> — the superscript run sits above the base
        // run's y, the subscript run below it.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("base".into()));
        let sup = doc.append_element(p, "sup");
        let sup_text = doc.append_child(sup, dom::NodeData::Text("hi".into()));
        let sub = doc.append_element(p, "sub");
        let sub_text = doc.append_child(sub, dom::NodeData::Text("lo".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        let sup_cs = style::ComputedStyle {
            vertical_align: style::VerticalAlign::Super,
            ..Default::default()
        };
        let sub_cs = style::ComputedStyle {
            vertical_align: style::VerticalAlign::Sub,
            ..Default::default()
        };
        styles.insert(sup, sup_cs);
        styles.insert(sub, sub_cs);

        let root_box = layout_document(&doc, &styles, 400.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();

        let y_of = |node: dom::NodeId| -> f32 {
            find_box(pbox, &|x| {
                matches!(x.content, BoxContent::Text(_)) && x.node == Some(node)
            })
            .unwrap()
            .dimensions
            .content
            .y
        };
        let base_y = find_box(
            pbox,
            &|x| matches!(&x.content, BoxContent::Text(t) if t == "base"),
        )
        .unwrap()
        .dimensions
        .content
        .y;
        let sup_y = y_of(sup_text);
        let sub_y = y_of(sub_text);
        assert!(
            sup_y < base_y,
            "sup run ({sup_y}) should sit above base ({base_y})"
        );
        assert!(
            sub_y > base_y,
            "sub run ({sub_y}) should sit below base ({base_y})"
        );
    }

    /// Collect references to all `Text` boxes in a subtree (DFS).
    fn collect_text_boxes(b: &LayoutBox) -> Vec<&LayoutBox> {
        let mut out = Vec::new();
        fn go<'a>(b: &'a LayoutBox, out: &mut Vec<&'a LayoutBox>) {
            if matches!(b.content, BoxContent::Text(_)) {
                out.push(b);
            }
            for c in &b.children {
                go(c, out);
            }
        }
        go(b, &mut out);
        out
    }

    #[test]
    fn display_none_produces_no_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let hidden = doc.append_element(body, "div");
        let shown = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            hidden,
            style::ComputedStyle {
                display_block: true,
                display_none: true,
                ..Default::default()
            },
        );
        styles.insert(shown, block_style(true));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(find_box(&root_box, &|x| x.node == Some(hidden)).is_none());
        assert!(find_box(&root_box, &|x| x.node == Some(shown)).is_some());
    }

    #[test]
    fn anonymous_box_wraps_mixed_inline_among_blocks() {
        // body > [ text, div ] : the leading text must be wrapped in an anonymous block.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        doc.append_child(body, dom::NodeData::Text("hello".into()));
        let d = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(d, block_style(true));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let anon = count_boxes(&root_box, &|x| matches!(x.content, BoxContent::Anonymous));
        assert_eq!(anon, 1);
    }

    #[test]
    fn deeply_nested_does_not_panic() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let mut styles = HashMap::new();
        let mut parent = doc.append_element(root, "body");
        styles.insert(parent, block_style(true));
        // A few hundred levels of nesting (more than any reasonable page) on a normal stack.
        for _ in 0..400 {
            let child = doc.append_element(parent, "div");
            styles.insert(child, block_style(true));
            parent = child;
        }
        doc.append_child(parent, dom::NodeData::Text("deep".into()));
        let _ = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
    }

    #[test]
    fn border_box_expands_content_by_padding_and_border() {
        let d = Dimensions {
            content: Rect {
                x: 10.0,
                y: 10.0,
                width: 100.0,
                height: 20.0,
            },
            padding: Edges {
                top: 5.0,
                right: 5.0,
                bottom: 5.0,
                left: 5.0,
            },
            border: Edges {
                top: 2.0,
                right: 2.0,
                bottom: 2.0,
                left: 2.0,
            },
            margin: Edges::default(),
        };
        let b = d.border_box();
        assert_eq!(b.x, 3.0);
        assert_eq!(b.width, 100.0 + 14.0);
    }

    // ----------------------------------------------------------------------------------------
    // Flex / positioning / inline-block / grid
    // ----------------------------------------------------------------------------------------

    /// A flex-container style with the given direction.
    fn flex_container(dir: style::FlexDirection) -> style::ComputedStyle {
        style::ComputedStyle {
            display: style::Display::Flex,
            display_block: true,
            flex_direction: dir,
            ..Default::default()
        }
    }

    #[test]
    fn flex_row_space_between_anchors_first_and_last() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                justify_content: style::JustifyContent::SpaceBetween,
                ..Default::default()
            },
        );
        let item = |w: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            width: Some(w),
            height: Some(20.0),
            ..Default::default()
        };
        styles.insert(a, item(50.0));
        styles.insert(b, item(50.0));
        styles.insert(d, item(50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let dbox = find_box(&root_box, &|x| x.node == Some(d)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        // First item at the container's left content edge.
        assert!((abox.dimensions.content.x - cbox.dimensions.content.x).abs() < 0.01);
        // Last item's right edge flush with the container's right content edge.
        let last_right = dbox.dimensions.content.x + dbox.dimensions.content.width;
        let cont_right = cbox.dimensions.content.x + 300.0;
        assert!(
            (last_right - cont_right).abs() < 0.01,
            "last_right={last_right} cont_right={cont_right}"
        );
    }

    #[test]
    fn flex_grow_expands_middle_child() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                ..Default::default()
            },
        );
        let fixed = style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            width: Some(50.0),
            ..Default::default()
        };
        styles.insert(a, fixed.clone());
        styles.insert(
            b,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                flex_grow: 1.0,
                flex_basis: Some(0.0),
                ..Default::default()
            },
        );
        styles.insert(d, fixed);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // free = 300 - 50 - 50 - 0(basis) = 200, all goes to b.
        assert!(
            (bbox.dimensions.content.width - 200.0).abs() < 0.01,
            "got {}",
            bbox.dimensions.content.width
        );
    }

    #[test]
    fn flex_column_stacks_vertically() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(c, flex_container(style::FlexDirection::Column));
        let item = |h: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            height: Some(h),
            width: Some(40.0),
            ..Default::default()
        };
        styles.insert(a, item(30.0));
        styles.insert(b, item(50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        assert!(bbox.dimensions.content.y > abox.dimensions.content.y);
        // b stacks directly below a (a height 30).
        assert!((bbox.dimensions.content.y - (abox.dimensions.content.y + 30.0)).abs() < 0.01);
    }

    #[test]
    fn flex_align_items_center_centers_cross_axis() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                width: Some(300.0),
                height: Some(100.0),
                align_items: style::AlignItems::Center,
                ..Default::default()
            },
        );
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(50.0),
                height: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        // Centered: a.y should be container.y + (100 - 20)/2 = +40.
        let expected_y = cbox.dimensions.content.y + 40.0;
        assert!(
            (abox.dimensions.content.y - expected_y).abs() < 0.01,
            "a.y={} expected {}",
            abox.dimensions.content.y,
            expected_y
        );
    }

    #[test]
    fn absolute_child_offsets_from_positioned_parent_padding_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            parent,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(400.0),
                height: Some(300.0),
                padding: style::Edges::all(10.0),
                ..Default::default()
            },
        );
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(50.0),
                height: Some(50.0),
                top: Some(10.0),
                left: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(parent)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let pad = pbox.dimensions.padding_box();
        // Child border-box origin = parent padding-box + (left, top).
        let cb = cbox.dimensions.border_box();
        assert!(
            (cb.x - (pad.x + 20.0)).abs() < 0.01,
            "cb.x={} pad.x={}",
            cb.x,
            pad.x
        );
        assert!(
            (cb.y - (pad.y + 10.0)).abs() < 0.01,
            "cb.y={} pad.y={}",
            cb.y,
            pad.y
        );
    }

    #[test]
    fn absolute_flex_child_static_position_follows_justify_and_align() {
        // An abspos child of a flex container with no insets takes its static position from the
        // container's justify-content (main) + the child's align-self (cross): both `center` → the
        // child's border box is centered in the container.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let item = doc.append_element(flex, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                position: style::Position::Relative,
                width: Some(800.0),
                height: Some(600.0),
                justify_content: style::JustifyContent::Center,
                ..Default::default()
            },
        );
        styles.insert(
            item,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(140.0),
                height: Some(60.0),
                align_self: style::AlignSelf::Center,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(item)).unwrap();
        let bb = ibox.dimensions.border_box();
        assert!((bb.x - 330.0).abs() < 0.5, "x={} (want 330)", bb.x); // (800-140)/2
        assert!((bb.y - 270.0).abs() < 0.5, "y={} (want 270)", bb.y); // (600-60)/2
    }

    #[test]
    fn fieldset_legend_offsets_content_into_the_border() {
        // A `<legend>` is laid out in the fieldset's block-start border; the following content is
        // pushed down by however much the legend exceeds the border (legend 20 over border 10 → +10).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let fs = doc.append_element(body, "fieldset");
        let legend = doc.append_element(fs, "legend");
        let content = doc.append_element(fs, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            fs,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(200.0),
                border: style::Edges::all(10.0),
                ..Default::default()
            },
        );
        styles.insert(
            legend,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(20.0),
                ..Default::default()
            },
        );
        styles.insert(
            content,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let fbox = find_box(&root_box, &|x| x.node == Some(fs)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(content)).unwrap();
        // Content border-box top = fieldset content-box top (border 10) + (legend 20 − border 10) = +20.
        let want = fbox.dimensions.content.y + 10.0;
        assert!(
            (cbox.dimensions.border_box().y - want).abs() < 0.5,
            "content y={} (want {want})",
            cbox.dimensions.border_box().y
        );
    }

    #[test]
    fn scroll_container_flex_baseline_clamps_to_border_box() {
        // A baseline-aligned scroll-container flex item (explicit height) whose content is pushed far
        // out of view by a negative margin must not drag its baseline outside its border box: the
        // clamp keeps it bounded, so the item's own offset stays small instead of ~220px down.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let a = doc.append_element(flex, "div");
        let b = doc.append_element(flex, "div");
        let bchild = doc.append_element(b, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                align_items: style::AlignItems::Baseline,
                width: Some(300.0),
                height: Some(300.0),
                ..Default::default()
            },
        );
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                ..Default::default()
            },
        );
        styles.insert(
            b,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(50.0),
                overflow_scrollport: true, // overflow: hidden → scroll container
                ..Default::default()
            },
        );
        styles.insert(
            bchild,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(10.0),
                margin: style::Edges {
                    top: -200.0,
                    right: 0.0,
                    bottom: 0.0,
                    left: 0.0,
                },
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // Without the clamp B's baseline would be ~ -190, dragging B's box ~220px down to align.
        assert!(
            bbox.dimensions.border_box().y < 100.0,
            "B offset {} should be clamped near the top",
            bbox.dimensions.border_box().y
        );
    }

    #[test]
    fn vertical_writing_mode_flex_item_cross_height_is_inline_extent() {
        // A vertical-lr flex item in a row container: its physical height (the flex cross extent) is
        // the inline size — the longest line's inline-block extent — not zero. Two 10px inline-block
        // spans split by <br> are two 1-span lines, so the height is 10 (one span), not 0 or 20.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let item = doc.append_element(flex, "div");
        let s1 = doc.append_element(item, "span");
        let _br = doc.append_element(item, "br");
        let s2 = doc.append_element(item, "span");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                align_items: style::AlignItems::Baseline,
                width: Some(200.0),
                height: Some(100.0),
                ..Default::default()
            },
        );
        styles.insert(
            item,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                writing_mode: style::WritingMode::VerticalLr,
                ..Default::default()
            },
        );
        styles.insert(_br, style::ComputedStyle::default());
        let span = style::ComputedStyle {
            display: style::Display::InlineBlock,
            width: Some(10.0),
            height: Some(10.0),
            ..Default::default()
        };
        styles.insert(s1, span.clone());
        styles.insert(s2, span);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(item)).unwrap();
        assert!(
            (ibox.dimensions.border_box().height - 10.0).abs() < 0.5,
            "vertical item cross height {} (want 10)",
            ibox.dimensions.border_box().height
        );
    }

    #[test]
    fn vertical_writing_mode_flex_row_main_axis_is_vertical() {
        // A `flex-direction: row` container in a vertical writing mode has its main axis VERTICAL
        // (the inline axis follows the writing mode), so its items stack top-to-bottom, not side by
        // side. Two 40x20 items therefore occupy distinct y bands.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let a = doc.append_element(flex, "div");
        let b = doc.append_element(flex, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                writing_mode: style::WritingMode::VerticalRl,
                width: Some(300.0),
                height: Some(300.0),
                ..Default::default()
            },
        );
        let item = style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            width: Some(40.0),
            height: Some(20.0),
            ..Default::default()
        };
        styles.insert(a, item.clone());
        styles.insert(b, item);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // Main axis is vertical → b stacks below a (distinct y), not to its side.
        assert!(
            bbox.dimensions.content.y >= abox.dimensions.content.y + 20.0,
            "items should stack vertically: a.y={} b.y={}",
            abox.dimensions.content.y,
            bbox.dimensions.content.y
        );
    }

    #[test]
    fn parallel_vertical_flex_items_align_by_central_baseline() {
        // In a vertical-writing-mode flex container, a vertical item is parallel to the container, so
        // baseline alignment uses its CENTRAL baseline (middle of the margin box). Two vertical items
        // of height 20 (central 10) and 40 (central 20) align centers → the shorter shifts +10 along
        // the (vertical) cross axis.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let a = doc.append_element(flex, "div");
        let b = doc.append_element(flex, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                flex_direction: style::FlexDirection::Column,
                writing_mode: style::WritingMode::VerticalRl,
                align_items: style::AlignItems::Baseline,
                width: Some(300.0),
                height: Some(300.0),
                ..Default::default()
            },
        );
        let mut item = |h: f32| style::ComputedStyle {
            display: style::Display::Block,
            display_block: true,
            writing_mode: style::WritingMode::VerticalRl,
            width: Some(10.0),
            height: Some(h),
            ..Default::default()
        };
        styles.insert(a, item(20.0));
        styles.insert(b, item(40.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ay = find_box(&root_box, &|x| x.node == Some(a))
            .unwrap()
            .dimensions
            .content
            .y;
        let by = find_box(&root_box, &|x| x.node == Some(b))
            .unwrap()
            .dimensions
            .content
            .y;
        // a's center (10) meets b's center (20): a shifts +10 down relative to b.
        assert!(
            (ay - (by + 10.0)).abs() < 0.5,
            "a.y={ay} b.y={by} (expected a = b + 10)"
        );
    }

    #[test]
    fn line_clamp_last_baseline_uses_nth_line() {
        // A `-webkit-line-clamp: 3` flex item's LAST baseline is its 3rd line's baseline, not its
        // true final line. Five stacked 20px lines → 3rd line bottom = 60 (not the 5th's 100), so a
        // baseline-aligned reference item (baseline 30) sits 30px below the clamp box's line top.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let refd = doc.append_element(flex, "div");
        let clamp = doc.append_element(flex, "div");
        let lines: Vec<_> = (0..5).map(|_| doc.append_element(clamp, "div")).collect();

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                align_items: style::AlignItems::LastBaseline,
                width: Some(300.0),
                height: Some(300.0),
                ..Default::default()
            },
        );
        styles.insert(
            refd,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                ..Default::default()
            },
        );
        styles.insert(
            clamp,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                line_clamp: Some(3),
                ..Default::default()
            },
        );
        for &l in &lines {
            styles.insert(
                l,
                style::ComputedStyle {
                    display: style::Display::Block,
                    display_block: true,
                    height: Some(20.0),
                    ..Default::default()
                },
            );
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ry = find_box(&root_box, &|x| x.node == Some(refd))
            .unwrap()
            .dimensions
            .content
            .y;
        let cy = find_box(&root_box, &|x| x.node == Some(clamp))
            .unwrap()
            .dimensions
            .content
            .y;
        // ref baseline (30) aligns to clamp's 3rd-line baseline (60): ref.y = clamp.y + 30.
        assert!(
            (ry - (cy + 30.0)).abs() < 0.5,
            "ref.y={ry} clamp.y={cy} (expected ref = clamp + 30 from 3rd-line clamp)"
        );
    }

    #[test]
    fn multicol_places_children_in_side_by_side_columns() {
        // A `column-count: 2` box with two children separated by a column break lays them out side by
        // side (distinct x, same top), and its height is the tallest column (50), not the stacked sum.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let mc = doc.append_element(body, "div");
        let a = doc.append_element(mc, "div");
        let b = doc.append_element(mc, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            mc,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(200.0),
                column_count: Some(2),
                ..Default::default()
            },
        );
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                ..Default::default()
            },
        );
        styles.insert(
            b,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(50.0),
                break_before_column: true, // forces the second column
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let mcb = find_box(&root_box, &|x| x.node == Some(mc)).unwrap();
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        assert!(
            bbox.dimensions.content.x > abox.dimensions.content.x + 50.0,
            "b should be in the next column: a.x={} b.x={}",
            abox.dimensions.content.x,
            bbox.dimensions.content.x
        );
        assert!(
            (abox.dimensions.content.y - bbox.dimensions.content.y).abs() < 0.5,
            "columns share a top: a.y={} b.y={}",
            abox.dimensions.content.y,
            bbox.dimensions.content.y
        );
        assert!(
            (mcb.dimensions.content.height - 50.0).abs() < 0.5,
            "multicol height = tallest column (50), got {}",
            mcb.dimensions.content.height
        );
    }

    #[test]
    fn flex_item_with_only_inline_block_gets_inline_block_cross_height() {
        // A row flex item whose only content is an inline-block (height 20, no text) takes that as its
        // cross size, not 0 — so a column flex of such items can actually wrap on its block-size.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let flex = doc.append_element(body, "div");
        let item = doc.append_element(flex, "div");
        let span = doc.append_element(item, "span");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            flex,
            style::ComputedStyle {
                display: style::Display::Flex,
                width: Some(200.0),
                ..Default::default()
            },
        );
        styles.insert(
            item,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                ..Default::default()
            },
        );
        styles.insert(
            span,
            style::ComputedStyle {
                display: style::Display::InlineBlock,
                width: Some(20.0),
                height: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(item)).unwrap();
        assert!(
            ibox.dimensions.content.height >= 20.0 - 0.5,
            "flex item cross height {} should be >= the inline-block's 20",
            ibox.dimensions.content.height
        );
    }

    #[test]
    fn percentage_height_resolves_against_definite_parent() {
        // A `height: 100%` block fills a parent with a definite (explicit) height — the report's
        // progress-bar fill.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let track = doc.append_element(body, "div");
        let fill = doc.append_element(track, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            track,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(200.0),
                height: Some(9.0),
                ..Default::default()
            },
        );
        styles.insert(
            fill,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width_pct: Some(0.5),
                height_pct: Some(1.0),
                ..Default::default()
            },
        );
        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let f = find_box(&root_box, &|x| x.node == Some(fill)).unwrap();
        assert!(
            (f.dimensions.content.height - 9.0).abs() < 0.5,
            "fill height {} (want 9 = 100% of the 9px track)",
            f.dimensions.content.height
        );
    }

    #[test]
    fn percentage_width_table_fills_container() {
        // A `width: 100%` table fills its container rather than shrinking to its content width.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let table = doc.append_element(wrap, "table");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            wrap,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(400.0),
                ..Default::default()
            },
        );
        styles.insert(
            table,
            style::ComputedStyle {
                display: style::Display::Table,
                width_pct: Some(1.0),
                ..Default::default()
            },
        );
        let _ = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let t = find_box(&root_box, &|x| x.node == Some(table)).unwrap();
        assert!(
            t.dimensions.content.width >= 400.0 - 1.0,
            "table width {} should fill the 400px container",
            t.dimensions.content.width
        );
    }

    #[test]
    fn fixed_child_anchors_to_viewport() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let child = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(wrap, block_style(true));
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Fixed,
                width: Some(40.0),
                height: Some(40.0),
                top: Some(10.0),
                left: Some(15.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let cb = cbox.dimensions.border_box();
        // Anchored to viewport (0,0): border-box at (15, 10).
        assert!((cb.x - 15.0).abs() < 0.01, "cb.x={}", cb.x);
        assert!((cb.y - 10.0).abs() < 0.01, "cb.y={}", cb.y);
    }

    #[test]
    fn absolute_right_top_shrinks_and_anchors_top_right() {
        // .badge (relative, auto width, height 60, padding 16) contains
        // .corner (absolute, top:6 right:6, padding:4, text "HI").
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let badge = doc.append_element(body, "div");
        let corner = doc.append_element(badge, "div");
        doc.append_child(corner, dom::NodeData::Text("HI".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            badge,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                height: Some(60.0),
                padding: style::Edges::all(16.0),
                ..Default::default()
            },
        );
        styles.insert(
            corner,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                top: Some(6.0),
                right: Some(6.0),
                padding: style::Edges::all(4.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(badge)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(corner)).unwrap();
        let pad = pbox.dimensions.padding_box();
        let bb = cbox.dimensions.border_box();

        // Shrink-to-fit: NOT full width. "HI" = 2*16*0.6 = 19.2 content + 8 padding = 27.2 border box.
        assert!(
            bb.width < pad.width,
            "corner border-box width {} should be < parent padding box {}",
            bb.width,
            pad.width
        );
        assert!(
            (bb.width - 27.2).abs() < 0.5,
            "corner border-box width = {}",
            bb.width
        );

        // Right edge anchored 6px from the parent's padding-box right edge.
        let right_gap = (pad.x + pad.width) - (bb.x + bb.width);
        assert!(
            (right_gap - 6.0).abs() < 0.01,
            "right gap = {} (bb.x={} bb.w={})",
            right_gap,
            bb.x,
            bb.width
        );
        // Top edge anchored 6px from the parent's padding-box top edge.
        assert!(
            (bb.y - (pad.y + 6.0)).abs() < 0.01,
            "bb.y={} pad.y={}",
            bb.y,
            pad.y
        );
    }

    #[test]
    fn absolute_percentage_insets_resolve_against_containing_block() {
        // Regression (wikipedia.org central language ring): an absolutely-positioned child with
        // percentage `top`/`left` must anchor at that fraction of the positioned ancestor's
        // padding box — not collapse to its top-left because the percentage was dropped.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let cb = doc.append_element(body, "div");
        let item = doc.append_element(cb, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            cb,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(400.0),
                height: Some(200.0),
                ..Default::default()
            },
        );
        styles.insert(
            item,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(50.0),
                top_spec: style::InsetValue::Percent(20.0),
                left_spec: style::InsetValue::Percent(75.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(cb)).unwrap();
        let ibox = find_box(&root_box, &|x| x.node == Some(item)).unwrap();
        let pad = pbox.dimensions.padding_box();
        let c = ibox.dimensions.content;

        // top: 20% of 200 = 40px below the padding-box top.
        assert!(
            (c.y - (pad.y + 40.0)).abs() < 0.01,
            "c.y={} pad.y={}",
            c.y,
            pad.y
        );
        // left: 75% of 400 = 300px right of the padding-box left.
        assert!(
            (c.x - (pad.x + 300.0)).abs() < 0.01,
            "c.x={} pad.x={}",
            c.x,
            pad.x
        );
    }

    #[test]
    fn relative_percentage_offset_resolves_against_containing_block() {
        // `position: relative` with a percentage offset shifts by that fraction of the containing
        // block (width for left, height for top).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let outer = doc.append_element(body, "div");
        let inner = doc.append_element(outer, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            outer,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                width: Some(200.0),
                height: Some(100.0),
                ..Default::default()
            },
        );
        styles.insert(
            inner,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                height: Some(10.0),
                left_spec: style::InsetValue::Percent(10.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(inner)).unwrap();
        // left: 10% of the 200px-wide containing block = 20px to the right of the normal-flow x.
        assert!(
            (ibox.dimensions.content.x - 20.0).abs() < 0.01,
            "x={}",
            ibox.dimensions.content.x
        );
    }

    #[test]
    fn absolute_bottom_anchors_near_parent_bottom() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let parent = doc.append_element(body, "div");
        let child = doc.append_element(parent, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            parent,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(200.0),
                height: Some(100.0),
                ..Default::default()
            },
        );
        styles.insert(
            child,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Absolute,
                width: Some(30.0),
                height: Some(20.0),
                bottom: Some(8.0),
                left: Some(5.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(parent)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(child)).unwrap();
        let pad = pbox.dimensions.padding_box();
        let bb = cbox.dimensions.border_box();
        // Border-box bottom edge sits 8px above the parent padding-box bottom edge.
        let bottom_gap = (pad.y + pad.height) - (bb.y + bb.height);
        assert!(
            (bottom_gap - 8.0).abs() < 0.01,
            "bottom gap = {}",
            bottom_gap
        );
        assert!(
            (bb.x - (pad.x + 5.0)).abs() < 0.01,
            "bb.x={} pad.x={}",
            bb.x,
            pad.x
        );
    }

    #[test]
    fn relative_offsets_without_affecting_siblings() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let a = doc.append_element(body, "div");
        let b = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(30.0),
                position: style::Position::Relative,
                left: Some(25.0),
                top: Some(5.0),
                ..Default::default()
            },
        );
        styles.insert(
            b,
            style::ComputedStyle {
                display_block: true,
                height: Some(40.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        // a shifted by (25, 5).
        assert!((abox.dimensions.content.x - 25.0).abs() < 0.01);
        assert!((abox.dimensions.content.y - 5.0).abs() < 0.01);
        // b is unaffected: stacks below a's in-flow position (y=30), not the shifted one.
        assert!(
            (bbox.dimensions.content.y - 30.0).abs() < 0.01,
            "b.y={}",
            bbox.dimensions.content.y
        );
    }

    #[test]
    fn inline_block_sits_inline_with_intrinsic_width() {
        // body > p > [ "ab", inline-block span("XY"), "cd" ]
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("ab".into()));
        let ib = doc.append_element(p, "span");
        doc.append_child(ib, dom::NodeData::Text("XY".into()));
        doc.append_child(p, dom::NodeData::Text("cd".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        styles.insert(
            ib,
            style::ComputedStyle {
                display: style::Display::InlineBlock,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        // The inline-block becomes an atomic box (node == ib) sitting in the line.
        let ibbox = find_box(&root_box, &|x| x.node == Some(ib)).unwrap();
        // Intrinsic width = "XY" = 2 chars * 16 * 0.6 = 19.2.
        assert!(
            (ibbox.dimensions.content.width - 19.2).abs() < 0.1,
            "ib width = {}",
            ibbox.dimensions.content.width
        );
        // It sits to the right of the leading "ab" word (x > content origin 0).
        assert!(ibbox.dimensions.content.x > 0.0);
    }

    #[test]
    fn grid_three_equal_fr_columns_split_width_in_thirds() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let g = doc.append_element(body, "div");
        let a = doc.append_element(g, "div");
        let b = doc.append_element(g, "div");
        let c = doc.append_element(g, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            g,
            style::ComputedStyle {
                display: style::Display::Grid,
                display_block: true,
                width: Some(300.0),
                grid_template_columns: vec![
                    style::TrackSize::Fr(1.0),
                    style::TrackSize::Fr(1.0),
                    style::TrackSize::Fr(1.0),
                ],
                ..Default::default()
            },
        );
        for &id in &[a, b, c] {
            styles.insert(
                id,
                style::ComputedStyle {
                    display_block: true,
                    height: Some(20.0),
                    ..Default::default()
                },
            );
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        let bbox = find_box(&root_box, &|x| x.node == Some(b)).unwrap();
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        // Each column 100 wide.
        assert!((abox.dimensions.content.width - 100.0).abs() < 0.01);
        assert!((bbox.dimensions.content.width - 100.0).abs() < 0.01);
        // Columns laid out left-to-right at x = 0, 100, 200 (relative to grid origin).
        let gx = find_box(&root_box, &|x| x.node == Some(g))
            .unwrap()
            .dimensions
            .content
            .x;
        assert!((abox.dimensions.content.x - gx).abs() < 0.01);
        assert!((bbox.dimensions.content.x - (gx + 100.0)).abs() < 0.01);
        assert!((cbox.dimensions.content.x - (gx + 200.0)).abs() < 0.01);
    }

    #[test]
    fn grid_align_self_end_positions_item_at_cell_bottom() {
        // An `align-self: end` grid item (height 20) in a 60px row sits at the cell bottom (offset
        // 40), content-sized rather than stretched to fill the cell.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let g = doc.append_element(body, "div");
        let a = doc.append_element(g, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            g,
            style::ComputedStyle {
                display: style::Display::Grid,
                display_block: true,
                width: Some(100.0),
                grid_template_columns: vec![style::TrackSize::Px(50.0)],
                grid_template_rows: vec![style::TrackSize::Px(60.0)],
                ..Default::default()
            },
        );
        styles.insert(
            a,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                height: Some(20.0),
                align_self: style::AlignSelf::FlexEnd,
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let gy = find_box(&root_box, &|x| x.node == Some(g))
            .unwrap()
            .dimensions
            .content
            .y;
        let abox = find_box(&root_box, &|x| x.node == Some(a)).unwrap();
        assert!(
            (abox.dimensions.content.height - 20.0).abs() < 0.5,
            "item should be content-sized (20), got {}",
            abox.dimensions.content.height
        );
        assert!(
            (abox.dimensions.content.y - (gy + 40.0)).abs() < 0.5,
            "item should sit at the cell bottom (gy+40), got {}",
            abox.dimensions.content.y
        );
    }

    #[test]
    fn sibling_after_wrapped_paragraph_clears_its_margin_box() {
        // body > p (multi-line wrapped text) , div (sibling). The sibling's content.y must be
        // >= the paragraph block's margin-box bottom (no vertical overlap). This guards the bug
        // where a block with wrapped inline content under-reported its height.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(
            p,
            dom::NodeData::Text("one two three four five six seven".into()),
        );
        let sib = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true)); // font_size 16 default
        styles.insert(
            sib,
            style::ComputedStyle {
                display_block: true,
                height: Some(10.0),
                ..Default::default()
            },
        );

        // Narrow width forces the paragraph to wrap to several lines.
        let root_box = layout_document(&doc, &styles, 60.0, 600.0, &Stub, &HashMap::new(), None);
        let pbox = find_box(&root_box, &|x| x.node == Some(p)).unwrap();
        let sbox = find_box(&root_box, &|x| x.node == Some(sib)).unwrap();

        let lines = count_boxes(pbox, &|x| matches!(x.content, BoxContent::Text(_)));
        assert!(
            lines > 1,
            "expected the paragraph to wrap, got {lines} line(s)"
        );
        let p_bottom = pbox.dimensions.margin_box().y + pbox.dimensions.margin_box().height;
        assert!(
            sbox.dimensions.content.y >= p_bottom - 0.01,
            "sibling overlaps paragraph: sib.y={} < p margin-box bottom {}",
            sbox.dimensions.content.y,
            p_bottom
        );
    }

    #[test]
    fn image_box_uses_intrinsic_size_when_no_css() {
        // body > img (no CSS width/height) with intrinsic (100, 50).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(img, style::ComputedStyle::default()); // inline by default

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 100.0);
        assert_eq!(ibox.dimensions.content.height, 50.0);
        assert_eq!(ibox.node, Some(img));
    }

    #[test]
    fn absolutely_positioned_image_keeps_intrinsic_size() {
        // Regression (wikipedia.org globe logo): an `position: absolute` <img> must keep its
        // build-time intrinsic size. The out-of-flow path used to size it like a container —
        // width from `intrinsic_width` (0, no children) and height from children (0) — so the
        // logo collapsed to 0×0 and never painted.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let cb = doc.append_element(body, "div");
        let img = doc.append_element(cb, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            cb,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                position: style::Position::Relative,
                width: Some(400.0),
                height: Some(300.0),
                ..Default::default()
            },
        );
        styles.insert(
            img,
            style::ComputedStyle {
                position: style::Position::Absolute,
                top: Some(10.0),
                left: Some(20.0),
                ..Default::default()
            },
        );

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (200.0, 183.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let pbox = find_box(&root_box, &|x| x.node == Some(cb)).unwrap();
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        let pad = pbox.dimensions.padding_box();
        let c = ibox.dimensions.content;
        assert_eq!(
            (c.width, c.height),
            (200.0, 183.0),
            "image must keep intrinsic size"
        );
        // Anchored at top:10 / left:20 from the positioned ancestor's padding box.
        assert!(
            (c.x - (pad.x + 20.0)).abs() < 0.01,
            "c.x={} pad.x={}",
            c.x,
            pad.x
        );
        assert!(
            (c.y - (pad.y + 10.0)).abs() < 0.01,
            "c.y={} pad.y={}",
            c.y,
            pad.y
        );
    }

    #[test]
    fn input_with_value_produces_text_child() {
        // body > input value="hello" → the input box has a Text("hello") child.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        if let dom::NodeData::Element(e) = &mut doc.get_mut(input).data {
            e.attrs.insert("value".to_string(), "hello".to_string());
        }

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            input,
            style::ComputedStyle {
                width: Some(120.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let txt = find_box(
            &root_box,
            &|x| matches!(&x.content, BoxContent::Text(s) if s == "hello"),
        );
        let txt = txt.expect("input value must render as a Text box");
        // The rendered text traces back to the input element.
        assert_eq!(txt.node, Some(input));
    }

    #[test]
    fn image_box_css_width_preserves_intrinsic_aspect_ratio() {
        // CSS width:200, no height, intrinsic 100x50 (2:1) → height 100.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle {
                width: Some(200.0),
                ..Default::default()
            },
        );

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 200.0);
        assert!(
            (ibox.dimensions.content.height - 100.0).abs() < 0.01,
            "aspect-preserved height = {}",
            ibox.dimensions.content.height
        );
    }

    #[test]
    fn image_box_explicit_both_dimensions() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle {
                width: Some(40.0),
                height: Some(30.0),
                ..Default::default()
            },
        );

        // Intrinsic provided but explicit CSS wins.
        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 40.0);
        assert_eq!(ibox.dimensions.content.height, 30.0);
    }

    #[test]
    fn block_image_contributes_height_so_sibling_clears_it() {
        // body > img(display:block, 100x50), div(sibling). Sibling must clear the image.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");
        let sib = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            img,
            style::ComputedStyle {
                display: style::Display::Block,
                display_block: true,
                ..Default::default()
            },
        );
        styles.insert(
            sib,
            style::ComputedStyle {
                display_block: true,
                height: Some(10.0),
                ..Default::default()
            },
        );

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (100.0, 50.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        let sbox = find_box(&root_box, &|x| x.node == Some(sib)).unwrap();
        assert_eq!(ibox.dimensions.content.height, 50.0);
        // Sibling stacks below the image's 50px-tall margin box.
        assert!(
            (sbox.dimensions.content.y - 50.0).abs() < 0.01,
            "sibling y = {} (should clear the 50px image)",
            sbox.dimensions.content.y
        );
    }

    #[test]
    fn inline_image_advances_the_line() {
        // body > p > [ "ab", img(20x10), "cd" ]: the image is atomic inline, to the right of "ab".
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let p = doc.append_element(body, "p");
        doc.append_child(p, dom::NodeData::Text("ab".into()));
        let img = doc.append_element(p, "img");
        doc.append_child(p, dom::NodeData::Text("cd".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(p, block_style(true));
        styles.insert(img, style::ComputedStyle::default());

        let mut intrinsic = HashMap::new();
        intrinsic.insert(img, (20.0, 10.0));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        let ibox = find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).unwrap();
        assert_eq!(ibox.dimensions.content.width, 20.0);
        // Sits to the right of the leading "ab" word.
        assert!(
            ibox.dimensions.content.x > 0.0,
            "image x = {}",
            ibox.dimensions.content.x
        );
    }

    #[test]
    fn image_with_no_size_known_produces_no_box() {
        // No CSS size, no intrinsic entry → nothing to draw, no Image box.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let img = doc.append_element(body, "img");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(img, style::ComputedStyle::default());

        let intrinsic: HashMap<dom::NodeId, (f32, f32)> = HashMap::new();
        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &intrinsic, None);
        assert!(find_box(&root_box, &|x| matches!(x.content, BoxContent::Image(_))).is_none());
    }

    #[test]
    fn flex_column_items_do_not_overlap_and_container_encompasses_them() {
        // A column flex of three items whose heights come from their (wrapped) content, plus a row
        // gap. Items must stack without overlap and the container height must be >= the sum of
        // item heights + gaps.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let c = doc.append_element(body, "div");
        let a = doc.append_element(c, "div");
        let b = doc.append_element(c, "div");
        let d = doc.append_element(c, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            c,
            style::ComputedStyle {
                display: style::Display::Flex,
                display_block: true,
                flex_direction: style::FlexDirection::Column,
                width: Some(200.0),
                row_gap: 8.0,
                ..Default::default()
            },
        );
        // Items have no explicit height; their height is driven by content (one line of text each).
        for &id in &[a, b, d] {
            styles.insert(id, block_style(true));
        }
        doc.append_child(a, dom::NodeData::Text("alpha".into()));
        doc.append_child(b, dom::NodeData::Text("beta".into()));
        doc.append_child(d, dom::NodeData::Text("gamma".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cbox = find_box(&root_box, &|x| x.node == Some(c)).unwrap();
        let boxes: Vec<_> = [a, b, d]
            .iter()
            .map(|&id| find_box(&root_box, &|x| x.node == Some(id)).unwrap())
            .collect();

        // Each item starts at or below the previous item's margin-box bottom.
        let mut sum_h = 0.0f32;
        for i in 0..boxes.len() {
            let mb = boxes[i].dimensions.margin_box();
            sum_h += mb.height;
            if i > 0 {
                let prev = boxes[i - 1].dimensions.margin_box();
                assert!(
                    boxes[i].dimensions.content.y >= prev.y + prev.height - 0.01,
                    "flex column item {i} overlaps the previous: y={} prev-bottom={}",
                    boxes[i].dimensions.content.y,
                    prev.y + prev.height
                );
            }
        }
        // Each item should actually have a non-zero height (the (792,275) zero-height bug).
        for (i, bx) in boxes.iter().enumerate() {
            assert!(
                bx.dimensions.content.height > 0.0,
                "item {i} has zero height"
            );
        }
        // Container height >= sum of item heights + gaps between them.
        let expected_min = sum_h + 8.0 * 2.0;
        assert!(
            cbox.dimensions.content.height >= expected_min - 0.01,
            "container height {} < items+gaps {}",
            cbox.dimensions.content.height,
            expected_min
        );
    }

    /// Set an attribute on an element node (test helper).
    fn set_attr(doc: &mut dom::Document, id: dom::NodeId, name: &str, value: &str) {
        if let dom::NodeData::Element(e) = &mut doc.get_mut(id).data {
            e.attrs.insert(name.to_string(), value.to_string());
        }
    }

    /// Does any text run in the subtree contain `needle`?
    fn has_text(b: &LayoutBox, needle: &str) -> bool {
        find_box(
            b,
            &|x| matches!(&x.content, BoxContent::Text(s) if s.contains(needle)),
        )
        .is_some()
    }

    #[test]
    fn checked_checkbox_renders_checked_indicator() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "checkbox");
        set_attr(&mut doc, input, "checked", "");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(input)).unwrap();
        // The checkbox is a drawn Widget; checked state is carried on the Widget content.
        assert!(
            matches!(
                ibox.content,
                BoxContent::Widget(WidgetKind::Checkbox { checked: true })
            ),
            "expected a checked Checkbox widget, got {:?}",
            ibox.content
        );
        // It has a non-zero box so it paints (and hit-tests).
        assert!(ibox.dimensions.content.width > 0.0 && ibox.dimensions.content.height > 0.0);
    }

    #[test]
    fn unchecked_checkbox_renders_empty_indicator() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "checkbox");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox = find_box(&root_box, &|x| x.node == Some(input)).unwrap();
        assert!(
            matches!(
                ibox.content,
                BoxContent::Widget(WidgetKind::Checkbox { checked: false })
            ),
            "expected an unchecked Checkbox widget, got {:?}",
            ibox.content
        );
    }

    /// Build a `<select>` with the given options. Each option is `(value_attr, text, selected)`;
    /// `value_attr = None` means no `value` attribute. Returns `(doc, select_id, body)`.
    fn build_select(
        options: &[(Option<&str>, &str, bool)],
        select_value: Option<&str>,
    ) -> (dom::Document, dom::NodeId, dom::NodeId) {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let select = doc.append_element(body, "select");
        if let Some(v) = select_value {
            set_attr(&mut doc, select, "value", v);
        }
        for &(val, text, selected) in options {
            let opt = doc.append_element(select, "option");
            if let Some(v) = val {
                set_attr(&mut doc, opt, "value", v);
            }
            if selected {
                set_attr(&mut doc, opt, "selected", "");
            }
            doc.append_child(opt, dom::NodeData::Text(text.to_string()));
        }
        (doc, select, body)
    }

    fn layout_select(doc: &dom::Document, select: dom::NodeId, body: dom::NodeId) -> LayoutBox {
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(select, style::ComputedStyle::default());
        // Style the options + their text so they would lay out if (wrongly) recursed into.
        for &child in &doc.get(select).children {
            styles.insert(child, style::ComputedStyle::default());
        }
        layout_document(doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None)
    }

    #[test]
    fn select_renders_selected_option_as_dropdown() {
        // Three options, the 2nd is `selected`.
        let (doc, select, body) = build_select(
            &[
                (None, "First", false),
                (None, "Second", true),
                (None, "Third", false),
            ],
            None,
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        // Shows the selected label and the dropdown arrow.
        assert!(has_text(sbox, "Second"), "expected selected option label");
        assert!(has_text(sbox, "\u{25BE}"), "expected dropdown arrow ▾");
        // Does NOT show the other options.
        assert!(!has_text(sbox, "First"), "unselected option leaked");
        assert!(!has_text(sbox, "Third"), "unselected option leaked");
    }

    #[test]
    fn select_value_attr_selects_matching_option() {
        // No `selected`; `value` attr matches the 3rd option's value.
        let (doc, select, body) = build_select(
            &[
                (Some("a"), "Apple", false),
                (Some("b"), "Banana", false),
                (Some("c"), "Cherry", false),
            ],
            Some("c"),
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        assert!(
            has_text(sbox, "Cherry"),
            "value=c should select the Cherry option"
        );
        assert!(!has_text(sbox, "Apple"));
        assert!(!has_text(sbox, "Banana"));
    }

    #[test]
    fn select_defaults_to_first_option() {
        // No `selected`, no `value` → first option shows.
        let (doc, select, body) = build_select(
            &[
                (None, "One", false),
                (None, "Two", false),
                (None, "Three", false),
            ],
            None,
        );
        let root_box = layout_select(&doc, select, body);
        let sbox = find_box(&root_box, &|x| x.node == Some(select)).unwrap();
        assert!(has_text(sbox, "One"), "first option should show by default");
        assert!(!has_text(sbox, "Two"));
        assert!(!has_text(sbox, "Three"));
    }

    #[test]
    fn select_options_are_not_separate_inline_boxes() {
        // The <option> DOM subtree must be suppressed: no Text box should carry an option node id,
        // and the unselected options' text must not appear anywhere in the layout tree.
        let (doc, select, body) = build_select(
            &[
                (None, "Alpha", true),
                (None, "Beta", false),
                (None, "Gamma", false),
            ],
            None,
        );
        let option_ids: Vec<dom::NodeId> = doc.get(select).children.clone();
        let root_box = layout_select(&doc, select, body);
        // No box anywhere is owned by an <option> element/text node.
        for opt in option_ids {
            assert!(
                find_box(&root_box, &|x| x.node == Some(opt)).is_none(),
                "an <option> produced its own box (should be suppressed)"
            );
        }
        assert!(
            !has_text(&root_box, "Beta"),
            "unselected option text leaked into layout"
        );
        assert!(
            !has_text(&root_box, "Gamma"),
            "unselected option text leaked into layout"
        );
    }

    #[test]
    fn focused_text_input_shows_caret() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "text");
        set_attr(&mut doc, input, "value", "hi");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        // Focused: the value text is "hi" (no pipe glyph) plus a separate caret bar box. The caret
        // is laid out as a sibling of the value run (both owned by the input), so search the tree.
        let root_box = layout_document(
            &doc,
            &styles,
            800.0,
            600.0,
            &Stub,
            &HashMap::new(),
            Some(input),
        );
        assert!(
            has_text(&root_box, "hi"),
            "focused input should still show its value"
        );
        assert!(
            !has_text(&root_box, "|"),
            "caret must be a bar, not a pipe glyph"
        );
        let caret = find_box(&root_box, &|x| matches!(x.content, BoxContent::Caret))
            .expect("focused input should have a caret bar box");
        assert_eq!(
            caret.node,
            Some(input),
            "caret belongs to the focused input"
        );
        assert!(
            caret.dimensions.content.width > 0.0 && caret.dimensions.content.height > 0.0,
            "caret bar has nonzero size"
        );

        // Not focused: no caret box, no pipe.
        let root_box2 = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(
            has_text(&root_box2, "hi"),
            "unfocused input still shows its value"
        );
        assert!(
            !has_text(&root_box2, "|"),
            "unfocused input must not show a caret"
        );
        assert_eq!(
            count_boxes(&root_box2, &|x| matches!(x.content, BoxContent::Caret)),
            0,
            "unfocused input must not have a caret bar box"
        );
    }

    #[test]
    fn empty_focused_input_shows_caret_not_placeholder() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let input = doc.append_element(body, "input");
        set_attr(&mut doc, input, "type", "text");
        set_attr(&mut doc, input, "placeholder", "Search");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(input, style::ComputedStyle::default());

        // Focused + empty: a caret bar, and the placeholder is hidden (as in real browsers).
        let root_box = layout_document(
            &doc,
            &styles,
            800.0,
            600.0,
            &Stub,
            &HashMap::new(),
            Some(input),
        );
        assert!(
            count_boxes(&root_box, &|x| matches!(x.content, BoxContent::Caret)) >= 1,
            "empty focused input should show a caret bar"
        );
        assert!(
            !has_text(&root_box, "Search"),
            "placeholder must be hidden while a focused field is being edited"
        );

        // Unfocused + empty: the placeholder shows and there's no caret.
        let root_box2 = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let ibox2 = find_box(&root_box2, &|x| x.node == Some(input)).unwrap();
        assert!(
            has_text(ibox2, "Search"),
            "unfocused empty input keeps its placeholder"
        );
        assert_eq!(
            count_boxes(ibox2, &|x| matches!(x.content, BoxContent::Caret)),
            0,
            "unfocused input has no caret"
        );
    }

    // ----- generated content (::before / ::after) -----

    /// A pseudo computed style carrying a content string (inline by default).
    fn pseudo_style(content: &str) -> style::ComputedStyle {
        style::ComputedStyle {
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    /// The first child of the box for `node` whose text equals `s` and its index among children.
    fn child_text_at(b: &LayoutBox) -> Vec<&str> {
        b.children
            .iter()
            .filter_map(|c| match &c.content {
                BoxContent::Text(t) => Some(t.as_str()),
                _ => c.children.first().and_then(|cc| match &cc.content {
                    BoxContent::Text(t) => Some(t.as_str()),
                    _ => None,
                }),
            })
            .collect()
    }

    #[test]
    fn before_text_precedes_real_text() {
        // <div class=x>hi</div> with ::before content "→".
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                before: Some(Box::new(pseudo_style("→"))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        // The ::before "→" text must appear before "hi" in document order.
        let texts = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["→".to_string(), "hi".to_string()]);
    }

    #[test]
    fn after_text_follows_real_text() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                after: Some(Box::new(pseudo_style("world"))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let texts = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        // "hi" and "world" are separate inline words on the same line; ::after comes last.
        assert!(texts.iter().any(|t| t.contains("hi")));
        let joined = texts.join(" ");
        let hi_pos = joined.find("hi").unwrap();
        let world_pos = joined.find("world").unwrap();
        assert!(
            hi_pos < world_pos,
            "::after text must follow the element's own text"
        );
    }

    #[test]
    fn empty_content_emits_no_text_box() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");
        doc.append_child(div, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                after: Some(Box::new(pseudo_style(""))),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let texts: Vec<_> = collect_text_boxes(dbox)
            .iter()
            .filter_map(|b| match &b.content {
                BoxContent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        // Only the real "hi" text — the empty ::after contributes a box but no text child.
        assert_eq!(texts, vec!["hi".to_string()]);
        let _ = child_text_at(dbox);
    }

    #[test]
    fn inline_pseudo_text_carries_its_own_color() {
        // An inline ::before is flattened into text runs; the run carries the pseudo's color.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");

        let mut pseudo = pseudo_style("x");
        pseudo.color = (255, 0, 0);

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                color: (0, 0, 255),
                before: Some(Box::new(pseudo)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let tb = collect_text_boxes(dbox)
            .into_iter()
            .find(|b| matches!(&b.content, BoxContent::Text(t) if t == "x"))
            .expect("::before text box");
        assert_eq!(tb.style.color, (255, 0, 0)); // pseudo red, distinct from element blue
    }

    #[test]
    fn block_pseudo_box_carries_background() {
        // A `display: block` ::before keeps its own box (not flattened), so its background applies.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let div = doc.append_element(body, "div");

        let mut pseudo = pseudo_style("x");
        pseudo.display = style::Display::Block;
        pseudo.display_block = true;
        pseudo.background_color = Some((0, 255, 0));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            div,
            style::ComputedStyle {
                display_block: true,
                before: Some(Box::new(pseudo)),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let dbox = find_box(&root_box, &|x| x.node == Some(div)).unwrap();
        let pseudo_box = dbox
            .children
            .iter()
            .find(|c| c.node.is_none() && matches!(c.content, BoxContent::Block))
            .expect("anonymous ::before block box");
        assert_eq!(pseudo_box.style.background_color, Some((0, 255, 0)));
    }

    // ------------------------------------------------------------------------------------------
    // Table layout
    // ------------------------------------------------------------------------------------------

    /// A computed style with a given `display` value (everything else default).
    fn disp(d: style::Display) -> style::ComputedStyle {
        style::ComputedStyle {
            display: d,
            ..Default::default()
        }
    }

    /// Build a `tr`-of-cells row under `parent`, returning the cell node ids. `cell_tag` is `td`/`th`.
    /// Each cell gets a single text node. `styles` is populated with table-* display values.
    fn build_row(
        doc: &mut dom::Document,
        styles: &mut HashMap<dom::NodeId, style::ComputedStyle>,
        parent: dom::NodeId,
        cell_tag: &str,
        texts: &[&str],
    ) -> Vec<dom::NodeId> {
        let tr = doc.append_element(parent, "tr");
        styles.insert(tr, disp(style::Display::TableRow));
        let mut cells = Vec::new();
        for t in texts {
            let cell = doc.append_element(tr, cell_tag);
            let mut cs = disp(style::Display::TableCell);
            if cell_tag == "th" {
                cs.bold = true;
                cs.text_align = style::TextAlign::Center;
            }
            styles.insert(cell, cs);
            doc.append_child(cell, dom::NodeData::Text((*t).into()));
            cells.push(cell);
        }
        cells
    }

    #[test]
    fn table_3x3_columns_align_rows_share_y() {
        // A 3x3 table: cells in the same column share x + width; cells in the same row share y.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let mut rows: Vec<Vec<dom::NodeId>> = vec![build_row(
            &mut doc,
            &mut styles,
            table,
            "td",
            &["aa", "bbbb", "c"],
        )];
        rows.push(build_row(
            &mut doc,
            &mut styles,
            table,
            "td",
            &["dddddd", "e", "ff"],
        ));
        rows.push(build_row(
            &mut doc,
            &mut styles,
            table,
            "td",
            &["g", "hh", "iii"],
        ));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        let cell_rect = |n: dom::NodeId| {
            find_box(&root_box, &|x| {
                x.node == Some(n) && matches!(x.content, BoxContent::Block)
            })
            .unwrap()
            .dimensions
            .border_box()
        };

        // Columns: x + width match down each column.
        for col in 0..3 {
            let r0 = cell_rect(rows[0][col]);
            for row in 1..3 {
                let r = cell_rect(rows[row][col]);
                assert!(
                    (r.x - r0.x).abs() < 0.01,
                    "col {col} x mismatch: {} vs {}",
                    r.x,
                    r0.x
                );
                assert!(
                    (r.width - r0.width).abs() < 0.01,
                    "col {col} width mismatch"
                );
            }
        }
        // Rows: y matches across each row.
        for row in 0..3 {
            let r0 = cell_rect(rows[row][0]);
            for col in 1..3 {
                let r = cell_rect(rows[row][col]);
                assert!(
                    (r.y - r0.y).abs() < 0.01,
                    "row {row} y mismatch: {} vs {}",
                    r.y,
                    r0.y
                );
            }
        }
        // Column 0's width is driven by its widest cell ("dddddd").
        let c00 = cell_rect(rows[0][0]);
        let c01 = cell_rect(rows[0][1]);
        assert!(c00.x < c01.x, "column 0 sits left of column 1");
    }

    #[test]
    fn table_thead_tbody_renders_all_cells() {
        // A table whose rows live inside <thead>/<tbody> must render every cell (regression: row
        // groups used to be inline and their rows vanished).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let thead = doc.append_element(table, "thead");
        styles.insert(thead, disp(style::Display::TableHeaderGroup));
        let h = build_row(&mut doc, &mut styles, thead, "th", &["H1", "H2"]);

        let tbody = doc.append_element(table, "tbody");
        styles.insert(tbody, disp(style::Display::TableRowGroup));
        let r1 = build_row(&mut doc, &mut styles, tbody, "td", &["a", "b"]);
        let r2 = build_row(&mut doc, &mut styles, tbody, "td", &["c", "d"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        for n in h.iter().chain(r1.iter()).chain(r2.iter()) {
            assert!(
                find_box(&root_box, &|x| x.node == Some(*n)).is_some(),
                "cell {n:?} should produce a box"
            );
        }
        // Header sits above the body rows.
        let hy = find_box(&root_box, &|x| x.node == Some(h[0]))
            .unwrap()
            .dimensions
            .content
            .y;
        let by = find_box(&root_box, &|x| x.node == Some(r1[0]))
            .unwrap()
            .dimensions
            .content
            .y;
        assert!(hy < by, "thead row ({hy}) above tbody row ({by})");
    }

    #[test]
    fn table_th_is_bold_and_centered() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));
        let h = build_row(&mut doc, &mut styles, table, "th", &["Header"]);
        // Give the cell an explicit width wider than its text so centering has room to show.
        if let Some(cs) = styles.get_mut(&h[0]) {
            cs.width = Some(200.0);
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cell = find_box(&root_box, &|x| {
            x.node == Some(h[0]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap();
        // The cell's text run is bold.
        let text = find_box(cell, &|x| matches!(x.content, BoxContent::Text(_))).unwrap();
        assert!(text.style.bold, "th text should be bold");
        // Centered: the text run is horizontally centered within the cell content box.
        let cell_box = cell.dimensions.content;
        let tr = text.dimensions.content;
        let left_gap = tr.x - cell_box.x;
        let right_gap = (cell_box.x + cell_box.width) - (tr.x + tr.width);
        assert!(
            left_gap > 0.5,
            "expected left padding from centering, got {left_gap}"
        );
        assert!(
            (left_gap - right_gap).abs() < 1.0,
            "text not centered: L={left_gap} R={right_gap}"
        );
    }

    #[test]
    fn table_colspan_spans_two_columns() {
        // Row 1: two cells (define two columns). Row 2: one cell with colspan=2 spanning both.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let r1 = build_row(&mut doc, &mut styles, table, "td", &["aaaa", "bbbb"]);

        let tr2 = doc.append_element(table, "tr");
        styles.insert(tr2, disp(style::Display::TableRow));
        let wide = doc.append_element(tr2, "td");
        let mut wcs = disp(style::Display::TableCell);
        wcs.bold = false;
        styles.insert(wide, wcs);
        if let dom::NodeData::Element(el) = &mut doc.get_mut(wide).data {
            el.attrs.insert("colspan".into(), "2".into());
        }
        doc.append_child(wide, dom::NodeData::Text("spanning".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);

        let c0 = find_box(&root_box, &|x| {
            x.node == Some(r1[0]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap()
        .dimensions
        .border_box();
        let c1 = find_box(&root_box, &|x| {
            x.node == Some(r1[1]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap()
        .dimensions
        .border_box();
        let span = find_box(&root_box, &|x| {
            x.node == Some(wide) && matches!(x.content, BoxContent::Block)
        })
        .unwrap()
        .dimensions
        .border_box();

        // The spanning cell's border box covers both columns: from col0.x to col1's right edge.
        assert!(
            (span.x - c0.x).abs() < 0.5,
            "spanning cell starts at col0 x"
        );
        let two_col_w = c0.width + c1.width;
        assert!(
            (span.width - two_col_w).abs() < 0.5,
            "colspan=2 width {} != {}",
            span.width,
            two_col_w
        );
    }

    #[test]
    fn table_caption_sits_above_first_row() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let caption = doc.append_element(table, "caption");
        styles.insert(caption, disp(style::Display::TableCaption));
        doc.append_child(caption, dom::NodeData::Text("My Caption".into()));

        let r1 = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cap_box = find_box(&root_box, &|x| x.node == Some(caption)).unwrap();
        let cell_box = find_box(&root_box, &|x| {
            x.node == Some(r1[0]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap();
        assert!(
            cap_box.dimensions.content.y < cell_box.dimensions.content.y,
            "caption ({}) should sit above the first cell ({})",
            cap_box.dimensions.content.y,
            cell_box.dimensions.content.y
        );
    }

    #[test]
    fn table_caption_side_bottom_sits_below_rows() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let r1 = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);
        let caption = doc.append_element(table, "caption");
        styles.insert(
            caption,
            style::ComputedStyle {
                display: style::Display::TableCaption,
                caption_side_bottom: true,
                ..Default::default()
            },
        );
        doc.append_child(caption, dom::NodeData::Text("Bottom".into()));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cap_y = find_box(&root_box, &|x| x.node == Some(caption))
            .unwrap()
            .dimensions
            .content
            .y;
        let cell_y = find_box(&root_box, &|x| {
            x.node == Some(r1[0]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap()
        .dimensions
        .content
        .y;
        assert!(
            cap_y > cell_y,
            "caption-side:bottom caption ({cap_y}) should sit below the first cell ({cell_y})"
        );
    }

    #[test]
    fn table_cell_content_wraps_within_column_width() {
        // A narrow fixed-width cell forces its long text to wrap onto multiple lines, making the
        // cell (and its row) taller than a single line.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut tcs = disp(style::Display::Table);
        tcs.width = Some(60.0); // narrow table → narrow column → wrapping
        styles.insert(table, tcs);

        let tr = doc.append_element(table, "tr");
        styles.insert(tr, disp(style::Display::TableRow));
        let cell = doc.append_element(tr, "td");
        styles.insert(cell, disp(style::Display::TableCell));
        doc.append_child(
            cell,
            dom::NodeData::Text("one two three four five six".into()),
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let cell_box = find_box(&root_box, &|x| {
            x.node == Some(cell) && matches!(x.content, BoxContent::Block)
        })
        .unwrap();
        // More than one line box of text => wrapped.
        let lines = collect_text_boxes(cell_box);
        assert!(
            lines.len() > 1,
            "cell content should wrap to multiple lines, got {}",
            lines.len()
        );
        // The cell content width should not exceed the (narrow) column.
        assert!(
            cell_box.dimensions.content.width <= 60.0 + 0.5,
            "cell wider than column"
        );
    }

    #[test]
    fn table_border_collapse_cells_are_flush() {
        // In the collapsed model adjacent cells sit flush: the right edge of one cell == the left
        // edge (x) of the next, with no inter-cell gap.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut tcs = disp(style::Display::Table);
        tcs.border_collapse = style::BorderCollapse::Collapse;
        styles.insert(table, tcs);

        let cells = build_row(&mut doc, &mut styles, table, "td", &["aa", "bb", "cc"]);
        // Give each cell a 1px border (the collapsed line) — inherits collapse from the table.
        for &c in &cells {
            if let Some(cs) = styles.get_mut(&c) {
                cs.border = style::Edges {
                    top: 1.0,
                    right: 1.0,
                    bottom: 1.0,
                    left: 1.0,
                };
                cs.border_collapse = style::BorderCollapse::Collapse;
            }
        }

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bx = |n: dom::NodeId| {
            find_box(&root_box, &|x| {
                x.node == Some(n) && matches!(x.content, BoxContent::Block)
            })
            .unwrap()
            .dimensions
            .border_box()
        };
        let c0 = bx(cells[0]);
        let c1 = bx(cells[1]);
        let c2 = bx(cells[2]);
        // Flush: next cell's x == previous cell's right edge.
        assert!(
            (c1.x - (c0.x + c0.width)).abs() < 0.01,
            "cell1 not flush with cell0: {} vs {}",
            c1.x,
            c0.x + c0.width
        );
        assert!(
            (c2.x - (c1.x + c1.width)).abs() < 0.01,
            "cell2 not flush with cell1"
        );
    }

    #[test]
    fn table_border_spacing_opens_a_gap() {
        // With the separated model + border-spacing, adjacent cells have a gap == the spacing.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut tcs = disp(style::Display::Table);
        tcs.border_spacing = 10.0; // separate is the default
        styles.insert(table, tcs);

        let cells = build_row(&mut doc, &mut styles, table, "td", &["aa", "bb"]);
        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bx = |n: dom::NodeId| {
            find_box(&root_box, &|x| {
                x.node == Some(n) && matches!(x.content, BoxContent::Block)
            })
            .unwrap()
            .dimensions
            .border_box()
        };
        let c0 = bx(cells[0]);
        let c1 = bx(cells[1]);
        let gap = c1.x - (c0.x + c0.width);
        assert!(
            (gap - 10.0).abs() < 0.5,
            "expected 10px border-spacing gap, got {gap}"
        );
        // And the cells are offset from the table content left by the leading spacing.
        let table_box = find_box(&root_box, &|x| x.node == Some(table))
            .unwrap()
            .dimensions
            .content;
        assert!(
            (c0.x - (table_box.x + 10.0)).abs() < 0.5,
            "first cell not offset by leading spacing"
        );
    }

    #[test]
    fn table_explicit_cell_width_sizes_column() {
        // A cell with an explicit width (what the `width="200"` presentational hint maps to) sizes
        // its column to that width.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let cells = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);
        if let Some(cs) = styles.get_mut(&cells[0]) {
            cs.width = Some(200.0);
        }
        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let c0 = find_box(&root_box, &|x| {
            x.node == Some(cells[0]) && matches!(x.content, BoxContent::Block)
        })
        .unwrap();
        assert!(
            (c0.dimensions.content.width - 200.0).abs() < 1.0,
            "explicit cell width not honored: {}",
            c0.dimensions.content.width
        );
    }

    #[test]
    fn table_colgroup_col_width_sizes_columns() {
        // <colgroup><col width=150><col width=50> sets the two column widths.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let table = doc.append_element(body, "table");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(table, disp(style::Display::Table));

        let cg = doc.append_element(table, "colgroup");
        styles.insert(cg, disp(style::Display::TableColumnGroup));
        let col0 = doc.append_element(cg, "col");
        let mut c0s = disp(style::Display::TableColumn);
        c0s.width = Some(150.0);
        styles.insert(col0, c0s);
        let col1 = doc.append_element(cg, "col");
        let mut c1s = disp(style::Display::TableColumn);
        c1s.width = Some(50.0);
        styles.insert(col1, c1s);

        let cells = build_row(&mut doc, &mut styles, table, "td", &["a", "b"]);

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let bx = |n: dom::NodeId| {
            find_box(&root_box, &|x| {
                x.node == Some(n) && matches!(x.content, BoxContent::Block)
            })
            .unwrap()
            .dimensions
            .border_box()
        };
        let w0 = bx(cells[0]).width;
        let w1 = bx(cells[1]).width;
        assert!(
            (w0 - 150.0).abs() < 1.5,
            "col0 width should be 150, got {w0}"
        );
        assert!((w1 - 50.0).abs() < 1.5, "col1 width should be 50, got {w1}");
    }

    #[test]
    fn box_sizing_border_box_subtracts_padding_border() {
        // A 200px-wide div with 20px padding + 5px border: border-box keeps the border-box at 200;
        // content-box would make the border-box 250 (200 + 2*20 + 2*5).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let bb = doc.append_element(body, "div");
        let cb = doc.append_element(body, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mk = |sizing| {
            let mut s = block_style(true);
            s.width = Some(200.0);
            s.padding = style::Edges::all(20.0);
            s.border = style::Edges::all(5.0);
            s.box_sizing = sizing;
            s
        };
        styles.insert(bb, mk(style::BoxSizing::BorderBox));
        styles.insert(cb, mk(style::BoxSizing::ContentBox));

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(
            (rect_of(&root_box, bb).width - 200.0).abs() < 0.5,
            "border-box width stays 200, got {}",
            rect_of(&root_box, bb).width
        );
        assert!(
            (rect_of(&root_box, cb).width - 250.0).abs() < 0.5,
            "content-box border-box = 250, got {}",
            rect_of(&root_box, cb).width
        );
    }

    #[test]
    fn flex_shrink_is_weighted_by_base_size() {
        // Items 300 + 100 (shrink 1 each) in a 200px flex row: the deficit (-200) is split by scaled
        // shrink (flex-shrink × base), so the larger item gives up more → 150 and 50, not 100 and 0.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let row = doc.append_element(body, "div");
        let a = doc.append_element(row, "div");
        let b = doc.append_element(row, "div");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            row,
            style::ComputedStyle {
                display: style::Display::Flex,
                width: Some(200.0),
                ..Default::default()
            },
        );
        let item = |w: f32| style::ComputedStyle {
            width: Some(w),
            height: Some(20.0),
            ..Default::default()
        };
        styles.insert(a, item(300.0));
        styles.insert(b, item(100.0));
        let rb = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(
            (rect_of(&rb, a).width - 150.0).abs() < 1.0,
            "a={}",
            rect_of(&rb, a).width
        );
        assert!(
            (rect_of(&rb, b).width - 50.0).abs() < 1.0,
            "b={}",
            rect_of(&rb, b).width
        );
    }

    #[test]
    fn flex_basis_percentage_resolves_against_container() {
        // `flex: 1 1 50%` items in a 400px flex row each get 200px (basis = 50% of the container).
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let row = doc.append_element(body, "div");
        let a = doc.append_element(row, "div");
        let b = doc.append_element(row, "div");
        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        styles.insert(
            row,
            style::ComputedStyle {
                display: style::Display::Flex,
                width: Some(400.0),
                ..Default::default()
            },
        );
        let item = || style::ComputedStyle {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            flex_basis_pct: Some(0.5),
            height: Some(20.0),
            ..Default::default()
        };
        styles.insert(a, item());
        styles.insert(b, item());
        let rb = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        assert!(
            (rect_of(&rb, a).width - 200.0).abs() < 1.0,
            "a={}",
            rect_of(&rb, a).width
        );
        assert!(
            (rect_of(&rb, b).width - 200.0).abs() < 1.0,
            "b={}",
            rect_of(&rb, b).width
        );
    }

    #[test]
    fn inline_block_percentage_width_resolves_against_container() {
        // An inline-block with width:50% inside a 400px block is 200px (not shrink-to-fit to its
        // content). Regression: percentage widths used to resolve against the box's intrinsic width.
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let ib = doc.append_element(wrap, "span");
        doc.append_child(ib, dom::NodeData::Text("hi".into()));

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(400.0);
        styles.insert(wrap, wrap_s);
        styles.insert(
            ib,
            style::ComputedStyle {
                display: style::Display::InlineBlock,
                width_pct: Some(0.5),
                height: Some(20.0),
                ..Default::default()
            },
        );

        let root_box = layout_document(&doc, &styles, 800.0, 600.0, &Stub, &HashMap::new(), None);
        let w = rect_of(&root_box, ib).width;
        assert!(
            (w - 200.0).abs() < 1.0,
            "inline-block width:50% of 400px should be 200px, got {w}"
        );
    }

    // ---- Floats ----

    /// Two `float:left` blocks pack side by side on the same row.
    #[test]
    fn two_left_floats_pack_side_by_side() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let a = doc.append_element(wrap, "div");
        let b = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(900.0);
        styles.insert(wrap, wrap_s);
        styles.insert(a, floated(300.0, 100.0, style::Float::Left));
        styles.insert(b, floated(300.0, 100.0, style::Float::Left));

        let root_box = layout_document(&doc, &styles, 900.0, 600.0, &Stub, &HashMap::new(), None);
        let ra = rect_of(&root_box, a);
        let rb = rect_of(&root_box, b);
        assert!(
            (ra.x - 0.0).abs() < 0.5,
            "first float at left edge, x={}",
            ra.x
        );
        assert!(
            (rb.x - 300.0).abs() < 0.5,
            "second float packs beside the first, x={} (want 300)",
            rb.x
        );
        assert!((ra.y - rb.y).abs() < 0.5, "both on the same row");
    }

    /// A left float + a right float sit on the same row at opposite edges.
    #[test]
    fn left_and_right_floats_oppose() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let l = doc.append_element(wrap, "div");
        let r = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(900.0);
        styles.insert(wrap, wrap_s);
        styles.insert(l, floated(200.0, 80.0, style::Float::Left));
        styles.insert(r, floated(200.0, 80.0, style::Float::Right));

        let root_box = layout_document(&doc, &styles, 900.0, 600.0, &Stub, &HashMap::new(), None);
        let rl = rect_of(&root_box, l);
        let rr = rect_of(&root_box, r);
        assert!((rl.x - 0.0).abs() < 0.5, "left float at x=0, got {}", rl.x);
        assert!(
            (rr.x - 700.0).abs() < 0.5,
            "right float flush right (900-200), got {}",
            rr.x
        );
    }

    /// Three 33%-wide left floats fit on one row; the fourth wraps to a second row.
    #[test]
    fn percent_floats_wrap_after_three() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let cols: Vec<_> = (0..4).map(|_| doc.append_element(wrap, "div")).collect();

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(900.0);
        styles.insert(wrap, wrap_s);
        for &c in &cols {
            styles.insert(c, floated_pct(0.33, 60.0, style::Float::Left));
        }

        let root_box = layout_document(&doc, &styles, 900.0, 600.0, &Stub, &HashMap::new(), None);
        let r: Vec<Rect> = cols.iter().map(|&c| rect_of(&root_box, c)).collect();
        // 0.33 * 900 = 297; three fit (891 <= 900), the fourth wraps.
        assert!(
            (r[0].width - 297.0).abs() < 1.0,
            "33% width, got {}",
            r[0].width
        );
        assert!(
            (r[0].y - r[1].y).abs() < 0.5 && (r[1].y - r[2].y).abs() < 0.5,
            "first three on row 1"
        );
        assert!(
            r[3].y > r[0].y + 0.5,
            "fourth wraps to a new row, y={}",
            r[3].y
        );
        assert!(
            (r[3].x - 0.0).abs() < 0.5,
            "wrapped float returns to the left edge"
        );
    }

    /// Floats inside an `inline-block` are contained within it (positioned relative to its content
    /// box), not escaping to an ancestor formatting context.
    #[test]
    fn floats_are_contained_by_inline_block() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        // A left float sidebar, then an inline-block "main" beside it containing its own floats.
        let wrap = doc.append_element(body, "div");
        let main = doc.append_element(wrap, "div");
        let c1 = doc.append_element(main, "div");
        let c2 = doc.append_element(main, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(900.0);
        styles.insert(wrap, wrap_s);
        styles.insert(main, inline_block(600.0, 200.0));
        styles.insert(c1, floated(200.0, 60.0, style::Float::Left));
        styles.insert(c2, floated(200.0, 60.0, style::Float::Left));

        let root_box = layout_document(&doc, &styles, 900.0, 600.0, &Stub, &HashMap::new(), None);
        let rm = rect_of(&root_box, main);
        let r1 = rect_of(&root_box, c1);
        let r2 = rect_of(&root_box, c2);
        // The inline-block's own floats live inside its content box, side by side.
        assert!(
            r1.x >= rm.x - 0.5 && r1.x < rm.x + rm.width,
            "float c1 (x={}) should sit inside main [{}, {})",
            r1.x,
            rm.x,
            rm.x + rm.width
        );
        assert!(
            (r2.x - (r1.x + 200.0)).abs() < 0.5,
            "float c2 packs beside c1 inside main, x={} (want {})",
            r2.x,
            r1.x + 200.0
        );
    }

    /// A `float:left` sidebar with an `inline-block` main beside it: the main starts after the
    /// sidebar's right edge (the classic two-column footer pattern).
    #[test]
    fn float_sidebar_then_inline_block_main() {
        let mut doc = dom::Document::new();
        let root = doc.root();
        let body = doc.append_element(root, "body");
        let wrap = doc.append_element(body, "div");
        let side = doc.append_element(wrap, "div");
        let main = doc.append_element(wrap, "div");

        let mut styles = HashMap::new();
        styles.insert(body, block_style(true));
        let mut wrap_s = block_style(true);
        wrap_s.width = Some(900.0);
        styles.insert(wrap, wrap_s);
        styles.insert(side, floated(300.0, 160.0, style::Float::Left));
        styles.insert(main, inline_block(500.0, 160.0));

        let root_box = layout_document(&doc, &styles, 900.0, 600.0, &Stub, &HashMap::new(), None);
        let rs = rect_of(&root_box, side);
        let rm = rect_of(&root_box, main);
        assert!(
            (rs.x - 0.0).abs() < 0.5,
            "sidebar floats to x=0, got {}",
            rs.x
        );
        assert!(
            rm.x >= 300.0 - 0.5,
            "inline-block main starts after the float (x={} want >=300)",
            rm.x
        );
    }
}
