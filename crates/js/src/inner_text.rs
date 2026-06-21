//! The `innerText` / `outerText` IDL attributes (HTML §the-innertext-idl-attribute).
//!
//! The getter implements the spec's *rendered text* algorithm: a layout-aware walk of the DOM that
//! honors `display:none`, `<br>`, block-vs-inline boundaries, table cell/row separators, `<p>` blank
//! lines, `text-transform`, and CSS whitespace collapsing (including collapse *across* inline
//! element boundaries). It is driven by the cascaded [`style::ComputedStyle`] of each element rather
//! than the pixel box tree, so it is independent of fonts / soft line wrapping (which `innerText`
//! must ignore anyway).
//!
//! The setters implement the *rendered text fragment* algorithm: runs of text become Text nodes and
//! each line break (CR, LF, or CRLF) becomes a `<br>` element. `innerText` replaces the element's
//! children; `outerText` replaces the element itself and merges with adjacent Text siblings.
//!
//! Known gaps (the engine does not model these yet): `visibility`, `float`, `white-space:pre-line`,
//! `display:inline-table`, and `::first-line`/`::first-letter` text-transform.

use std::collections::HashMap;

use style::{ComputedStyle, Display, Position, TextTransform, Visibility, WhiteSpace};

type Styles = HashMap<dom::NodeId, ComputedStyle>;

// ---------------------------------------------------------------------------------------------
// Getter: the rendered-text algorithm.
// ---------------------------------------------------------------------------------------------

/// One element of the spec's intermediate results list: either literal text, or a *required line
/// break count* contributed by a block-level / `<p>` boundary (collapsed to `max` newlines later).
#[derive(Clone)]
enum Item {
    Str(String),
    Break(u8),
}

/// A token produced while walking the tree, before whitespace collapsing.
#[derive(Clone)]
enum Tok {
    /// A run-collapsible whitespace character (becomes at most one space).
    Space,
    /// A literal character (preserved whitespace, letters, `\n` from `<br>` / `white-space:pre`).
    Char(char),
    /// An atomic inline boundary (replaced element / inline-block): contributes no text, but stops
    /// adjacent collapsible whitespace from being removed at what would otherwise look like an edge.
    Atomic,
    /// A required line break count from a block-level box.
    Break(u8),
}

const fn is_collapsible_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0c}')
}

/// Elements whose contents never produce CSS boxes (replaced / embedded content). Their descendant
/// text is not part of the rendered text; they act as a single atomic inline (or block) box.
fn is_embedded(tag: &str) -> bool {
    matches!(
        tag,
        "img" | "canvas" | "iframe" | "embed" | "object" | "audio" | "video" | "input" | "textarea"
    )
}

fn display_of(map: &Styles, id: dom::NodeId) -> Display {
    map.get(&id)
        .map(|c| {
            if c.display_none {
                Display::None
            } else {
                c.display
            }
        })
        .unwrap_or(Display::Inline)
}

fn position_of(map: &Styles, id: dom::NodeId) -> Position {
    map.get(&id).map(|c| c.position).unwrap_or(Position::Static)
}

fn visibility_of(map: &Styles, id: dom::NodeId) -> Visibility {
    map.get(&id)
        .map(|c| c.visibility)
        .unwrap_or(Visibility::Visible)
}

/// Whether a box with this `display`/`position` is block-level (induces a line break around it).
fn is_block_level(display: Display, position: Position) -> bool {
    if matches!(position, Position::Absolute | Position::Fixed) {
        return true;
    }
    matches!(
        display,
        Display::Block | Display::Flex | Display::Grid | Display::Table | Display::TableCaption
    )
}

/// Table-internal display contexts in which whitespace-only anonymous text is not rendered.
fn is_table_internal(display: Display) -> bool {
    matches!(
        display,
        Display::Table
            | Display::TableRow
            | Display::TableRowGroup
            | Display::TableHeaderGroup
            | Display::TableFooterGroup
            | Display::TableColumn
            | Display::TableColumnGroup
    )
}

