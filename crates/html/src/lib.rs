//! Hand-written HTML parsing (Phase 2): a tokenizer plus a forgiving tree builder that
//! populates an arena [`dom::Document`].
//!
//! This is intentionally a pragmatic subset of the HTML5 spec. We do not implement the
//! adoption agency algorithm or `<table>`/`<select>` foster-parenting. We *do* implement a
//! small set of **insertion modes** so the final tree always has the
//! `Document → html → [head, body]` skeleton, exactly like a real browser: `document.body`
//! is never null on a real HTML page, even when the source omits `<html>`/`<head>`/`<body>`.
//!
//! ## Head vs body routing
//!
//! As tokens stream in we track an [`InsertMode`]:
//! `Initial → BeforeHtml → BeforeHead → InHead → InBody`. The first non-doctype/non-comment
//! token forces an `<html>` element (reusing an explicit `<html>` start tag if present), then
//! a `<head>`. Metadata start tags (`title`, `base`, `meta`, `link`, `style`, `script`,
//! `noscript`, `template`) stay in `<head>`; any *flow* content — a start tag that is not
//! metadata, or text with a non-whitespace character — implicitly closes `<head>`, opens
//! `<body>`, and switches to `InBody`. Once in body, everything (including late metadata and
//! content after `</body>`/`</html>`) is appended to the body subtree. Explicit
//! `<html>`/`<head>`/`<body>` tags are reused rather than duplicated.
//!
//! The goal is to produce a sensible tree for typical real-world pages and to *never panic*
//! on malformed input.

use dom::{Document, ElementData, NodeData, NodeId};

/// The SVG namespace URI (foreign content entered via an `<svg>` start tag).
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The MathML namespace URI (foreign content entered via a `<math>` start tag).
const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";

/// Elements that never have children and need no end tag.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Elements whose content is raw text (not parsed as HTML).
const RAWTEXT_ELEMENTS: &[&str] = &["script", "style"];

/// "Metadata content" start tags that belong in `<head>` when they appear before any flow
/// content. Anything not in this set is treated as flow content and opens `<body>`.
const METADATA_ELEMENTS: &[&str] = &[
    "base", "basefont", "bgsound", "link", "meta", "noscript", "script", "style", "template",
    "title",
];

fn is_void(tag: &str) -> bool {
    VOID_ELEMENTS.contains(&tag)
}

fn is_rawtext(tag: &str) -> bool {
    RAWTEXT_ELEMENTS.contains(&tag)
}

fn is_metadata(tag: &str) -> bool {
    METADATA_ELEMENTS.contains(&tag)
}

/// Where the tree builder currently is in the implied `html > head, body` skeleton. We only
/// model the handful of modes needed to route head vs body correctly; everything inside
/// `<body>` uses the lenient stack-based builder (mode stays [`InsertMode::InBody`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InsertMode {
    /// Nothing inserted yet (doctype/comments/whitespace allowed).
    Initial,
    /// `<html>` not yet open.
    BeforeHtml,
    /// `<html>` open, `<head>` not yet open.
    BeforeHead,
    /// `<head>` open and current; metadata goes here, flow content closes it.
    InHead,
    /// `<body>` open and current (or we are otherwise placing flow content).
    InBody,
}

/// Parse an HTML string into a [`dom::Document`].
pub fn parse(html: &str) -> Document {
    // Single source of truth: delegate to the streaming parser. Feeding the whole input then
    // finishing is equivalent to a one-shot parse of the full string.
    let mut p = StreamParser::new();
    p.feed(html.as_bytes());
    p.finish()
}

/// Incremental HTML parser: feed bytes as they arrive, snapshot the partial DOM at any time, and
/// finish to get the complete document.
///
/// Strategy (v1): re-parse the accumulated buffer. Each `feed` appends decoded text to an internal
/// `String`; `snapshot`/`finish` run the existing one-shot [`Parser`] over everything accumulated
/// so far. This guarantees the final DOM is byte-identical to `parse(full_input)` and yields
/// correct progressive snapshots, at the cost of re-parsing. A resumable tokenizer is a future
/// optimization.
#[derive(Default)]
pub struct StreamParser {
    /// All input decoded as valid UTF-8 so far.
    buffer: String,
    /// Trailing bytes from the last `feed` that did not yet form a complete UTF-8 scalar; they
    /// are prepended to the next chunk (or, in `finish`, decoded lossily as end-of-input).
    carry: Vec<u8>,
}

impl StreamParser {
    pub fn new() -> Self {
        StreamParser { buffer: String::new(), carry: Vec::new() }
    }

    /// Append a chunk of raw bytes. Handles a UTF-8 multibyte sequence split across chunk
    /// boundaries by buffering the trailing incomplete bytes until the next `feed`/`finish`.
    pub fn feed(&mut self, chunk: &[u8]) {
        // Combine any carried-over partial sequence with the new chunk.
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(chunk);

        match std::str::from_utf8(&bytes) {
            Ok(s) => self.buffer.push_str(s),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // Bytes before `valid_up_to` are complete, valid UTF-8.
                // Safe: `valid_up_to` is a valid boundary by definition.
                self.buffer
                    .push_str(unsafe { std::str::from_utf8_unchecked(&bytes[..valid_up_to]) });
                let remainder = &bytes[valid_up_to..];
                // If the trailing error is an incomplete (but possibly valid) sequence, carry it
                // over. Otherwise it is genuinely invalid bytes that completing won't fix, so
                // decode them now (lossily) rather than carry forever.
                if e.error_len().is_none() {
                    self.carry.extend_from_slice(remainder);
                } else {
                    self.buffer
                        .push_str(&String::from_utf8_lossy(remainder));
                }
            }
        }
    }

    /// The current partial DOM — a well-formed, renderable tree of everything parsed so far (open
    /// elements auto-closed by the lenient tree builder so layout/paint can walk it safely).
    pub fn snapshot(&self) -> Document {
        // Decode any carried-over trailing bytes lossily for this point-in-time view, without
        // consuming them from the real carry-over.
        if self.carry.is_empty() {
            Parser::new(&self.buffer).run()
        } else {
            let mut s = self.buffer.clone();
            s.push_str(&String::from_utf8_lossy(&self.carry));
            Parser::new(&s).run()
        }
    }

    /// Consume the parser and return the final complete document (decodes any buffered trailing
    /// bytes as the end of input).
    pub fn finish(mut self) -> Document {
        if !self.carry.is_empty() {
            // End of input: any leftover bytes can no longer be completed; decode lossily.
            let decoded = String::from_utf8_lossy(&self.carry).into_owned();
            self.buffer.push_str(&decoded);
            self.carry.clear();
        }
        Parser::new(&self.buffer).run()
    }
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    doc: Document,
    /// Stack of currently-open element nodes. The document root is the implicit bottom.
    open: Vec<NodeId>,
    /// Accumulated text characters, flushed to a text node when the next tag begins.
    text_buf: String,
    /// Current insertion mode (drives the implied `html > head, body` skeleton).
    mode: InsertMode,
    /// The `<html>` element once created.
    html: Option<NodeId>,
    /// The `<head>` element once created.
    head: Option<NodeId>,
    /// The `<body>` element once created.
    body: Option<NodeId>,
}

