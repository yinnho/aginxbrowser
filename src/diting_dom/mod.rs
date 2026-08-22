#![allow(dead_code)]
pub mod tree;
pub mod tree_sink;
pub mod selector;
pub mod serialize;

pub use tree::{DomTree, NodeData, NodeId};
pub use tree_sink::{parse_html, parse_fragment};
