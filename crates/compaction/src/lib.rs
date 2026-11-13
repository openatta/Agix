//! Agix — context compaction strategies (snip, micro-compact, collapse, LLM summarize).

pub mod aging;
pub mod cached;
pub mod compact;
pub mod grouping;
pub mod llm_summary;
pub mod reactive;
pub mod time_based_mc;
