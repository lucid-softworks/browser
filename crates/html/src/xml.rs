//! A tolerant XML parser producing a [`dom::Document`], used for top-level XML documents
//! (`image/svg+xml`, `application/xhtml+xml`, `application/xml`, `text/xml`). It is deliberately
//! lenient — it does not validate, fetch DTDs, or reject malformed input; it builds the most
//! reasonable tree it can so the engine can script and render the document.
//!
//! Namespaces are resolved via a scope stack of `xmlns`/`xmlns:prefix` bindings: each element's
//! [`ElementData::namespace`] is set to its resolved URI, and its `tag` is the local name (case
//! preserved — XML is case sensitive). Attributes are stored under their qualified name (e.g.
//! `xlink:href`, `attributeName`), matching how the JS DOM layer reads them.

use dom::{Document, ElementData, NodeData, NodeId};
use std::collections::HashMap;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Parse an XML document string into a [`dom::Document`].
pub fn parse_xml(src: &str) -> Document {
    let mut doc = Document::new();
    let root = doc.root();
    let mut p = XmlParser {
        b: src.as_bytes(),
        i: 0,
    };
    // Namespace scope stack: maps prefix ("" = default) -> URI. Seed with the implicit `xml` prefix.
    let mut scopes: Vec<HashMap<String, String>> = vec![{
        let mut m = HashMap::new();
        m.insert("xml".to_string(), XML_NS.to_string());
        m.insert("xlink".to_string(), XLINK_NS.to_string());
        m
    }];
    // The element stack (parents). When empty, new elements attach to the document root.
    let mut open: Vec<NodeId> = Vec::new();

    while p.i < p.b.len() {
        if p.starts_with(b"<?") {
            p.skip_until(b"?>");
            continue;
        }
        if p.starts_with(b"<!--") {
            let start = p.i + 4;
            let end = p.find(b"-->").unwrap_or(p.b.len());
            let text = p.slice(start, end);
            let parent = *open.last().unwrap_or(&root);
            doc.append_child(parent, NodeData::Comment(text));
            p.i = (end + 3).min(p.b.len());
            continue;
        }
        if p.starts_with(b"<![CDATA[") {
            let start = p.i + 9;
            let end = p.find(b"]]>").unwrap_or(p.b.len());
            let text = p.slice(start, end);
            if let Some(&parent) = open.last() {
                doc.append_child(parent, NodeData::Cdata(text));
            }
            p.i = (end + 3).min(p.b.len());
            continue;
        }
        if p.starts_with(b"<!") {
            // DOCTYPE or other declaration — skip to the matching '>'.
            p.skip_until(b">");
            continue;
        }
        if p.starts_with(b"</") {
            // End tag — pop the open stack and the namespace scope.
            p.i += 2;
            p.skip_name();
            p.skip_until(b">");
            if !open.is_empty() {
                open.pop();
                scopes.pop();
            }
            continue;
        }
        if p.peek() == Some(b'<') {
            // Start tag.
            p.i += 1;
            let qname = p.read_name();
            // Parse attributes.
            let mut raw_attrs: Vec<(String, String)> = Vec::new();
            let mut self_closing = false;
            loop {
                p.skip_ws();
                match p.peek() {
                    Some(b'/') => {
                        if p.b.get(p.i + 1) == Some(&b'>') {
                            self_closing = true;
                            p.i += 2;
                        } else {
                            p.i += 1;
                        }
                        break;
                    }
                    Some(b'>') => {
                        p.i += 1;
                        break;
                    }
                    None => break,
                    _ => {
                        let aname = p.read_name();
                        if aname.is_empty() {
                            p.i += 1; // avoid stalling on stray bytes
                            continue;
                        }
                        p.skip_ws();
                        let mut aval = String::new();
                        if p.peek() == Some(b'=') {
                            p.i += 1;
                            p.skip_ws();
                            aval = p.read_attr_value();
                        }
                        raw_attrs.push((aname, decode_entities(&aval)));
                    }
                }
            }

            // Build the namespace scope for this element from xmlns / xmlns:prefix attributes.
            let mut scope = scopes.last().cloned().unwrap_or_default();
            for (k, v) in &raw_attrs {
                if k == "xmlns" {
                    scope.insert("".to_string(), v.clone());
                } else if let Some(prefix) = k.strip_prefix("xmlns:") {
                    scope.insert(prefix.to_string(), v.clone());
                }
            }

            // Resolve this element's namespace from its prefix (or the default binding).
            let (prefix, local) = split_qname(&qname);
            let namespace = match prefix {
                Some(px) => scope.get(px).cloned(),
                None => scope.get("").cloned(),
            };

            let mut attrs: dom::AttrMap = dom::AttrMap::new();
            for (k, v) in raw_attrs {
                attrs.entry(k).or_insert(v);
            }

            let parent = *open.last().unwrap_or(&root);
            let node = doc.append_child(
                parent,
                NodeData::Element(ElementData {
                    tag: local.to_string(),
                    attrs,
                    namespace,
                }),
            );

            if !self_closing {
                open.push(node);
                scopes.push(scope);
            }
            continue;
        }

        // Character data up to the next '<'.
        let start = p.i;
        let end = p.find(b"<").unwrap_or(p.b.len());
        let text = p.slice(start, end);
        p.i = end;
        if let Some(&parent) = open.last() {
            let decoded = decode_entities(&text);
            // Skip whitespace-only text directly under the document root (between PIs/the root).
            doc.append_child(parent, NodeData::Text(decoded));
        }
    }

    doc
}

