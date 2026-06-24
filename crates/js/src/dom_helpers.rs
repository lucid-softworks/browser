/// Concatenate every descendant `Text` node under `id`, in document order.
pub(crate) fn text_content(doc: &dom::Document, id: dom::NodeId) -> String {
    // Per the DOM standard's `textContent` getter: for a Text/Comment/PI node it's the node's own
    // data; for an Element/DocumentFragment it's the concatenation of all *descendant Text* node
    // data in tree order (Comment data is NOT included for those).
    match &doc.get(id).data {
        dom::NodeData::Text(t) => return t.clone(),
        dom::NodeData::Comment(c) => return c.clone(),
        dom::NodeData::Cdata(c) => return c.clone(),
        dom::NodeData::ProcessingInstruction(p) => return p.data.clone(),
        _ => {}
    }
    let mut out = String::new();
    fn walk(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(t),
            dom::NodeData::Cdata(t) => out.push_str(t),
            _ => {
                for &child in &doc.get(id).children {
                    walk(doc, child, out);
                }
            }
        }
    }
    walk(doc, id, &mut out);
    out
}

/// Serialize the children of `id` back to an HTML string (the `innerHTML` of `id`).
pub(crate) fn inner_html(doc: &dom::Document, id: dom::NodeId) -> String {
    fn is_void(tag: &str) -> bool {
        matches!(
            tag.to_ascii_lowercase().as_str(),
            "area"
                | "base"
                | "br"
                | "col"
                | "embed"
                | "hr"
                | "img"
                | "input"
                | "link"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
        )
    }
    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn escape_attr(s: &str) -> String {
        s.replace('&', "&amp;").replace('"', "&quot;")
    }
    fn serialize_node(doc: &dom::Document, id: dom::NodeId, out: &mut String) {
        match &doc.get(id).data {
            dom::NodeData::Text(t) => out.push_str(&escape_text(t)),
            dom::NodeData::Cdata(c) => {
                out.push_str("<![CDATA[");
                out.push_str(c);
                out.push_str("]]>");
            }
            dom::NodeData::Comment(c) => {
                out.push_str("<!--");
                out.push_str(c);
                out.push_str("-->");
            }
            dom::NodeData::Element(e) => {
                out.push('<');
                out.push_str(&e.tag);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&escape_attr(v));
                    out.push('"');
                }
                out.push('>');
                if !is_void(&e.tag) {
                    for &child in &doc.get(id).children {
                        serialize_node(doc, child, out);
                    }
                    out.push_str("</");
                    out.push_str(&e.tag);
                    out.push('>');
                }
            }
            dom::NodeData::DocumentType(d) => {
                out.push_str("<!DOCTYPE ");
                out.push_str(&d.name);
                out.push('>');
            }
            dom::NodeData::ProcessingInstruction(p) => {
                out.push_str("<?");
                out.push_str(&p.target);
                out.push(' ');
                out.push_str(&p.data);
                out.push('>');
            }
            dom::NodeData::Document | dom::NodeData::DocumentFragment => {
                for &child in &doc.get(id).children {
                    serialize_node(doc, child, out);
                }
            }
        }
    }
    let mut out = String::new();
    for &child in &doc.get(id).children {
        serialize_node(doc, child, &mut out);
    }
    out
}

/// Replace all children of `id` with a single `Text` node holding `text`.
pub(crate) fn set_text_content(doc: &mut dom::Document, id: dom::NodeId, text: &str) {
    // For a Text/Comment node, mutating `.textContent`/`.data`/`.nodeValue` updates the node's own
    // string value in place (Vue's `setText` patches text/comment anchors this way).
    match &mut doc.get_mut(id).data {
        dom::NodeData::Text(t) => {
            *t = text.to_string();
            return;
        }
        dom::NodeData::Comment(c) => {
            *c = text.to_string();
            return;
        }
        dom::NodeData::Cdata(c) => {
            *c = text.to_string();
            return;
        }
        dom::NodeData::ProcessingInstruction(p) => {
            p.data = text.to_string();
            return;
        }
        _ => {}
    }
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(id).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    // Per spec: only insert a Text node when the new value is non-empty (empty string => no child).
    if !text.is_empty() {
        doc.append_child(id, dom::NodeData::Text(text.to_string()));
    }
}