fn elem_tag(doc: &dom::Document, id: dom::NodeId) -> Option<String> {
    match &doc.get(id).data {
        dom::NodeData::Element(e) => Some(e.tag.to_ascii_lowercase()),
        _ => None,
    }
}

fn is_elem_tag(doc: &dom::Document, id: dom::NodeId, tag: &str) -> bool {
    elem_tag(doc, id).as_deref() == Some(tag)
}

/// Whether `id` is an HTML element. `innerText`/`outerText` are HTMLElement-only — they are absent
/// (getter → `undefined`, setter → no-op) on SVG/MathML elements, which the HTML parser tags with a
/// foreign `namespace` (HTML elements carry `None` / the XHTML namespace).
pub fn is_html_element(doc: &dom::Document, id: dom::NodeId) -> bool {
    match &doc.get(id).data {
        dom::NodeData::Element(e) => match e.namespace.as_deref() {
            None | Some("http://www.w3.org/1999/xhtml") => true,
            Some(_) => false,
        },
        _ => false,
    }
}

/// The `innerText`/`outerText` getter for element `id` (identical for both attributes).
pub fn inner_text(doc: &dom::Document, map: &Styles, id: dom::NodeId) -> String {
    if !is_being_rendered(doc, map, id) {
        // Not rendered (detached, or display:none on self / an ancestor): fall back to the element's
        // descendant text content, verbatim.
        return descendant_text(doc, id);
    }
    let mut toks = Vec::new();
    collect_element(doc, map, id, &mut toks, true);
    collapse(toks)
}

/// An element is "being rendered" if it is connected to the document and neither it nor any ancestor
/// is `display:none`.
fn is_being_rendered(doc: &dom::Document, map: &Styles, id: dom::NodeId) -> bool {
    let mut cur = Some(id);
    while let Some(n) = cur {
        match &doc.get(n).data {
            dom::NodeData::Document => return true,
            dom::NodeData::Element(_) => {
                if matches!(display_of(map, n), Display::None) {
                    return false;
                }
            }
            _ => {}
        }
        cur = doc.get(n).parent;
    }
    false
}

/// Concatenate every descendant Text/CDATA node under `id`, in tree order (the DOM `textContent`).
fn descendant_text(doc: &dom::Document, id: dom::NodeId) -> String {
    let mut out = String::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) | dom::NodeData::Cdata(t) => out.push_str(t),
            _ => {
                for &c in &doc.get(id).children {
                    walk(doc, c, out);
                }
            }
        }
    }
    walk(doc, id, &mut out);
    out
}

/// Run the rendered text collection steps over a child node (Text or Element), appending to `out`.
fn collect(doc: &dom::Document, map: &Styles, id: dom::NodeId, out: &mut Vec<Tok>) {
    match &doc.get(id).data {
        dom::NodeData::Text(t) | dom::NodeData::Cdata(t) => push_text(doc, map, id, t, out),
        dom::NodeData::Element(_) => collect_element(doc, map, id, out, false),
        dom::NodeData::DocumentFragment => {
            for &c in &doc.get(id).children {
                collect(doc, map, c, out);
            }
        }
        _ => {}
    }
}

/// Append the rendered tokens for a Text node, applying `white-space` and `text-transform`.
fn push_text(doc: &dom::Document, map: &Styles, id: dom::NodeId, raw: &str, out: &mut Vec<Tok>) {
    let parent = doc.get(id).parent;
    let (ws, tt) = parent
        .and_then(|p| map.get(&p))
        .map(|cs| (cs.white_space, cs.text_transform))
        .unwrap_or((WhiteSpace::Normal, TextTransform::None));

    if let Some(p) = parent {
        // Text inherits its parent's visibility: `hidden`/`collapse` text is not rendered.
        if !matches!(visibility_of(map, p), Visibility::Visible) {
            return;
        }
        // Whitespace-only text directly inside a table-internal box generates no rendered box.
        if is_table_internal(display_of(map, p)) && raw.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    let transformed = apply_text_transform(raw, tt);
    match ws {
        WhiteSpace::Normal | WhiteSpace::Nowrap => {
            for c in transformed.chars() {
                if is_collapsible_ws(c) {
                    out.push(Tok::Space);
                } else {
                    out.push(Tok::Char(c));
                }
            }
        }
        WhiteSpace::Pre | WhiteSpace::PreWrap => {
            // Spaces and tabs are preserved; CR / CRLF normalize to a single LF.
            let mut chars = transformed.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                        out.push(Tok::Char('\n'));
                    }
                    _ => out.push(Tok::Char(c)),
                }
            }
        }
        WhiteSpace::PreLine => {
            // Newlines are preserved as forced breaks; spaces/tabs collapse like `normal`.
            let mut chars = transformed.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\n' => out.push(Tok::Char('\n')),
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                        out.push(Tok::Char('\n'));
                    }
                    c if is_collapsible_ws(c) => out.push(Tok::Space),
                    _ => out.push(Tok::Char(c)),
                }
            }
        }
    }
}

