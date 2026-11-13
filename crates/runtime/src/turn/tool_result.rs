//! 工具结果进入对话：内容渲染成文本、带外图片的收集与追加。

use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use base::tool::ToolResultContent;

/// A base64 image a tool returned, waiting to be attached to this round's
/// tool-result message. See [`ToolExecCtx::images`].
#[derive(Debug, Clone)]
pub(crate) struct PendingToolImage {
    pub media_type: String,
    pub data: String,
}

/// Render a `ToolResultContent` into the text the model sees, lifting out any
/// images so the caller can emit them as real `ModelContentBlock::Image`s.
///
/// **The bug this replaces:** the `Blocks` arm was `format!("{:?}", b)` — the
/// model literally received
/// `[ToolResultBlock { block_type: "image", text: None, source: Some(Object {..}) }]`.
/// Text blocks lost their content entirely (it was buried inside `text: Some(..)`
/// in Debug form), and images arrived as a Rust struct dump. Every MCP server
/// returning anything other than one plain string was affected.
///
/// `Blocks` is not an exotic shape — `mcp::adapter::into_tool_result` produces it
/// for any structured MCP result — so this is the normal path for a large class
/// of tools, not an edge case.
pub(super) fn render_tool_result_content(
    content: base::tool::ToolResultContent,
) -> (String, Vec<PendingToolImage>) {
    match content {
        ToolResultContent::Text(t) => (t, Vec::new()),
        ToolResultContent::Blocks(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            let mut images: Vec<PendingToolImage> = Vec::new();
            for block in &blocks {
                if let Some((media_type, data)) = block.as_image() {
                    images.push(PendingToolImage {
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    });
                    continue;
                }
                // Any non-image block contributes whatever text it carries.
                // An unknown `block_type` with text is still worth showing;
                // one with neither text nor a usable image source has nothing
                // to contribute and is dropped rather than Debug-printed.
                if let Some(text) = block.text.as_ref() {
                    if !text.is_empty() {
                        parts.push(text.clone());
                    }
                }
            }
            let mut text = parts.join("\n");
            if text.is_empty() && !images.is_empty() {
                // A tool_result whose content is the empty string is not useful
                // to the model (and some providers reject it) — name what the
                // accompanying Image blocks are instead.
                text = if images.len() == 1 {
                    "[image]".to_string()
                } else {
                    format!("[{} images]", images.len())
                };
            }
            (text, images)
        }
    }
}

/// Fence a result that came from outside the user's reach — a server's, a
/// host's, a file's outside the project — so the model reads it as data. A
/// server's error is its own words and is fenced too; any other failure is
/// the engine's. A program reading a file outside the project gets its lines
/// as they are.
pub(super) fn fence_external(
    name: &str,
    facts: &base::interface::permission::ToolFacts,
    from_code: bool,
    outcome: &mut base::interface::tool_middleware::ToolOutcome,
) {
    use base::interface::permission::Reach;
    let source = || facts.subject.content().unwrap_or(name).to_string();
    let (source, from) = match &facts.reach {
        Reach::Server => (
            name.to_string(),
            match facts.origin.mcp_server() {
                Some(server) => format!("the MCP server `{server}`"),
                None => "a server".to_string(),
            },
        ),
        Reach::Host { .. } => (source(), "the web".to_string()),
        Reach::Outside if !from_code => (source(), "a file outside the project".to_string()),
        _ => return,
    };
    let text = match outcome {
        Ok(answer) => &mut answer.text,
        Err(message) if facts.reach == Reach::Server => message,
        Err(_) => return,
    };
    *text = tools::external_content::fence(&source, &from, text);
}

/// Attach `images` to the tool-result message `execute_stream` just pushed.
///
/// **Shape, and why it is API-valid:** the images become
/// `ModelContentBlock::Image` blocks appended *after* every `ToolResult` block in
/// the same user message. `model::adapter::to_content_block` already maps that
/// variant to `base::message::ContentBlock::Image { source: ImageSource::Base64 }`,
/// so no new wire mapping is involved, and a user message's content array is a
/// heterogeneous list that accepts `image` blocks alongside `tool_result` ones.
/// Appending (never inserting) is the load-bearing detail: the API requires a
/// user turn's `tool_result` blocks to come first, so images must trail them.
///
/// This is deliberately *not* done by rewriting `ModelContentBlock::ToolResult`
/// to hold structured content. That variant has ~32 construction sites and ~116
/// match sites across 17 files; widening it to carry images would be a
/// workspace-wide refactor to express something the sibling-block form already
/// expresses correctly.
pub(super) fn attach_tool_result_images(
    messages: &mut [ModelMessage],
    images: Vec<PendingToolImage>,
) {
    let target = messages.iter_mut().rev().find(|m| {
        m.role == MessageRole::User
            && m.content
                .iter()
                .any(|b| matches!(b, ModelContentBlock::ToolResult { .. }))
    });
    let Some(msg) = target else {
        // No tool-result message to hang them on (the round produced no tool
        // results at all). Dropping is correct: an orphan image block with no
        // surrounding context is noise, and we must not fabricate a user turn.
        tracing::debug!(
            count = images.len(),
            "tool returned images but no tool_result message was pushed; dropping"
        );
        return;
    };
    for image in images {
        msg.content.push(ModelContentBlock::Image {
            media_type: image.media_type,
            data: image.data,
        });
    }
}
