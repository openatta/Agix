//! Inline images in a session's entries and history: decoding them, replacing
//! them with a placeholder when attachments are left out, and pointing history
//! at the archive file when they are kept.

use super::{Attachments, Include};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// An inline image decoded: its content, what it is, and the archive file it
/// goes to.
pub(super) struct Decoded {
    pub(super) bytes: Vec<u8>,
    pub(super) media_type: String,
    pub(super) sha256: String,
    pub(super) file: String,
}

pub(super) fn decode_image(block: &Value) -> Option<Decoded> {
    let source = block.get("source")?;
    if source.get("type")?.as_str()? != "base64" {
        return None;
    }
    let media_type = source.get("media_type")?.as_str()?.to_string();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.get("data")?.as_str()?)
        .ok()?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let file = format!("attachments/{sha256}.{}", extension_of(&media_type));
    Some(Decoded {
        bytes,
        media_type,
        sha256,
        file,
    })
}

fn extension_of(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

pub(super) fn placeholder(image: &Decoded) -> Value {
    json!({
        "type": "text",
        "text": format!(
            "[image omitted from export: {}, {} bytes, sha256 {}]",
            image.media_type,
            image.bytes.len(),
            image.sha256
        ),
    })
}

/// Call `visit` on every image block under `value`: message content, tool
/// results nested in it, a compaction's replacement history.
pub(super) fn for_each_image(value: &mut Value, visit: &mut dyn FnMut(&mut Value)) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && map.get("source").is_some_and(Value::is_object)
            {
                visit(value);
                return;
            }
            for child in map.values_mut() {
                for_each_image(child, visit);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_each_image(item, visit);
            }
        }
        _ => {}
    }
}

pub(super) fn rewrite_history_images(
    history: &mut [Value],
    sid: &str,
    include: Include,
    attachments: &mut Attachments,
) {
    for item in history {
        let entry_id = item.get("id").and_then(Value::as_str).map(str::to_string);
        let seq = item.get("seq").and_then(Value::as_u64).map(|n| n as usize);
        for_each_image(item, &mut |block| {
            let Some(image) = decode_image(block) else {
                return;
            };
            if !include.attachments {
                *block = placeholder(&image);
                return;
            }
            if let Some(entry_id) = &entry_id {
                attachments.note(sid, entry_id, seq, &image);
            }
            block["source"] = json!({
                "attachment": image.file,
                "media_type": image.media_type,
            });
        });
    }
}
