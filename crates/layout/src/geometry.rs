use crate::{LayoutBox, Rect};
use std::collections::HashMap;

/// Order sibling boxes for painting after flow geometry is final. Positioned descendants
/// at auto/zero or positive stack levels paint above in-flow siblings regardless of DOM order;
/// negative levels paint below them. Stable sorting preserves tree order within one level.
/// This handles sibling stacking; it does not yet flatten nested CSS stacking contexts.
pub(crate) fn order_for_paint(
    boxx: &mut LayoutBox,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) {
    for child in &mut boxx.children {
        order_for_paint(child, styles);
    }
    boxx.children.sort_by_key(|child| {
        let Some(cs) = child.node.and_then(|id| styles.get(&id)) else {
            return (1, 0);
        };
        if cs.position == style::Position::Static {
            (1, 0)
        } else {
            let z = cs.z_index.unwrap_or(0);
            (if z < 0 { 0 } else { 2 }, z)
        }
    });
}

/// Bounding border rectangles for DOM nodes, including flattened inline ancestors.
/// Layout emits one text box per fragment, so inline elements need the union of their
/// descendants' fragments even though they no longer have a box in the paint tree.
pub fn node_rects(
    root: &LayoutBox,
    doc: &dom::Document,
    styles: &HashMap<dom::NodeId, style::ComputedStyle>,
) -> HashMap<usize, Rect> {
    fn add(out: &mut HashMap<usize, Rect>, node: dom::NodeId, rect: Rect) {
        out.entry(node.0)
            .and_modify(|old| {
                // CSSOM ignores empty fragments when a nonempty fragment exists.
                if rect.width <= 0.0 || rect.height <= 0.0 {
                    return;
                }
                if old.width <= 0.0 || old.height <= 0.0 {
                    *old = rect;
                    return;
                }
                let right = (old.x + old.width).max(rect.x + rect.width);
                let bottom = (old.y + old.height).max(rect.y + rect.height);
                old.x = old.x.min(rect.x);
                old.y = old.y.min(rect.y);
                old.width = right - old.x;
                old.height = bottom - old.y;
            })
            .or_insert(rect);
    }

    fn walk(
        boxx: &LayoutBox,
        doc: &dom::Document,
        styles: &HashMap<dom::NodeId, style::ComputedStyle>,
        out: &mut HashMap<usize, Rect>,
    ) {
        if let Some(node) = boxx.node {
            let rect = boxx.dimensions.border_box();
            let out_of_flow = |id| {
                styles.get(&id).is_some_and(|cs| {
                    matches!(
                        cs.position,
                        style::Position::Absolute | style::Position::Fixed
                    ) || cs.float != style::Float::None
                })
            };
            // Block-in-inline wrappers may span the containing block; the inline element's
            // bounds still come from its in-flow fragments, not this implementation wrapper.
            let inline_wrapper = !out_of_flow(node)
                && styles
                    .get(&node)
                    .is_some_and(|cs| cs.display == style::Display::Inline && !cs.display_block)
                && matches!(
                    boxx.content,
                    crate::BoxContent::Block
                        | crate::BoxContent::Inline
                        | crate::BoxContent::Anonymous
                );
            if !inline_wrapper {
                add(out, node, rect);
            }
            // Positioned descendants have their own boxes, not fragments of an inline ancestor.
            let mut parent = if out_of_flow(node) {
                None
            } else {
                doc.get(node).parent
            };
            while let Some(id) = parent {
                if out_of_flow(id)
                    || !styles
                        .get(&id)
                        .is_some_and(|cs| cs.display == style::Display::Inline && !cs.display_block)
                {
                    break;
                }
                if !inline_wrapper {
                    add(out, id, rect);
                }
                parent = doc.get(id).parent;
            }
        }
        for child in &boxx.children {
            walk(child, doc, styles, out);
        }
    }

    let mut out = HashMap::new();
    walk(root, doc, styles, &mut out);
    out
}