/// Parse `html` and replace `target`'s children with the resulting real nodes in the live `doc`.
pub(crate) fn set_inner_html(doc: &mut dom::Document, target: dom::NodeId, html: &str) {
    let old: Vec<dom::NodeId> = std::mem::take(&mut doc.get_mut(target).children);
    for child in old {
        doc.get_mut(child).parent = None;
    }
    let frag = html::parse(html);
    let frag_root = frag.root();
    copy_children_into(doc, target, &frag, frag_root);
}

/// Recursively copy the children of `src_node` (in `frag`) as children of `dst_parent` in `doc`.
/// Synthesized structural wrappers (`html`/`head`/`body`) are transparently descended into.
pub(crate) fn copy_children_into(
    doc: &mut dom::Document,
    dst_parent: dom::NodeId,
    frag: &dom::Document,
    src_node: dom::NodeId,
) {
    for &child in &frag.get(src_node).children {
        match &frag.get(child).data {
            dom::NodeData::Element(e) if matches!(e.tag.as_str(), "html" | "head" | "body") => {
                copy_children_into(doc, dst_parent, frag, child);
            }
            data => {
                let new_id = doc.append_child(dst_parent, data.clone());
                copy_children_into(doc, new_id, frag, child);
            }
        }
    }
}

/// Parse a full HTML document string and copy the parsed `<head>`'s children under `head` and the
/// parsed `<body>`'s children under `body` (either may be `None`). Backs DOMParser's `text/html`
/// path, which builds a fresh, independent document and fills its head/body from the string — unlike
/// `set_inner_html`, which flattens the html/head/body wrappers into a single target.
pub(crate) fn parse_html_into_sections(
    doc: &mut dom::Document,
    head: Option<dom::NodeId>,
    body: Option<dom::NodeId>,
    html: &str,
) {
    let frag = html::parse(html);
    let frag_root = frag.root();
    if let (Some(h), Some(fh)) = (head, find_by_tag(&frag, frag_root, "head")) {
        copy_children_into(doc, h, &frag, fh);
    }
    if let (Some(b), Some(fb)) = (body, find_by_tag(&frag, frag_root, "body")) {
        copy_children_into(doc, b, &frag, fb);
    }
}

/// Depth-first search for the first element whose tag equals `tag` (ASCII case-insensitive).
pub(crate) fn find_by_tag(
    doc: &dom::Document,
    root: dom::NodeId,
    tag: &str,
) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_tag(doc, child, tag) {
            return Some(found);
        }
    }
    None
}

/// Collect every element matching `tag` (ASCII case-insensitive), document order.
pub(crate) fn collect_by_tag(
    doc: &dom::Document,
    root: dom::NodeId,
    tag: &str,
    out: &mut Vec<dom::NodeId>,
) {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.tag.eq_ignore_ascii_case(tag) {
            out.push(root);
        }
    }
    let children = doc.get(root).children.clone();
    for child in children {
        collect_by_tag(doc, child, tag, out);
    }
}

/// Depth-first search for the first element with `id` equal to `id`.
pub(crate) fn find_by_id(doc: &dom::Document, root: dom::NodeId, id: &str) -> Option<dom::NodeId> {
    if let dom::NodeData::Element(e) = &doc.get(root).data {
        if e.id() == Some(id) {
            return Some(root);
        }
    }
    for &child in &doc.get(root).children {
        if let Some(found) = find_by_id(doc, child, id) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// CSS selector engine (type / .class / #id / compound / descendant). Reused verbatim.
// ---------------------------------------------------------------------------------------------