fn apply_text_transform(s: &str, t: TextTransform) -> String {
    match t {
        TextTransform::None => s.to_string(),
        TextTransform::Uppercase => s.to_uppercase(),
        TextTransform::Lowercase => s.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(s.len());
            let mut at_word_start = true;
            for c in s.chars() {
                if c.is_whitespace() {
                    at_word_start = true;
                    out.push(c);
                } else if at_word_start {
                    at_word_start = false;
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
            }
            out
        }
    }
}

/// The rendered text collection steps for an element. `top_level` is true for the element the getter
/// was called on: it contributes its *content* but not its own block-break / atomic wrappers.
fn collect_element(
    doc: &dom::Document,
    map: &Styles,
    id: dom::NodeId,
    out: &mut Vec<Tok>,
    top_level: bool,
) {
    let tag = match elem_tag(doc, id) {
        Some(t) => t,
        None => return,
    };
    let children: Vec<dom::NodeId> = doc.get(id).children.clone();
    let display = display_of(map, id);
    let position = position_of(map, id);

    // `visibility: hidden`/`collapse`: the element contributes no rendered text of its own (no break,
    // tab, atomic box, or text), but visible descendants still show — so recurse and return their
    // contributions only. `visibility` inherits, so hidden text children are dropped in `push_text`.
    if !matches!(visibility_of(map, id), Visibility::Visible) {
        if !top_level && matches!(display, Display::None) {
            return;
        }
        for &c in &children {
            collect(doc, map, c, out);
        }
        return;
    }

    // <br>: a forced line break (its own content is ignored). As the root element it has no content.
    if tag == "br" {
        if !top_level {
            out.push(Tok::Char('\n'));
        }
        return;
    }
    // <noscript>/<template> contents are never rendered as text.
    if tag == "noscript" || tag == "template" {
        return;
    }
    // A non-rendered descendant subtree produces nothing. (The root never reaches here: a
    // display:none root is handled by `is_being_rendered`.)
    if !top_level && matches!(display, Display::None) {
        return;
    }

    // <select>: an inline box whose children are only its <optgroup>/<option> boxes.
    if tag == "select" {
        for &c in &children {
            if is_elem_tag(doc, c, "option") || is_elem_tag(doc, c, "optgroup") {
                collect_element(doc, map, c, out, false);
            }
        }
        return;
    }
    // <optgroup>: a block box whose children are only its <option> boxes.
    if tag == "optgroup" {
        if !top_level {
            out.push(Tok::Break(1));
        }
        for &c in &children {
            if is_elem_tag(doc, c, "option") {
                collect_element(doc, map, c, out, false);
            }
        }
        if !top_level {
            out.push(Tok::Break(1));
        }
        return;
    }
    // <option>: a block box with normal children.
    if tag == "option" {
        if !top_level {
            out.push(Tok::Break(1));
        }
        for &c in &children {
            collect(doc, map, c, out);
        }
        if !top_level {
            out.push(Tok::Break(1));
        }
        return;
    }
    // <details>: a block box; when closed, only the first <summary> is rendered.
    if tag == "details" {
        let open =
            matches!(&doc.get(id).data, dom::NodeData::Element(e) if e.attrs.contains_key("open"));
        if !top_level {
            out.push(Tok::Break(1));
        }
        if open {
            for &c in &children {
                collect(doc, map, c, out);
            }
        } else if let Some(&s) = children.iter().find(|&&c| is_elem_tag(doc, c, "summary")) {
            collect_element(doc, map, s, out, false);
        }
        if !top_level {
            out.push(Tok::Break(1));
        }
        return;
    }
    // <rp> is `display:none` by default (ruby parenthesis): rendered only if author-styled non-inline.
    if tag == "rp" && matches!(display, Display::Inline) {
        return;
    }

    // Replaced / embedded content: descendant text is not rendered.
    if is_embedded(&tag) {
        if top_level {
            return;
        }
        if is_block_level(display, position) {
            out.push(Tok::Break(1));
            out.push(Tok::Break(1));
        } else {
            out.push(Tok::Atomic);
            out.push(Tok::Atomic);
        }
        return;
    }

    let parent_display = doc.get(id).parent.map(|p| display_of(map, p));
    let blockified = matches!(
        parent_display,
        Some(Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid)
    );

    // Atomic inline (inline-block / inline-flex / inline-grid): its content forms an independent
    // formatting context (own leading/trailing trim) and is opaque to the outer whitespace collapse.
    // <p> is excluded: it always contributes blank lines, even when displayed as inline-block.
    if !top_level
        && !blockified
        && tag != "p"
        && matches!(
            display,
            Display::InlineBlock | Display::InlineFlex | Display::InlineGrid
        )
    {
        let mut inner = Vec::new();
        collect_element(doc, map, id, &mut inner, true);
        let text = collapse(inner);
        out.push(Tok::Atomic);
        out.extend(text.chars().map(Tok::Char));
        out.push(Tok::Atomic);
        return;
    }

    // Table cell: separate from the following cell in its row with a tab.
    if matches!(display, Display::TableCell) {
        for &c in &children {
            collect(doc, map, c, out);
        }
        if !top_level && !is_last_table_cell(doc, map, id) {
            out.push(Tok::Char('\t'));
        }
        return;
    }
    // Table row: separate from the following row with a newline.
    if matches!(display, Display::TableRow) {
        for &c in &children {
            collect(doc, map, c, out);
        }
        if !top_level && !is_last_table_row(doc, map, id) {
            out.push(Tok::Char('\n'));
        }
        return;
    }

    // Generic flow content. <p> contributes a *blank* line (2) around it; other block-level boxes a
    // single line (1). Inline content contributes no break.
    let block = is_block_level(display, position) || blockified;
    let breaks = if tag == "p" {
        2
    } else if block {
        1
    } else {
        0
    };
    if !top_level && breaks > 0 {
        out.push(Tok::Break(breaks));
    }
    for &c in &children {
        collect(doc, map, c, out);
    }
    if !top_level && breaks > 0 {
        out.push(Tok::Break(breaks));
    }
}

