//! 附件解析成内容块。

use crate::turn::attachments::{resolve_attachments, MAX_ATTACHMENT_IMAGE_BYTES};
use base::interface::model::ModelContentBlock;

use crate::agent::Attachment;

/// A 1x1 PNG. Real bytes so the extension-based media-type detection and
/// the base64 round-trip are both exercised.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89,
];

#[tokio::test]
async fn image_file_becomes_an_image_block_not_text() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("shot.png");
    std::fs::write(&p, TINY_PNG).unwrap();

    let blocks = resolve_attachments(
        &[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;

    assert_eq!(blocks.len(), 1);
    let ModelContentBlock::Image { media_type, data } = &blocks[0] else {
        panic!("expected an Image block, got {:?}", blocks[0]);
    };
    assert_eq!(media_type, "image/png");
    // Base64 of the real bytes, not a Debug rendering or a path string.
    use base64::Engine as _;
    assert_eq!(
        data,
        &base64::engine::general_purpose::STANDARD.encode(TINY_PNG)
    );
}

#[tokio::test]
async fn non_image_file_is_inlined_as_labelled_text() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("notes.md");
    std::fs::write(&p, "hello 世界").unwrap();

    let blocks = resolve_attachments(
        &[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;

    let ModelContentBlock::Text { text } = &blocks[0] else {
        panic!("expected a Text block");
    };
    assert!(text.contains("hello 世界"));
    assert!(text.contains("notes.md"), "path label missing: {text}");
}

/// Clipboard paste: no file exists, the host hands over decoded bytes.
#[tokio::test]
async fn pre_decoded_image_passes_through() {
    let blocks = resolve_attachments(
        &[Attachment::Image {
            media_type: "image/jpeg".into(),
            data: "QUJD".into(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;
    assert!(matches!(
        &blocks[0],
        ModelContentBlock::Image { media_type, data }
            if media_type == "image/jpeg" && data == "QUJD"
    ));
}

/// An unreadable attachment must not fail the turn — the user still wants
/// their message sent, and the model needs to be able to explain why it
/// cannot see what they referred to.
#[tokio::test]
async fn unreadable_attachment_degrades_to_an_error_note() {
    let blocks = resolve_attachments(
        &[Attachment::File {
            path: "/nonexistent/nope.png".into(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;
    let ModelContentBlock::Text { text } = &blocks[0] else {
        panic!("expected a Text block explaining the failure");
    };
    assert!(text.contains("error="), "got {text}");
    assert!(text.contains("nope.png"));
}

#[tokio::test]
async fn oversized_image_is_refused_rather_than_inlined() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("huge.png");
    std::fs::write(&p, vec![0u8; (MAX_ATTACHMENT_IMAGE_BYTES + 1) as usize]).unwrap();

    let blocks = resolve_attachments(
        &[Attachment::File {
            path: p.to_string_lossy().into_owned(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;

    let ModelContentBlock::Text { text } = &blocks[0] else {
        panic!("oversized image should degrade to text, not inline");
    };
    assert!(text.contains("too large"), "got {text}");
}

#[tokio::test]
async fn no_attachments_produces_no_blocks() {
    assert!(
        resolve_attachments(&[], &base::paths::PromptFiles::unscoped())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_credential_is_not_attached() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".agix")).unwrap();
    std::fs::write(dir.path().join(".agix/settings.local.json"), "SECRET").unwrap();
    std::os::unix::fs::symlink(".agix/settings.local.json", dir.path().join("notes.md")).unwrap();

    let blocks = resolve_attachments(
        &[Attachment::File {
            path: dir.path().join("notes.md").to_string_lossy().into_owned(),
        }],
        &base::paths::PromptFiles::unscoped(),
    )
    .await;

    let ModelContentBlock::Text { text } = &blocks[0] else {
        panic!("expected a Text block explaining the refusal");
    };
    assert!(!text.contains("SECRET"), "got {text}");
    assert!(text.contains("error="), "got {text}");
}
