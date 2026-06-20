//! A minimal, arena-allocated DOM. Nodes are referenced by [`NodeId`] (an index into the
//! arena) rather than by pointer, which keeps the tree `Clone`/`Send` and sidesteps the
//! ownership headaches of a pointer-linked tree in Rust.
//!
//! Phase 0: just the data model. The HTML tree builder (in the `html` crate) populates it.

use indexmap::IndexMap;

/// Element attributes, preserving insertion order (the DOM exposes attributes in source / set
/// order via `element.attributes`, `getAttributeNames()`, etc.).
pub type AttrMap = IndexMap<String, String>;

/// Index of a node within a [`Document`]'s arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone)]
pub enum NodeData {
    /// The root document node.
    Document,
    /// An element, e.g. `<div class="x">`.
    Element(ElementData),
    /// A run of text.
    Text(String),
    /// A comment `<!-- ... -->`.
    Comment(String),
    /// A `DocumentFragment` — a parentless container whose children move on insertion.
    DocumentFragment,
    /// A `<!DOCTYPE ...>` node (nodeType 10). Carries the name and the (usually empty) public/system
    /// identifiers. Not rendered; present so `document.doctype` and the ChildNode mixin work.
    DocumentType(DoctypeData),
    /// A processing instruction `<?target data?>` (nodeType 7). Created via
    /// `document.createProcessingInstruction`; not produced by the HTML parser.
    ProcessingInstruction(ProcessingInstructionData),
}

#[derive(Debug, Clone)]
pub struct DoctypeData {
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

#[derive(Debug, Clone)]
pub struct ProcessingInstructionData {
    pub target: String,
    pub data: String,
}

#[derive(Debug, Clone, Default)]
pub struct ElementData {
    pub tag: String,
    pub attrs: AttrMap,
    /// The element's namespace URI. `None` = the HTML namespace (the common case; kept `None` so
    /// existing code and constructors need no change). `Some(uri)` for foreign content (SVG / MathML
    /// elements, or elements created via `createElementNS`), used by namespaced selector matching
    /// (`@namespace` + `ns|tag` / `*|tag` / `|tag`).
    pub namespace: Option<String>,
}

impl ElementData {
    pub fn id(&self) -> Option<&str> {
        self.attrs.get("id").map(String::as_str)
    }
    /// Whitespace-separated class list.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attrs
            .get("class")
            .map(String::as_str)
            .unwrap_or("")
            .split_whitespace()
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub data: NodeData,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

/// An arena of nodes. Node 0 is always the [`NodeData::Document`] root.
#[derive(Debug, Clone, Default)]
pub struct Document {
    nodes: Vec<Node>,
}

impl Document {
    pub fn new() -> Self {
        let mut doc = Document { nodes: Vec::new() };
        doc.alloc(NodeData::Document, None);
        doc
    }

    /// The document root, always [`NodeId`]`(0)`.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Drop any child/parent references that point outside the arena. Page JS (via the engine's
    /// DOM bindings) can, in pathological cases, leave a stale or garbage node id in a `children`
    /// list; walking it later would panic with an out-of-bounds index. Call this once after
    /// scripts run, before layout/paint, so the renderer only ever sees valid ids. O(nodes).
    pub fn prune_invalid(&mut self) {
        let len = self.nodes.len();
        for node in &mut self.nodes {
            node.children.retain(|c| c.0 < len);
            if let Some(p) = node.parent {
                if p.0 >= len {
                    node.parent = None;
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Allocate a node (without linking it as anyone's child) and return its id.
    pub fn alloc(&mut self, data: NodeData, parent: Option<NodeId>) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { data, parent, children: Vec::new() });
        id
    }

    /// Allocate `data` as a child of `parent`, linking both directions.
    pub fn append_child(&mut self, parent: NodeId, data: NodeData) -> NodeId {
        let id = self.alloc(data, Some(parent));
        self.nodes[parent.0].children.push(id);
        id
    }

    /// Convenience: create an element child.
    pub fn append_element(&mut self, parent: NodeId, tag: &str) -> NodeId {
        self.append_child(
            parent,
            NodeData::Element(ElementData { tag: tag.to_string(), attrs: Default::default(), namespace: None }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_small_tree() {
        let mut doc = Document::new();
        let root = doc.root();
        let html = doc.append_element(root, "html");
        let body = doc.append_element(html, "body");
        doc.append_child(body, NodeData::Text("hi".into()));

        assert_eq!(doc.get(root).children, vec![html]);
        assert_eq!(doc.get(html).children, vec![body]);
        assert_eq!(doc.get(body).children.len(), 1);
        assert_eq!(doc.get(body).parent, Some(html));
    }
}