/// Whether `id` (a table-cell) is the last table-cell box among its siblings.
fn is_last_table_cell(doc: &dom::Document, map: &Styles, id: dom::NodeId) -> bool {
    match doc.get(id).parent {
        None => true,
        Some(p) => {
            doc.get(p)
                .children
                .iter()
                .rev()
                .find(|&&c| matches!(display_of(map, c), Display::TableCell))
                == Some(&id)
        }
    }
}

/// Whether `id` (a table-row) is the last table-row box in its nearest enclosing table.
fn is_last_table_row(doc: &dom::Document, map: &Styles, id: dom::NodeId) -> bool {
    // Find the nearest ancestor table box.
    let mut table = None;
    let mut cur = doc.get(id).parent;
    while let Some(n) = cur {
        if matches!(display_of(map, n), Display::Table) {
            table = Some(n);
            break;
        }
        cur = doc.get(n).parent;
    }
    let table = match table {
        Some(t) => t,
        None => return true,
    };
    let mut rows = Vec::new();
    collect_rows(doc, map, table, &mut rows);
    rows.last() == Some(&id)
}

/// Collect table-row boxes under `node`, in tree order, without descending into nested tables/rows.
fn collect_rows(doc: &dom::Document, map: &Styles, node: dom::NodeId, rows: &mut Vec<dom::NodeId>) {
    for &c in &doc.get(node).children {
        if !matches!(doc.get(c).data, dom::NodeData::Element(_)) {
            continue;
        }
        let d = display_of(map, c);
        if matches!(d, Display::TableRow) {
            rows.push(c);
        } else if matches!(d, Display::Table) {
            // a nested table — its rows belong to it, not us
        } else {
            collect_rows(doc, map, c, rows);
        }
    }
}

