//! A minimal, arena-allocated DOM. Nodes are referenced by [`NodeId`] (an index into the
//! arena) rather than by pointer, which keeps the tree `Clone`/`Send` and sidesteps the
//! ownership headaches of a pointer-linked tree in Rust.
//!
//! Phase 0: just the data model. The HTML tree builder (in the `html` crate) populates it.

use std::collections::HashMap;

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
}

#[derive(Debug, Clone)]
pub struct ElementData {
    pub tag: String,
    pub attrs: HashMap<String, String>,
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
            NodeData::Element(ElementData { tag: tag.to_string(), attrs: HashMap::new() }),
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
