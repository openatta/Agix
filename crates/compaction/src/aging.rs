//! 老化（按时间的 MC、cached MC）：两道老化看什么、做了什么，以及它们就地清掉的工具
//! 结果原文——压缩没走完时，调用方凭它把对话原样放回去。

use crate::time_based_mc::{TimeBasedMcConfig, TimeBasedMcResult};
use base::interface::model::{ModelContentBlock, ModelMessage};
use std::time::Instant;

/// What the aging passes may look at.
///
/// Neither pass is about the token budget: one clears tool results that have
/// simply grown old, the other keeps the server-side cache from carrying
/// results nobody will read again. They run before anything consults the
/// budget, which is why they get their own input.
pub struct AgingInput<'a> {
    /// `(message index, when it was pushed)` for every message.
    pub message_ages: &'a [(usize, Instant)],
    pub time_based: &'a TimeBasedMcConfig,
    /// Whether the cached pass's interval has elapsed. The interval and the
    /// clock stay with the caller, which owns the pass's pending cache edits.
    pub cached_due: bool,
    pub cached_keep_recent: usize,
}

/// What one aging pass did.
#[derive(Debug, Default)]
pub struct Aging {
    pub time_based: TimeBasedMcResult,
    pub cached_cleared: usize,
    /// `Some` when the cached pass ran: the `tool_use_id`s whose results it
    /// blanked, which the next request sends as `cache_edits`.
    pub cache_edits: Option<Vec<String>>,
    /// What both passes overwrote: a caller whose compaction does not go
    /// through puts it back.
    pub undo: AgingUndo,
}

impl Aging {
    pub fn cleared(&self) -> usize {
        self.time_based.cleared + self.cached_cleared
    }
}

/// What an aging pass overwrote, block by block, so a caller whose compaction
/// then does not go through can put the conversation back as it was.
#[derive(Debug, Default)]
pub struct AgingUndo {
    overwritten: Vec<Overwritten>,
}

#[derive(Debug)]
struct Overwritten {
    message: usize,
    block: usize,
    tool_use_id: String,
    content: String,
    is_error: Option<bool>,
}

impl AgingUndo {
    pub fn is_empty(&self) -> bool {
        self.overwritten.is_empty()
    }

    /// Note the tool result at `(message, block)` as it is before a pass
    /// overwrites it.
    pub(crate) fn note(
        &mut self,
        message: usize,
        block: usize,
        tool_use_id: &str,
        content: &str,
        is_error: Option<bool>,
    ) {
        self.overwritten.push(Overwritten {
            message,
            block,
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error,
        });
    }

    /// Put every noted tool result back, the last overwrite first. A block
    /// that is no longer the tool result it was noted as is left alone.
    pub fn restore(self, messages: &mut [ModelMessage]) {
        for noted in self.overwritten.into_iter().rev() {
            if let Some(ModelContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }) = messages
                .get_mut(noted.message)
                .and_then(|m| m.content.get_mut(noted.block))
            {
                if *tool_use_id == noted.tool_use_id {
                    *content = noted.content;
                    *is_error = noted.is_error;
                }
            }
        }
    }
}