impl<'a> Parser<'a> {
    fn new(html: &'a str) -> Self {
        Parser {
            input: html.as_bytes(),
            pos: 0,
            doc: Document::new(),
            open: Vec::new(),
            text_buf: String::new(),
            mode: InsertMode::Initial,
            html: None,
            head: None,
            body: None,
        }
    }

    /// The element we should append new nodes to: the top of the open stack, or the
    /// document root if nothing is open.
    fn current_parent(&self) -> NodeId {
        *self.open.last().unwrap_or(&self.doc.root())
    }

    // ---- implied skeleton (html > head, body) ----

    /// Ensure an `<html>` element exists under the document root and is on the open stack.
    /// Reuses `attrs` only when synthesizing (an explicit `<html>` start tag is handled in
    /// `parse_start_tag`, which calls this then merges its attributes).
    fn ensure_html(&mut self) -> NodeId {
        if let Some(html) = self.html {
            return html;
        }
        let root = self.doc.root();
        let html = self.doc.append_child(
            root,
            NodeData::Element(ElementData { tag: "html".into(), attrs: Default::default(), namespace: None }),
        );
        self.html = Some(html);
        self.open.push(html);
        if self.mode == InsertMode::Initial || self.mode == InsertMode::BeforeHtml {
            self.mode = InsertMode::BeforeHead;
        }
        html
    }

    /// Ensure a `<head>` element exists under `<html>` and is current (mode `InHead`).
    fn ensure_head(&mut self) -> NodeId {
        if let Some(head) = self.head {
            return head;
        }
        let html = self.ensure_html();
        let head = self.doc.append_child(
            html,
            NodeData::Element(ElementData { tag: "head".into(), attrs: Default::default(), namespace: None }),
        );
        self.head = Some(head);
        self.open.push(head);
        self.mode = InsertMode::InHead;
        head
    }

    /// Pop the `<head>` (and anything above it) off the open stack, leaving `<html>` current.
    fn pop_head(&mut self) {
        if let Some(head) = self.head {
            if let Some(idx) = self.open.iter().rposition(|&id| id == head) {
                self.open.truncate(idx);
            }
        }
    }

    /// Ensure a `<body>` element exists under `<html>` and is current (mode `InBody`). Closes
    /// an open `<head>` first. Idempotent once body exists.
    fn ensure_body(&mut self) -> NodeId {
        if let Some(body) = self.body {
            return body;
        }
        // Make sure head exists (even if empty) so head precedes body in document order.
        let html = self.ensure_html();
        if self.head.is_none() {
            let head = self.doc.append_child(
                html,
                NodeData::Element(ElementData { tag: "head".into(), attrs: Default::default(), namespace: None }),
            );
            self.head = Some(head);
        }
        self.pop_head();
        let body = self.doc.append_child(
            html,
            NodeData::Element(ElementData { tag: "body".into(), attrs: Default::default(), namespace: None }),
        );
        self.body = Some(body);
        self.open.push(body);
        self.mode = InsertMode::InBody;
        body
    }

