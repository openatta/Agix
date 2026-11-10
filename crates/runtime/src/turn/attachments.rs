//! 用户输入附件：图片/文本的识别、内联与降级。

use base::interface::model::ModelContentBlock;

/// Media type for an image path, by extension. `None` for anything the
/// Anthropic API does not accept as an image, which is the signal to fall back
/// to reading the file as text.
pub(super) fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

pub(super) const MAX_ATTACHMENT_IMAGE_BYTES: u64 = base::tool::MAX_IMAGE_BYTES;

/// What a prompt middleware is shown of the attachments.
pub(super) async fn describe_attachments(
    attachments: &[crate::agent::Attachment],
) -> Vec<base::interface::prompt_middleware::PromptAttachment<'_>> {
    use crate::agent::Attachment;
    use base::interface::prompt_middleware::PromptAttachment;

    let mut described = Vec::with_capacity(attachments.len());
    for att in attachments {
        described.push(match att {
            Attachment::File { path } => {
                let size = tokio::fs::metadata(path).await.ok().map(|m| m.len());
                PromptAttachment::file(path, image_media_type(std::path::Path::new(path)), size)
            }
            Attachment::Text { path, content } => PromptAttachment::text(path, content),
            Attachment::Image { media_type, data } => {
                PromptAttachment::image(media_type, base64_decoded_len(data))
            }
        });
    }
    described
}

pub(super) fn base64_decoded_len(data: &str) -> u64 {
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count().min(2);
    ((data.len() / 4) * 3).saturating_sub(padding) as u64
}

/// Turn attachments into content blocks for the user message.
///
/// Failures degrade to an explanatory text block rather than propagating: an
/// unreadable attachment should not fail the user's whole turn, and telling
/// the model "this attachment could not be read" is more useful than silently
/// dropping it (the user will refer to it, and the model needs to be able to
/// say why it cannot see it).
pub(super) async fn resolve_attachments(
    attachments: &[crate::agent::Attachment],
    files: &base::paths::PromptFiles,
) -> Vec<ModelContentBlock> {
    use crate::agent::Attachment;
    use base64::Engine as _;

    let mut blocks = Vec::new();
    for att in attachments {
        match att {
            Attachment::Image { media_type, data } => blocks.push(ModelContentBlock::Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            Attachment::Text { path, content } => blocks.push(ModelContentBlock::Text {
                text: format!("<attachment path=\"{path}\">\n{content}\n</attachment>"),
            }),
            Attachment::File { path } => {
                let p = match files.resolve(std::path::Path::new(path)) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                        });
                        continue;
                    }
                };
                let p = p.as_path();
                match image_media_type(std::path::Path::new(path)) {
                    Some(media_type) => {
                        let size = tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
                        if size > MAX_ATTACHMENT_IMAGE_BYTES {
                            blocks.push(ModelContentBlock::Text {
                                text: format!(
                                    "<attachment path=\"{path}\" error=\"image too large: \
                                     {size} bytes, cap is {MAX_ATTACHMENT_IMAGE_BYTES}\" />"
                                ),
                            });
                            continue;
                        }
                        match tokio::fs::read(p).await {
                            Ok(bytes) => blocks.push(ModelContentBlock::Image {
                                media_type: media_type.to_string(),
                                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            }),
                            Err(e) => blocks.push(ModelContentBlock::Text {
                                text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                            }),
                        }
                    }
                    None => match tokio::fs::read_to_string(p).await {
                        Ok(text) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\">\n{text}\n</attachment>"),
                        }),
                        Err(e) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                        }),
                    },
                }
            }
        }
    }
    blocks
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. A module-local alias for `base::text`'s, which all
/// truncation sites share.
pub(super) fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    base::text::truncate_at_char_boundary(s, max_bytes)
}