/// Collapse a token stream to the final rendered string: remove collapsible whitespace at line
/// edges (stream start/end, around forced breaks), collapse interior runs to a single space, then
/// resolve required line break counts (dropping leading/trailing runs, each run → max newlines).
fn collapse(toks: Vec<Tok>) -> String {
    // Pass 1: collapse runs of `Space`, dropping any run that touches a hard boundary on either side
    // (stream start/end, a `Break`, or a `\n` produced by `<br>`/preserved newline). `Atomic` and
    // ordinary characters are *not* boundaries, so a space between content and an atomic box stays.
    let mut kept: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i], Tok::Space) {
            let mut j = i;
            while j < toks.len() && matches!(toks[j], Tok::Space) {
                j += 1;
            }
            let left_edge = matches!(
                kept.last(),
                None | Some(Tok::Break(_)) | Some(Tok::Char('\n'))
            );
            let right_edge = matches!(
                toks.get(j),
                None | Some(Tok::Break(_)) | Some(Tok::Char('\n'))
            );
            if !left_edge && !right_edge {
                kept.push(Tok::Space);
            }
            i = j;
        } else {
            kept.push(toks[i].clone());
            i += 1;
        }
    }

    // Pass 2: build the Item list (text segments interleaved with required line break counts).
    let mut items: Vec<Item> = Vec::new();
    let mut cur = String::new();
    for t in kept {
        match t {
            Tok::Space => cur.push(' '),
            Tok::Char(c) => cur.push(c),
            Tok::Atomic => {}
            Tok::Break(k) => {
                if !cur.is_empty() {
                    items.push(Item::Str(std::mem::take(&mut cur)));
                }
                items.push(Item::Break(k));
            }
        }
    }
    if !cur.is_empty() {
        items.push(Item::Str(cur));
    }

    // Pass 3: drop leading/trailing break runs; collapse each interior run to `max` newlines.
    let mut start = 0;
    while start < items.len() && matches!(items[start], Item::Break(_)) {
        start += 1;
    }
    let mut end = items.len();
    while end > start && matches!(items[end - 1], Item::Break(_)) {
        end -= 1;
    }
    let mut out = String::new();
    let mut pending = 0u8;
    for item in &items[start..end] {
        match item {
            Item::Str(s) => {
                if pending > 0 {
                    out.push_str(&"\n".repeat(pending as usize));
                    pending = 0;
                }
                out.push_str(s);
            }
            Item::Break(k) => pending = pending.max(*k),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Setters: the rendered-text-fragment algorithm.
// ---------------------------------------------------------------------------------------------

/// Build the nodes of the "rendered text fragment" for `input`: runs of non-newline text become
/// Text nodes and each line break (CR, LF, or CRLF) becomes a `<br>` element.
fn rendered_text_fragment(input: &str) -> Vec<dom::NodeData> {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let start = i;
        while i < n && chars[i] != '\r' && chars[i] != '\n' {
            i += 1;
        }
        if i > start {
            out.push(dom::NodeData::Text(chars[start..i].iter().collect()));
        }
        if i < n {
            if chars[i] == '\r' && i + 1 < n && chars[i + 1] == '\n' {
                i += 2;
            } else {
                i += 1;
            }
            out.push(dom::NodeData::Element(dom::ElementData {
                tag: "br".to_string(),
                attrs: Default::default(),
                namespace: None,
            }));
        }
    }
    out
}

/// The `innerText` setter: replace `id`'s children with the rendered text fragment of `text`.
pub fn set_inner_text(doc: &mut dom::Document, id: dom::NodeId, text: &str) {
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(id).children);
    for c in old {
        doc.get_mut(c).parent = None;
    }
    for nd in rendered_text_fragment(text) {
        doc.append_child(id, nd);
    }
}

