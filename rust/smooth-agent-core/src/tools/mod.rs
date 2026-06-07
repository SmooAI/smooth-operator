//! Tools the smooth-agent runtime registers on the smooth-operator engine.
//!
//! Each tool implements smooth-operator's [`Tool`](smooth_operator::Tool) trait
//! so the [`Agent`](smooth_operator::Agent) can invoke it during a turn.

pub mod knowledge_search;

pub use knowledge_search::KnowledgeSearchTool;