struct XmlParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> XmlParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn starts_with(&self, s: &[u8]) -> bool {
        self.b[self.i..].starts_with(s)
    }
    fn slice(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.b[start..end.min(self.b.len())]).into_owned()
    }
    fn find(&self, needle: &[u8]) -> Option<usize> {
        let mut j = self.i;
        while j + needle.len() <= self.b.len() {
            if self.b[j..].starts_with(needle) {
                return Some(j);
            }
            j += 1;
        }
        None
    }
    fn skip_until(&mut self, needle: &[u8]) {
        match self.find(needle) {
            Some(j) => self.i = (j + needle.len()).min(self.b.len()),
            None => self.i = self.b.len(),
        }
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }
    /// A name char: anything that isn't whitespace, or one of `/ > = ?`.
    fn is_name_byte(c: u8) -> bool {
        !matches!(
            c,
            b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>' | b'=' | b'<' | b'?'
        )
    }
    fn read_name(&mut self) -> String {
        let start = self.i;
        while let Some(c) = self.peek() {
            if Self::is_name_byte(c) {
                self.i += 1;
            } else {
                break;
            }
        }
        self.slice(start, self.i)
    }
    fn skip_name(&mut self) {
        while let Some(c) = self.peek() {
            if Self::is_name_byte(c) {
                self.i += 1;
            } else {
                break;
            }
        }
    }
    fn read_attr_value(&mut self) -> String {
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                self.i += 1;
                let start = self.i;
                while let Some(c) = self.peek() {
                    if c == q {
                        break;
                    }
                    self.i += 1;
                }
                let s = self.slice(start, self.i);
                if self.peek() == Some(q) {
                    self.i += 1;
                }
                s
            }
            _ => {
                // Unquoted value (lenient): up to whitespace or tag end.
                let start = self.i;
                while let Some(c) = self.peek() {
                    if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/') {
                        break;
                    }
                    self.i += 1;
                }
                self.slice(start, self.i)
            }
        }
    }
}

/// Split `prefix:local` into (Some(prefix), local); a name with no colon has no prefix.
fn split_qname(qname: &str) -> (Option<&str>, &str) {
    match qname.split_once(':') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() => (Some(p), l),
        _ => (None, qname),
    }
}

/// Decode the XML predefined entities and numeric character references.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'&' {
            if let Some(semi) = s[i + 1..].find(';') {
                let ent = &s[i + 1..i + 1 + semi];
                let decoded = match ent {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                        u32::from_str_radix(&ent[2..], 16)
                            .ok()
                            .and_then(char::from_u32)
                    }
                    _ if ent.starts_with('#') => {
                        ent[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i += 1 + semi + 1;
                    continue;
                }
            }
        }
        // Not a recognized entity — copy the byte (as part of a UTF-8 char).
        let ch_len = utf8_len(b[i]);
        out.push_str(&s[i..(i + ch_len).min(s.len())]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[allow(dead_code)]
const _ASSERT_NS: &[&str] = &[SVG_NS, XHTML_NS];

#[cfg(test)]
mod tests {
    use super::*;

    fn first_elem(doc: &Document, parent: NodeId) -> NodeId {
        for &c in &doc.get(parent).children {
            if let NodeData::Element(_) = doc.get(c).data {
                return c;
            }
        }
        panic!("no element child");
    }

    #[test]
    fn parses_svg_with_namespaced_script() {
        let doc = parse_xml(
            r#"<?xml version="1.0"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml">
  <h:script src="/x.js"/>
  <script><![CDATA[ var a = 1 < 2; ]]></script>
  <rect width="10"/>
</svg>"#,
        );
        let svg = first_elem(&doc, doc.root());
        match &doc.get(svg).data {
            NodeData::Element(e) => {
                assert_eq!(e.tag, "svg");
                assert_eq!(e.namespace.as_deref(), Some(SVG_NS));
            }
            _ => panic!(),
        }
        // The XHTML script keeps its src and is in the XHTML namespace.
        let kids: Vec<_> = doc.get(svg).children.clone();
        let script_h = kids
            .iter()
            .find(|&&c| matches!(&doc.get(c).data, NodeData::Element(e) if e.tag == "script" && e.namespace.as_deref() == Some(XHTML_NS)))
            .expect("xhtml script");
        if let NodeData::Element(e) = &doc.get(*script_h).data {
            assert_eq!(e.attrs.get("src").map(String::as_str), Some("/x.js"));
        }
        // The SVG inline script has a CDATA child carrying its body.
        let script_svg = kids
            .iter()
            .find(|&&c| matches!(&doc.get(c).data, NodeData::Element(e) if e.tag == "script" && e.namespace.as_deref() == Some(SVG_NS)))
            .expect("svg script");
        let cdata = doc.get(*script_svg).children[0];
        assert!(matches!(&doc.get(cdata).data, NodeData::Cdata(t) if t.contains("a = 1 < 2")));
    }
}