fn is_text_node(doc: &dom::Document, id: dom::NodeId) -> bool {
    matches!(doc.get(id).data, dom::NodeData::Text(_))
}

/// Append `source`'s data onto `target` (both Text nodes) and remove `source` from its parent.
fn merge_text(doc: &mut dom::Document, target: dom::NodeId, source: dom::NodeId) {
    let src = match &doc.get(source).data {
        dom::NodeData::Text(t) => t.clone(),
        _ => return,
    };
    if let dom::NodeData::Text(t) = &mut doc.get_mut(target).data {
        t.push_str(&src);
    }
    if let Some(p) = doc.get(source).parent {
        if let Some(pos) = doc.get(p).children.iter().position(|&c| c == source) {
            doc.get_mut(p).children.remove(pos);
        }
    }
    doc.get_mut(source).parent = None;
}

/// The `outerText` setter: replace `id` itself with the rendered text fragment of `text`, then merge
/// the boundary nodes with adjacent Text siblings. Returns `false` if `id` has no parent (the caller
/// throws `NoModificationAllowedError`).
pub fn set_outer_text(doc: &mut dom::Document, id: dom::NodeId, text: &str) -> bool {
    let parent = match doc.get(id).parent {
        Some(p) => p,
        None => return false,
    };
    let idx = match doc.get(parent).children.iter().position(|&c| c == id) {
        Some(i) => i,
        None => return false,
    };
    let prev = if idx > 0 {
        Some(doc.get(parent).children[idx - 1])
    } else {
        None
    };
    let next = doc.get(parent).children.get(idx + 1).copied();

    let mut frag = rendered_text_fragment(text);
    if frag.is_empty() {
        frag.push(dom::NodeData::Text(String::new()));
    }
    let new_ids: Vec<dom::NodeId> = frag
        .into_iter()
        .map(|nd| doc.alloc(nd, Some(parent)))
        .collect();

    // Replace the element with the fragment nodes in the parent's child list.
    doc.get_mut(id).parent = None;
    doc.get_mut(parent)
        .children
        .splice(idx..idx + 1, new_ids.iter().copied());

    // Merge the last fragment node with the following Text sibling, then the previous Text sibling
    // with what now follows it (spec order: next first, then previous).
    if let Some(next_id) = next {
        if is_text_node(doc, next_id) {
            if let Some(&last) = new_ids.last() {
                if is_text_node(doc, last) {
                    merge_text(doc, last, next_id);
                }
            }
        }
    }
    if let Some(prev_id) = prev {
        if is_text_node(doc, prev_id) {
            let pidx = doc.get(parent).children.iter().position(|&c| c == prev_id);
            if let Some(pidx) = pidx {
                if let Some(&sib) = doc.get(parent).children.get(pidx + 1) {
                    if is_text_node(doc, sib) {
                        merge_text(doc, prev_id, sib);
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag_tags(input: &str) -> Vec<String> {
        rendered_text_fragment(input)
            .iter()
            .map(|nd| match nd {
                dom::NodeData::Text(t) => format!("text:{t}"),
                dom::NodeData::Element(e) => format!("<{}>", e.tag),
                _ => "?".to_string(),
            })
            .collect()
    }

    #[test]
    fn fragment_splits_on_line_breaks() {
        assert_eq!(frag_tags("abc"), vec!["text:abc"]);
        assert_eq!(frag_tags("abc\ndef"), vec!["text:abc", "<br>", "text:def"]);
        assert_eq!(
            frag_tags("abc\r\ndef"),
            vec!["text:abc", "<br>", "text:def"]
        );
        assert_eq!(
            frag_tags("abc\n\ndef"),
            vec!["text:abc", "<br>", "<br>", "text:def"]
        );
        assert_eq!(frag_tags("\nabc"), vec!["<br>", "text:abc"]);
        assert_eq!(frag_tags("abc\n"), vec!["text:abc", "<br>"]);
        assert_eq!(frag_tags(""), Vec::<String>::new());
        assert_eq!(frag_tags("abc  def"), vec!["text:abc  def"]);
    }
}