    fn run(mut self) -> Document {
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'<' {
                // Possible tag/comment/doctype. Peek ahead.
                if self.starts_with("<!--") {
                    self.flush_text();
                    self.parse_comment();
                } else if self.starts_with_ci("<!doctype") {
                    self.flush_text();
                    self.parse_doctype();
                } else if self.peek_at(1).map(is_tag_name_start).unwrap_or(false) {
                    self.flush_text();
                    self.parse_start_tag();
                } else if self.peek_at(1) == Some(b'/') {
                    self.flush_text();
                    self.parse_end_tag();
                } else if self.peek_at(1) == Some(b'!') || self.peek_at(1) == Some(b'?') {
                    // Bogus comment / processing instruction: consume to '>'.
                    self.flush_text();
                    self.consume_until_byte(b'>');
                    self.consume_byte(); // the '>'
                } else {
                    // A stray '<' that doesn't begin a markup construct: literal text.
                    self.text_buf.push('<');
                    self.pos += 1;
                }
            } else {
                // Text. Accumulate decoded characters until the next '<'.
                self.consume_text_until_lt();
            }
        }
        self.flush_text();
        // Guarantee the full skeleton even for empty / comment-only / head-only input so
        // `document.body` is never null. `ensure_body` synthesizes head (if missing) then body.
        self.ensure_body();
        self.doc
    }

    // ---- low-level cursor helpers ----

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.input.get(self.pos + offset).copied()
    }

    fn consume_byte(&mut self) {
        if self.pos < self.input.len() {
            self.pos += 1;
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s.as_bytes())
    }

    /// Case-insensitive `starts_with` for ASCII.
    fn starts_with_ci(&self, s: &str) -> bool {
        let bytes = s.as_bytes();
        if self.pos + bytes.len() > self.input.len() {
            return false;
        }
        self.input[self.pos..self.pos + bytes.len()]
            .iter()
            .zip(bytes)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    fn consume_until_byte(&mut self, byte: u8) {
        while let Some(c) = self.peek_at(0) {
            if c == byte {
                break;
            }
            self.pos += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_at(0) {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    // ---- text ----

    /// Consume a run of text (not raw text), decoding character references, until the next
    /// `<` or end of input.
    fn consume_text_until_lt(&mut self) {
        while let Some(c) = self.peek_at(0) {
            if c == b'<' {
                break;
            }
            if c == b'&' {
                self.consume_entity_into_text();
            } else {
                // Copy one UTF-8 scalar. We operate on bytes but the input came from a
                // &str, so byte boundaries that are not '<'/'&' are safe to copy verbatim.
                let start = self.pos;
                self.pos += 1;
                // Extend over continuation bytes of a multi-byte UTF-8 sequence.
                while let Some(b) = self.peek_at(0) {
                    if b & 0b1100_0000 == 0b1000_0000 {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                // Safe: bytes [start, self.pos) form complete UTF-8 from the original &str.
                let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("");
                self.text_buf.push_str(s);
            }
        }
    }

    /// Decode a character reference starting at the current `&` into `text_buf`. On any
    /// malformed reference, the literal `&` (and consumed chars) pass through.
    fn consume_entity_into_text(&mut self) {
        let decoded = self.read_entity();
        self.text_buf.push_str(&decoded);
    }

    /// Read a character reference at the cursor (which is on `&`). Advances the cursor and
    /// returns the decoded string, or the literal text if it is not a valid reference.
    fn read_entity(&mut self) -> String {
        debug_assert_eq!(self.peek_at(0), Some(b'&'));
        let amp_start = self.pos;
        self.pos += 1; // consume '&'

        if self.peek_at(0) == Some(b'#') {
            self.pos += 1; // consume '#'
            let hex = matches!(self.peek_at(0), Some(b'x') | Some(b'X'));
            if hex {
                self.pos += 1;
            }
            let digits_start = self.pos;
            while let Some(c) = self.peek_at(0) {
                let ok = if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() };
                if ok {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.pos == digits_start {
                // No digits: not a valid numeric reference.
                return self.literal_from(amp_start);
            }
            let digits = std::str::from_utf8(&self.input[digits_start..self.pos]).unwrap_or("");
            let code = u32::from_str_radix(digits, if hex { 16 } else { 10 }).ok();
            // Optional trailing semicolon.
            if self.peek_at(0) == Some(b';') {
                self.pos += 1;
            }
            return match code.and_then(char::from_u32) {
                Some(ch) => ch.to_string(),
                None => '\u{FFFD}'.to_string(),
            };
        }

        // Named reference: read ASCII alphanumerics.
        let name_start = self.pos;
        while let Some(c) = self.peek_at(0) {
            if c.is_ascii_alphanumeric() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == name_start {
            return self.literal_from(amp_start);
        }
        let name = std::str::from_utf8(&self.input[name_start..self.pos]).unwrap_or("");
        if let Some(replacement) = named_entity(name) {
            // Consume an optional trailing semicolon.
            if self.peek_at(0) == Some(b';') {
                self.pos += 1;
            }
            replacement.to_string()
        } else {
            // Unknown entity: pass through literally (the '&' + name we consumed).
            self.literal_from(amp_start)
        }
    }

    /// Return the literal input bytes from `start` to the current cursor as a string.
    fn literal_from(&self, start: usize) -> String {
        std::str::from_utf8(&self.input[start..self.pos])
            .unwrap_or("")
            .to_string()
    }

    fn flush_text(&mut self) {
        if self.text_buf.is_empty() {
            return;
        }
        // Before we're in body, only non-whitespace text that would land directly under a
        // skeleton container (root / html / head) counts as flow content: it implicitly closes
        // `<head>` and opens `<body>`. Text inside an open element (e.g. `<title>`) just appends
        // there. Pure inter-element whitespace before body is dropped (matching browsers — it
        // would otherwise become stray text under root/head).
        if self.mode != InsertMode::InBody {
            let parent = self.current_parent();
            let in_container = Some(parent) == self.html
                || Some(parent) == self.head
                || parent == self.doc.root();
            if in_container {
                if self.text_buf.trim().is_empty() {
                    self.text_buf.clear();
                    return;
                }
                self.ensure_body();
            }
        }
        let text = std::mem::take(&mut self.text_buf);
        let parent = self.current_parent();
        self.doc.append_child(parent, NodeData::Text(text));
    }

    // ---- comments / doctype ----

    fn parse_comment(&mut self) {
        // cursor on "<!--"
        self.pos += 4;
        let start = self.pos;
        let end = loop {
            if self.pos >= self.input.len() {
                break self.input.len();
            }
            if self.input[self.pos..].starts_with(b"-->") {
                break self.pos;
            }
            self.pos += 1;
        };
        let content = std::str::from_utf8(&self.input[start..end]).unwrap_or("").to_string();
        if self.pos < self.input.len() {
            self.pos += 3; // consume "-->"
        }
        let parent = self.current_parent();
        self.doc.append_child(parent, NodeData::Comment(content));
    }

    fn parse_doctype(&mut self) {
        // cursor on "<!doctype". Skip the keyword, capture the name token (e.g. "html"), then
        // consume the rest up to '>'. Public/system identifiers are not parsed (left empty).
        self.pos += "<!doctype".len();
        self.skip_whitespace();
        let start = self.pos;
        while self.pos < self.input.len() {
            let b = self.input[self.pos];
            if b == b'>' || b.is_ascii_whitespace() {
                break;
            }
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.input[start..self.pos])
            .unwrap_or("")
            .to_ascii_lowercase();
        self.consume_until_byte(b'>');
        self.consume_byte();
        // A doctype is only inserted before the root element (Initial insertion mode). Append it to
        // the document root so `document.doctype` reflects it.
        let root = self.doc.root();
        self.doc.append_child(
            root,
            NodeData::DocumentType(dom::DoctypeData {
                name,
                public_id: String::new(),
                system_id: String::new(),
            }),
        );
    }

    // ---- tags ----

    fn parse_start_tag(&mut self) {
        // cursor on '<'
        self.pos += 1;
        let tag = self.read_tag_name();
        if tag.is_empty() {
            // Shouldn't happen given the caller's check, but be defensive.
            return;
        }
        let mut attrs: dom::AttrMap = dom::AttrMap::new();
        let mut self_closing = false;

        loop {
            self.skip_whitespace();
            match self.peek_at(0) {
                None => break,
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b'/') => {
                    self.pos += 1;
                    if self.peek_at(0) == Some(b'>') {
                        self_closing = true;
                        self.pos += 1;
                        break;
                    }
                    // stray '/', keep going
                }
                _ => {
                    let (name, value) = self.read_attribute();
                    if !name.is_empty() {
                        attrs.entry(name).or_insert(value);
                    } else {
                        // No progress would loop forever; force advance.
                        self.consume_byte();
                    }
                }
            }
        }

        // ---- skeleton handling: html / head / body and head-vs-body routing ----
        // The `<html>`, `<head>`, `<body>` tags reuse the implied elements rather than creating
        // duplicates; their attributes are merged onto the existing element.
        match tag.as_str() {
            "html" => {
                let html = self.ensure_html();
                self.merge_attrs(html, attrs);
                return;
            }
            "head" => {
                // An explicit <head>: ensure it (creating html first), make it current.
                let head = self.ensure_head();
                self.merge_attrs(head, attrs);
                return;
            }
            "body" => {
                let body = self.ensure_body();
                self.merge_attrs(body, attrs);
                return;
            }
            _ => {}
        }

        // Decide the destination based on the current mode and whether this is metadata.
        match self.mode {
            InsertMode::Initial | InsertMode::BeforeHtml | InsertMode::BeforeHead => {
                if is_metadata(&tag) {
                    self.ensure_head();
                } else {
                    self.ensure_body();
                }
            }
            InsertMode::InHead => {
                if !is_metadata(&tag) {
                    // First flow content: implicitly close head, open body.
                    self.ensure_body();
                }
                // Metadata stays in head (current parent is head).
            }
            InsertMode::InBody => {}
        }

        let parent = self.current_parent();
        // Foreign content: an `<svg>` (or `<math>`) start tag enters the SVG (or MathML) namespace,
        // and descendants inherit it until the foreign root closes. We don't implement the full
        // foreign-content algorithm, but tracking the namespace this way is enough for namespaced
        // selector matching (`@namespace svg url(...)` + `svg|rect`).
        let namespace = {
            let parent_ns = match &self.doc.get(parent).data {
                NodeData::Element(e) => e.namespace.clone(),
                _ => None,
            };
            if tag.eq_ignore_ascii_case("svg") {
                Some(SVG_NS.to_string())
            } else if tag.eq_ignore_ascii_case("math") {
                Some(MATHML_NS.to_string())
            } else {
                parent_ns
            }
        };
        let node = self.doc.append_child(
            parent,
            NodeData::Element(ElementData { tag: tag.clone(), attrs, namespace }),
        );

        if is_void(&tag) || self_closing {
            // Void / self-closing elements don't open a scope.
            return;
        }

        if is_rawtext(&tag) {
            // Raw-text element: consume everything up to the matching end tag as one text
            // node and do not push onto the open stack.
            self.consume_rawtext(&tag, node);
            return;
        }

        self.open.push(node);
    }

    /// Merge `attrs` onto an existing element, keeping already-present attributes (first wins,
    /// matching how duplicate `<html>`/`<body>` start tags behave).
    fn merge_attrs(&mut self, node: NodeId, attrs: dom::AttrMap) {
        if attrs.is_empty() {
            return;
        }
        if let NodeData::Element(e) = &mut self.doc.get_mut(node).data {
            for (k, v) in attrs {
                e.attrs.entry(k).or_insert(v);
            }
        }
    }

    /// Consume raw text up to (but not parsing) `</tag>`, appending it as a single text
    /// child of `node`.
    fn consume_rawtext(&mut self, tag: &str, node: NodeId) {
        let close = format!("</{tag}");
        let start = self.pos;
        let end = loop {
            if self.pos >= self.input.len() {
                break self.input.len();
            }
            if self.starts_with_ci(&close) {
                break self.pos;
            }
            self.pos += 1;
        };
        let content = std::str::from_utf8(&self.input[start..end]).unwrap_or("").to_string();
        if !content.is_empty() {
            self.doc.append_child(node, NodeData::Text(content));
        }
        // Consume the end tag if present.
        if self.pos < self.input.len() {
            // skip "</tag" then up to and including '>'
            self.pos += close.len();
            self.consume_until_byte(b'>');
            self.consume_byte();
        }
    }

    fn parse_end_tag(&mut self) {
        // cursor on '<', next is '/'
        self.pos += 2;
        let tag = self.read_tag_name();
        // Consume to '>'.
        self.consume_until_byte(b'>');
        self.consume_byte();
        if tag.is_empty() {
            return;
        }

        // Skeleton end tags get special, lenient handling so the head/body shape is preserved.
        match tag.as_str() {
            "head" => {
                // Close head and move to body for subsequent content. (Per spec the mode goes
                // to "after head"; we open body lazily, but pop head off the stack now so it is
                // no longer current.)
                if self.head.is_some() {
                    self.pop_head();
                    if self.mode == InsertMode::InHead {
                        // No body yet; switch out of head so the next flow content opens body.
                        self.mode = InsertMode::BeforeHead;
                    }
                }
                return;
            }
            "body" | "html" => {
                // Lenient: content after `</body>`/`</html>` still goes in body. Ensure body
                // exists but do NOT pop it off the open stack, so later nodes land in body.
                self.ensure_body();
                return;
            }
            _ => {}
        }

        // Find nearest matching open element.
        if let Some(idx) = self.open.iter().rposition(|&id| {
            matches!(&self.doc.get(id).data, NodeData::Element(e) if e.tag == tag)
        }) {
            // Don't pop past the body element: a stray `</div>` must never unwind body/html.
            let floor = self
                .body
                .and_then(|b| self.open.iter().rposition(|&id| id == b))
                .map(|i| i + 1)
                .unwrap_or(0);
            if idx >= floor {
                // Pop everything above it, plus the match itself.
                self.open.truncate(idx);
            }
        }
        // If there's no match, ignore the stray end tag.
    }

    /// Read a lowercased tag name at the cursor.
    fn read_tag_name(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek_at(0) {
            if is_tag_name_char(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Read one attribute (name plus optional value) at the cursor.
    fn read_attribute(&mut self) -> (String, String) {
        // Read name.
        let name_start = self.pos;
        while let Some(c) = self.peek_at(0) {
            if c.is_ascii_whitespace() || c == b'=' || c == b'>' || c == b'/' {
                break;
            }
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.input[name_start..self.pos])
            .unwrap_or("")
            .to_ascii_lowercase();
        if name.is_empty() {
            return (String::new(), String::new());
        }

        self.skip_whitespace();
        if self.peek_at(0) != Some(b'=') {
            // Boolean attribute.
            return (name, String::new());
        }
        self.pos += 1; // consume '='
        self.skip_whitespace();

        let value = match self.peek_at(0) {
            Some(q @ (b'"' | b'\'')) => {
                self.pos += 1; // opening quote
                let v = self.read_attr_value_until(|c| c == q);
                if self.peek_at(0) == Some(q) {
                    self.pos += 1; // closing quote
                }
                v
            }
            _ => self.read_attr_value_until(|c| c.is_ascii_whitespace() || c == b'>'),
        };
        (name, value)
    }

    /// Read an attribute value with entity decoding, stopping when `stop` matches (the
    /// stop byte is not consumed).
    fn read_attr_value_until(&mut self, stop: impl Fn(u8) -> bool) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek_at(0) {
            if stop(c) {
                break;
            }
            if c == b'&' {
                out.push_str(&self.read_entity());
            } else {
                let start = self.pos;
                self.pos += 1;
                while let Some(b) = self.peek_at(0) {
                    if b & 0b1100_0000 == 0b1000_0000 {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                out.push_str(std::str::from_utf8(&self.input[start..self.pos]).unwrap_or(""));
            }
        }
        out
    }
}

fn is_tag_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

fn is_tag_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_'
}

/// Decode a named character reference. Returns `None` for unknown names.
///
/// Covers the comprehensive HTML5 named character reference set: core, Latin-1
/// supplement (accented letters, typography, currency), mathematical operators,
/// Greek letters, arrows, and miscellaneous symbols. Unknown names return `None`
/// so the tokenizer can pass them through literally.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        // --- Core ---
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00A0}",

        // --- Latin-1 punctuation & symbols ---
        "iexcl" => "\u{00A1}",
        "cent" => "\u{00A2}",
        "pound" => "\u{00A3}",
        "curren" => "\u{00A4}",
        "yen" => "\u{00A5}",
        "brvbar" => "\u{00A6}",
        "sect" => "\u{00A7}",
        "uml" => "\u{00A8}",
        "copy" => "\u{00A9}",
        "ordf" => "\u{00AA}",
        "laquo" => "\u{00AB}",
        "not" => "\u{00AC}",
        "shy" => "\u{00AD}",
        "reg" => "\u{00AE}",
        "macr" => "\u{00AF}",
        "deg" => "\u{00B0}",
        "plusmn" => "\u{00B1}",
        "sup2" => "\u{00B2}",
        "sup3" => "\u{00B3}",
        "acute" => "\u{00B4}",
        "micro" => "\u{00B5}",
        "para" => "\u{00B6}",
        "middot" => "\u{00B7}",
        "cedil" => "\u{00B8}",
        "sup1" => "\u{00B9}",
        "ordm" => "\u{00BA}",
        "raquo" => "\u{00BB}",
        "frac14" => "\u{00BC}",
        "frac12" => "\u{00BD}",
        "frac34" => "\u{00BE}",
        "iquest" => "\u{00BF}",
        "times" => "\u{00D7}",
        "divide" => "\u{00F7}",

        // --- Latin-1 accented letters (uppercase) ---
        "Agrave" => "\u{00C0}",
        "Aacute" => "\u{00C1}",
        "Acirc" => "\u{00C2}",
        "Atilde" => "\u{00C3}",
        "Auml" => "\u{00C4}",
        "Aring" => "\u{00C5}",
        "AElig" => "\u{00C6}",
        "Ccedil" => "\u{00C7}",
        "Egrave" => "\u{00C8}",
        "Eacute" => "\u{00C9}",
        "Ecirc" => "\u{00CA}",
        "Euml" => "\u{00CB}",
        "Igrave" => "\u{00CC}",
        "Iacute" => "\u{00CD}",
        "Icirc" => "\u{00CE}",
        "Iuml" => "\u{00CF}",
        "ETH" => "\u{00D0}",
        "Ntilde" => "\u{00D1}",
        "Ograve" => "\u{00D2}",
        "Oacute" => "\u{00D3}",
        "Ocirc" => "\u{00D4}",
        "Otilde" => "\u{00D5}",
        "Ouml" => "\u{00D6}",
        "Oslash" => "\u{00D8}",
        "Ugrave" => "\u{00D9}",
        "Uacute" => "\u{00DA}",
        "Ucirc" => "\u{00DB}",
        "Uuml" => "\u{00DC}",
        "Yacute" => "\u{00DD}",
        "THORN" => "\u{00DE}",
        "szlig" => "\u{00DF}",

        // --- Latin-1 accented letters (lowercase) ---
        "agrave" => "\u{00E0}",
        "aacute" => "\u{00E1}",
        "acirc" => "\u{00E2}",
        "atilde" => "\u{00E3}",
        "auml" => "\u{00E4}",
        "aring" => "\u{00E5}",
        "aelig" => "\u{00E6}",
        "ccedil" => "\u{00E7}",
        "egrave" => "\u{00E8}",
        "eacute" => "\u{00E9}",
        "ecirc" => "\u{00EA}",
        "euml" => "\u{00EB}",
        "igrave" => "\u{00EC}",
        "iacute" => "\u{00ED}",
        "icirc" => "\u{00EE}",
        "iuml" => "\u{00EF}",
        "eth" => "\u{00F0}",
        "ntilde" => "\u{00F1}",
        "ograve" => "\u{00F2}",
        "oacute" => "\u{00F3}",
        "ocirc" => "\u{00F4}",
        "otilde" => "\u{00F5}",
        "ouml" => "\u{00F6}",
        "oslash" => "\u{00F8}",
        "ugrave" => "\u{00F9}",
        "uacute" => "\u{00FA}",
        "ucirc" => "\u{00FB}",
        "uuml" => "\u{00FC}",
        "yacute" => "\u{00FD}",
        "thorn" => "\u{00FE}",
        "yuml" => "\u{00FF}",

        // --- Latin Extended / letterlike ---
        "OElig" => "\u{0152}",
        "oelig" => "\u{0153}",
        "Scaron" => "\u{0160}",
        "scaron" => "\u{0161}",
        "Yuml" => "\u{0178}",
        "fnof" => "\u{0192}",
        "circ" => "\u{02C6}",
        "tilde" => "\u{02DC}",

        // --- Greek (uppercase) ---
        "Alpha" => "\u{0391}",
        "Beta" => "\u{0392}",
        "Gamma" => "\u{0393}",
        "Delta" => "\u{0394}",
        "Epsilon" => "\u{0395}",
        "Zeta" => "\u{0396}",
        "Eta" => "\u{0397}",
        "Theta" => "\u{0398}",
        "Iota" => "\u{0399}",
        "Kappa" => "\u{039A}",
        "Lambda" => "\u{039B}",
        "Mu" => "\u{039C}",
        "Nu" => "\u{039D}",
        "Xi" => "\u{039E}",
        "Omicron" => "\u{039F}",
        "Pi" => "\u{03A0}",
        "Rho" => "\u{03A1}",
        "Sigma" => "\u{03A3}",
        "Tau" => "\u{03A4}",
        "Upsilon" => "\u{03A5}",
        "Phi" => "\u{03A6}",
        "Chi" => "\u{03A7}",
        "Psi" => "\u{03A8}",
        "Omega" => "\u{03A9}",

        // --- Greek (lowercase) ---
        "alpha" => "\u{03B1}",
        "beta" => "\u{03B2}",
        "gamma" => "\u{03B3}",
        "delta" => "\u{03B4}",
        "epsilon" => "\u{03B5}",
        "zeta" => "\u{03B6}",
        "eta" => "\u{03B7}",
        "theta" => "\u{03B8}",
        "iota" => "\u{03B9}",
        "kappa" => "\u{03BA}",
        "lambda" => "\u{03BB}",
        "mu" => "\u{03BC}",
        "nu" => "\u{03BD}",
        "xi" => "\u{03BE}",
        "omicron" => "\u{03BF}",
        "pi" => "\u{03C0}",
        "rho" => "\u{03C1}",
        "sigmaf" => "\u{03C2}",
        "sigma" => "\u{03C3}",
        "tau" => "\u{03C4}",
        "upsilon" => "\u{03C5}",
        "phi" => "\u{03C6}",
        "chi" => "\u{03C7}",
        "psi" => "\u{03C8}",
        "omega" => "\u{03C9}",
        "thetasym" => "\u{03D1}",
        "upsih" => "\u{03D2}",
        "piv" => "\u{03D6}",

        // --- General punctuation / typography ---
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200C}",
        "zwj" => "\u{200D}",
        "lrm" => "\u{200E}",
        "rlm" => "\u{200F}",
        "ndash" => "\u{2013}",
        "mdash" => "\u{2014}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "sbquo" => "\u{201A}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "bdquo" => "\u{201E}",
        "dagger" => "\u{2020}",
        "Dagger" => "\u{2021}",
        "bull" => "\u{2022}",
        "hellip" => "\u{2026}",
        "permil" => "\u{2030}",
        "prime" => "\u{2032}",
        "Prime" => "\u{2033}",
        "lsaquo" => "\u{2039}",
        "rsaquo" => "\u{203A}",
        "oline" => "\u{203E}",
        "frasl" => "\u{2044}",
        "euro" => "\u{20AC}",

        // --- Letterlike symbols ---
        "weierp" => "\u{2118}",
        "image" => "\u{2111}",
        "real" => "\u{211C}",
        "trade" => "\u{2122}",
        "alefsym" => "\u{2135}",

        // --- Arrows ---
        "larr" => "\u{2190}",
        "uarr" => "\u{2191}",
        "rarr" => "\u{2192}",
        "darr" => "\u{2193}",
        "harr" => "\u{2194}",
        "crarr" => "\u{21B5}",
        "lArr" => "\u{21D0}",
        "uArr" => "\u{21D1}",
        "rArr" => "\u{21D2}",
        "dArr" => "\u{21D3}",
        "hArr" => "\u{21D4}",

        // --- Mathematical operators ---
        "forall" => "\u{2200}",
        "part" => "\u{2202}",
        "exist" => "\u{2203}",
        "empty" => "\u{2205}",
        "nabla" => "\u{2207}",
        "isin" => "\u{2208}",
        "notin" => "\u{2209}",
        "ni" => "\u{220B}",
        "prod" => "\u{220F}",
        "sum" => "\u{2211}",
        "minus" => "\u{2212}",
        "lowast" => "\u{2217}",
        "radic" => "\u{221A}",
        "prop" => "\u{221D}",
        "infin" => "\u{221E}",
        "ang" => "\u{2220}",
        "and" => "\u{2227}",
        "or" => "\u{2228}",
        "cap" => "\u{2229}",
        "cup" => "\u{222A}",
        "int" => "\u{222B}",
        "there4" => "\u{2234}",
        "sim" => "\u{223C}",
        "cong" => "\u{2245}",
        "asymp" => "\u{2248}",
        "ne" => "\u{2260}",
        "equiv" => "\u{2261}",
        "le" => "\u{2264}",
        "ge" => "\u{2265}",
        "sub" => "\u{2282}",
        "sup" => "\u{2283}",
        "nsub" => "\u{2284}",
        "sube" => "\u{2286}",
        "supe" => "\u{2287}",
        "oplus" => "\u{2295}",
        "otimes" => "\u{2297}",
        "perp" => "\u{22A5}",
        "sdot" => "\u{22C5}",

        // --- Miscellaneous technical ---
        "lceil" => "\u{2308}",
        "rceil" => "\u{2309}",
        "lfloor" => "\u{230A}",
        "rfloor" => "\u{230B}",
        "lang" => "\u{2329}",
        "rang" => "\u{232A}",

        // --- Geometric / misc symbols ---
        "loz" => "\u{25CA}",
        "spades" => "\u{2660}",
        "clubs" => "\u{2663}",
        "hearts" => "\u{2665}",
        "diams" => "\u{2666}",
        "star" => "\u{2606}",

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: collect child node ids of `id`.
    fn children(doc: &Document, id: NodeId) -> Vec<NodeId> {
        doc.get(id).children.clone()
    }

    fn tag_of(doc: &Document, id: NodeId) -> &str {
        match &doc.get(id).data {
            NodeData::Element(e) => &e.tag,
            _ => "",
        }
    }

    fn text_of(doc: &Document, id: NodeId) -> Option<&str> {
        match &doc.get(id).data {
            NodeData::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The `<html>` element (always the first/only child of root after normalization).
    fn html_of(doc: &Document) -> NodeId {
        children(doc, doc.root())
            .into_iter()
            .find(|&id| tag_of(doc, id) == "html")
            .expect("document should always have <html>")
    }

    /// The `<head>` element.
    fn head_of(doc: &Document) -> NodeId {
        children(doc, html_of(doc))
            .into_iter()
            .find(|&id| tag_of(doc, id) == "head")
            .expect("document should always have <head>")
    }

    /// The `<body>` element.
    fn body_of(doc: &Document) -> NodeId {
        children(doc, html_of(doc))
            .into_iter()
            .find(|&id| tag_of(doc, id) == "body")
            .expect("document should always have <body>")
    }

    #[test]
    fn empty_parse_has_root_node_zero() {
        let doc = parse("");
        assert_eq!(doc.root(), dom::NodeId(0));
    }

    #[test]
    fn nests_p_under_body_under_html() {
        let doc = parse("<html><body><p>hi</p></body></html>");
        let root = doc.root();
        let html = children(&doc, root)[0];
        assert_eq!(tag_of(&doc, html), "html");
        // html > head, body — head is synthesized even though the source omitted it.
        let html_kids = children(&doc, html);
        assert_eq!(tag_of(&doc, html_kids[0]), "head");
        let body = html_kids[1];
        assert_eq!(tag_of(&doc, body), "body");
        let p = children(&doc, body)[0];
        assert_eq!(tag_of(&doc, p), "p");
        let text = children(&doc, p)[0];
        assert_eq!(text_of(&doc, text), Some("hi"));
    }

    #[test]
    fn parses_attributes_quoted_unquoted_boolean() {
        let doc = parse(r#"<input type="text" name='n' size=10 disabled>"#);
        // `<input>` is flow content → goes in body.
        let input = children(&doc, body_of(&doc))[0];
        let attrs = match &doc.get(input).data {
            NodeData::Element(e) => &e.attrs,
            _ => panic!("expected element"),
        };
        assert_eq!(attrs.get("type").map(String::as_str), Some("text"));
        assert_eq!(attrs.get("name").map(String::as_str), Some("n"));
        assert_eq!(attrs.get("size").map(String::as_str), Some("10"));
        assert_eq!(attrs.get("disabled").map(String::as_str), Some(""));
    }

    #[test]
    fn void_element_has_no_children_and_siblings_attach() {
        let doc = parse("<div><img src=x>after</div>");
        let div = children(&doc, body_of(&doc))[0];
        let kids = children(&doc, div);
        assert_eq!(tag_of(&doc, kids[0]), "img");
        assert!(children(&doc, kids[0]).is_empty());
        // The text "after" is a sibling of img under div, not a child of img.
        assert_eq!(text_of(&doc, kids[1]), Some("after"));
    }

    #[test]
    fn decodes_entities_in_text() {
        let doc = parse("<p>a &amp; b &lt;c&gt; &#39;x&#39; &#x41; &nbsp;&copy;</p>");
        let p = children(&doc, body_of(&doc))[0];
        let t = children(&doc, p)[0];
        assert_eq!(text_of(&doc, t), Some("a & b <c> 'x' A \u{00A0}\u{00A9}"));
    }

    #[test]
    fn decodes_comprehensive_named_entities() {
        let doc = parse(
            "<p>&middot;&mdash;&hellip;&rarr;&eacute;&deg;&times;&frac12;&alpha;&euro;&trade;&copy;</p>",
        );
        let p = children(&doc, body_of(&doc))[0];
        let t = children(&doc, p)[0];
        assert_eq!(
            text_of(&doc, t),
            Some("\u{00B7}\u{2014}\u{2026}\u{2192}\u{00E9}\u{00B0}\u{00D7}\u{00BD}\u{03B1}\u{20AC}\u{2122}\u{00A9}")
        );
    }

    #[test]
    fn unknown_entity_passes_through() {
        let doc = parse("<p>5 &notreal; x</p>");
        let p = children(&doc, body_of(&doc))[0];
        let t = children(&doc, p)[0];
        assert_eq!(text_of(&doc, t), Some("5 &notreal; x"));
    }

    #[test]
    fn creates_comment_node() {
        let doc = parse("<div><!-- hello --></div>");
        let div = children(&doc, body_of(&doc))[0];
        let c = children(&doc, div)[0];
        match &doc.get(c).data {
            NodeData::Comment(s) => assert_eq!(s, " hello "),
            other => panic!("expected comment, got {other:?}"),
        }
    }

    #[test]
    fn script_rawtext_is_not_parsed() {
        let doc = parse("<script>if (a<b) { x = 0; }</script>");
        // A leading <script> is metadata content → lives in <head>.
        let script = children(&doc, head_of(&doc))[0];
        assert_eq!(tag_of(&doc, script), "script");
        let kids = children(&doc, script);
        assert_eq!(kids.len(), 1);
        assert_eq!(text_of(&doc, kids[0]), Some("if (a<b) { x = 0; }"));
        // No bogus <b> element anywhere.
        for n in 0..doc.len() {
            assert_ne!(tag_of(&doc, NodeId(n)), "b");
        }
    }

    #[test]
    fn style_rawtext_is_not_parsed() {
        let doc = parse("<style>a > b { color: red }</style>");
        // <style> is metadata content → lives in <head>.
        let style = children(&doc, head_of(&doc))[0];
        assert_eq!(tag_of(&doc, style), "style");
        let kids = children(&doc, style);
        assert_eq!(text_of(&doc, kids[0]), Some("a > b { color: red }"));
    }

    #[test]
    fn doctype_produces_node() {
        let doc = parse("<!DOCTYPE html><html></html>");
        let kids = children(&doc, doc.root());
        // A DocumentType node (named "html") followed by the html element.
        assert_eq!(kids.len(), 2);
        match &doc.get(kids[0]).data {
            NodeData::DocumentType(d) => assert_eq!(d.name, "html"),
            other => panic!("expected DocumentType, got {other:?}"),
        }
        assert_eq!(tag_of(&doc, kids[1]), "html");
    }

    #[test]
    fn self_closing_tag() {
        let doc = parse("<div><br/>x</div>");
        let div = children(&doc, body_of(&doc))[0];
        let kids = children(&doc, div);
        assert_eq!(tag_of(&doc, kids[0]), "br");
        assert!(children(&doc, kids[0]).is_empty());
        assert_eq!(text_of(&doc, kids[1]), Some("x"));
    }

    #[test]
    fn mismatched_end_tag_pops_to_ancestor() {
        // </b> has no match; should be ignored. </span> closes span.
        let doc = parse("<span>a</b>b</span>c");
        let body = body_of(&doc);
        let span = children(&doc, body)[0];
        assert_eq!(tag_of(&doc, span), "span");
        // span has two text children "a" and "b".
        let kids = children(&doc, span);
        assert_eq!(text_of(&doc, kids[0]), Some("a"));
        assert_eq!(text_of(&doc, kids[1]), Some("b"));
        // "c" is a sibling of span under body.
        let body_kids = children(&doc, body);
        assert_eq!(text_of(&doc, body_kids[1]), Some("c"));
    }

    #[test]
    fn unbalanced_input_does_not_panic() {
        // A grab-bag of malformed constructs.
        let _ = parse("<<<>>> <div class=>< <!-- unterminated <p attr");
        let _ = parse("</></p></div>< & &# &#x &;");
        let _ = parse("<script>unterminated");
        let _ = parse("<!DOCTYPE");
        let _ = parse("<a href=\"unclosed quote>text");
        // If we got here without panicking, the test passes.
    }

    #[test]
    fn tag_names_are_lowercased() {
        let doc = parse("<DIV><SPAN>x</SPAN></DIV>");
        let div = children(&doc, body_of(&doc))[0];
        assert_eq!(tag_of(&doc, div), "div");
        let span = children(&doc, div)[0];
        assert_eq!(tag_of(&doc, span), "span");
    }

    // ---- StreamParser tests ----

    /// Total node count, used to compare tree structure between parses.
    fn node_count(doc: &Document) -> usize {
        doc.len()
    }

    #[test]
    fn stream_feed_in_two_halves_equals_one_shot() {
        let whole = "<html><body><p>Hello, world</p><div class=\"x\">more</div></body></html>";
        // Split mid-tag: the first half ends inside `<div`.
        let split = whole.find("class").unwrap() - 4; // somewhere inside the <div tag
        let (a, b) = whole.split_at(split);

        let mut p = StreamParser::new();
        p.feed(a.as_bytes());
        p.feed(b.as_bytes());
        let streamed = p.finish();

        let one_shot = parse(whole);
        assert_eq!(node_count(&streamed), node_count(&one_shot));

        // Structural spot-check: html > head, body; body > p with text "Hello, world".
        let html = children(&streamed, streamed.root())[0];
        assert_eq!(tag_of(&streamed, html), "html");
        let body = body_of(&streamed);
        let para = children(&streamed, body)[0];
        let text = children(&streamed, para)[0];
        assert_eq!(text_of(&streamed, text), Some("Hello, world"));
    }

    #[test]
    fn stream_multibyte_utf8_split_across_feeds() {
        let whole = "<p>café 🎉</p>";
        let bytes = whole.as_bytes();
        // Find a split point that lands in the middle of the emoji's 4-byte sequence.
        let emoji_start = whole.find('🎉').unwrap();
        let split = emoji_start + 2; // mid-emoji byte boundary

        let mut p = StreamParser::new();
        p.feed(&bytes[..split]);
        p.feed(&bytes[split..]);
        let doc = p.finish();

        let para = children(&doc, body_of(&doc))[0];
        let text = children(&doc, para)[0];
        assert_eq!(text_of(&doc, text), Some("café 🎉"));
    }

    #[test]
    fn stream_snapshot_on_truncated_prefix_does_not_panic() {
        let mut p = StreamParser::new();
        p.feed(b"<html><body><p>Hello");
        let doc = p.snapshot();

        // The lenient tree builder auto-closes open elements; the partial text is present.
        let body = body_of(&doc);
        let para = children(&doc, body)[0];
        let text = children(&doc, para)[0];
        assert_eq!(text_of(&doc, text), Some("Hello"));

        // Snapshotting again with a truncated open tag must also not panic.
        p.feed(b"<div class=\"");
        let _ = p.snapshot();
    }

    #[test]
    fn stream_regression_matches_parse() {
        let cases = [
            "<html><body><p>hi</p></body></html>",
            r#"<input type="text" name='n' size=10 disabled>"#,
            "<div><img src=x>after</div>",
            "<script>if (a<b) { x = 0; }</script>",
            "<span>a</b>b</span>c",
        ];
        for case in cases {
            let mut p = StreamParser::new();
            p.feed(case.as_bytes());
            let streamed = p.finish();
            let one_shot = parse(case);
            assert_eq!(
                node_count(&streamed),
                node_count(&one_shot),
                "node count mismatch for {case:?}"
            );
        }
    }

    // ---- skeleton (html > head, body) tests ----

    /// Every parse yields exactly one `<html>` containing `<head>` then `<body>` (in that order).
    fn assert_skeleton(doc: &Document) {
        let root_kids: Vec<_> = children(doc, doc.root())
            .into_iter()
            .filter(|&id| tag_of(doc, id) == "html")
            .collect();
        assert_eq!(root_kids.len(), 1, "exactly one <html>");
        let html_kids: Vec<_> = children(doc, root_kids[0])
            .into_iter()
            .filter(|&id| matches!(tag_of(doc, id), "head" | "body"))
            .collect();
        assert_eq!(tag_of(doc, html_kids[0]), "head", "head first");
        assert_eq!(tag_of(doc, html_kids[1]), "body", "body second");
        assert_eq!(html_kids.len(), 2, "exactly one head + one body");
    }

    #[test]
    fn bare_flow_content_gets_skeleton_and_body() {
        let doc = parse("<p>hi</p>");
        assert_skeleton(&doc);
        // head is empty, the <p> is in body.
        assert!(children(&doc, head_of(&doc)).is_empty());
        let p = children(&doc, body_of(&doc))[0];
        assert_eq!(tag_of(&doc, p), "p");
        assert_eq!(text_of(&doc, children(&doc, p)[0]), Some("hi"));
    }

    #[test]
    fn metadata_to_head_flow_to_body() {
        let doc = parse("<!doctype html><title>T</title><div>x</div>");
        assert_skeleton(&doc);
        // <title> in head.
        let head_kids = children(&doc, head_of(&doc));
        assert_eq!(tag_of(&doc, head_kids[0]), "title");
        assert_eq!(text_of(&doc, children(&doc, head_kids[0])[0]), Some("T"));
        // <div> in body.
        let body_kids = children(&doc, body_of(&doc));
        assert_eq!(tag_of(&doc, body_kids[0]), "div");
        assert_eq!(text_of(&doc, children(&doc, body_kids[0])[0]), Some("x"));
    }

    #[test]
    fn explicit_skeleton_is_not_duplicated() {
        let doc = parse(
            "<html><head><meta charset=\"utf-8\"><title>T</title></head><body><p>hi</p></body></html>",
        );
        assert_skeleton(&doc);
        let head_kids = children(&doc, head_of(&doc));
        assert_eq!(tag_of(&doc, head_kids[0]), "meta");
        assert_eq!(tag_of(&doc, head_kids[1]), "title");
        let body_kids = children(&doc, body_of(&doc));
        assert_eq!(tag_of(&doc, body_kids[0]), "p");
    }

    #[test]
    fn head_script_runs_before_body_script_in_document_order() {
        // Both scripts must survive, head's first then body's. Document order of execution is the
        // tree's depth-first order: head/script precedes body/script.
        let doc = parse("<script>a()</script><div></div><script>b()</script>");
        let head_scripts = children(&doc, head_of(&doc));
        assert_eq!(tag_of(&doc, head_scripts[0]), "script");
        assert_eq!(text_of(&doc, children(&doc, head_scripts[0])[0]), Some("a()"));
        let body_kids = children(&doc, body_of(&doc));
        assert_eq!(tag_of(&doc, body_kids[0]), "div");
        assert_eq!(tag_of(&doc, body_kids[1]), "script");
        assert_eq!(text_of(&doc, children(&doc, body_kids[1])[0]), Some("b()"));
    }

    #[test]
    fn empty_input_yields_skeleton() {
        assert_skeleton(&parse(""));
    }

    #[test]
    fn text_only_input_yields_skeleton_with_body_text() {
        let doc = parse("just text");
        assert_skeleton(&doc);
        assert_eq!(text_of(&doc, children(&doc, body_of(&doc))[0]), Some("just text"));
    }

    #[test]
    fn comment_only_input_yields_skeleton() {
        let doc = parse("<!-- just a comment -->");
        assert_skeleton(&doc);
    }

    #[test]
    fn head_only_input_yields_empty_body() {
        let doc = parse("<head><title>T</title></head>");
        assert_skeleton(&doc);
        assert_eq!(tag_of(&doc, children(&doc, head_of(&doc))[0]), "title");
        assert!(children(&doc, body_of(&doc)).is_empty());
    }

    #[test]
    fn content_after_body_close_stays_in_body() {
        let doc = parse("<body><p>a</p></body><div>b</div>");
        assert_skeleton(&doc);
        let body_kids = children(&doc, body_of(&doc));
        assert_eq!(tag_of(&doc, body_kids[0]), "p");
        assert_eq!(tag_of(&doc, body_kids[1]), "div");
    }

    #[test]
    fn stray_end_tag_does_not_unwind_body() {
        // </body> with content after it must not strand later nodes outside body.
        let doc = parse("<div>a</div></body><span>b</span>");
        assert_skeleton(&doc);
        let body_kids = children(&doc, body_of(&doc));
        // Both <div> and the post-</body> <span> remain children of body.
        assert_eq!(tag_of(&doc, body_kids[0]), "div");
        assert_eq!(tag_of(&doc, body_kids[1]), "span");
    }
}
