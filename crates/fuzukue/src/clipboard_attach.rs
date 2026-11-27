//! Clipboard attachments and the shared queue for reading draft attachments.

use std::{io::Cursor, path::PathBuf, sync::atomic::Ordering};

use fuzukue_proto::{MAX_ATTACHMENTS, MAX_ATTACHMENT_BYTES};
use gpui_kit::{ClipboardEntry, ClipboardItem, Context, Image, ImageFormat};

use super::{decode_attachment_image, AttachmentState, DraftAttachment, Workbench, NEXT_ATTACHMENT_ID};

#[derive(Debug, PartialEq, Eq)]
enum PasteAction {
    PasteText,
    AttachImage,
    AttachFiles,
}

fn paste_action(has_text: bool, has_image: bool, file_paths: &[PathBuf]) -> PasteAction {
    // A file manager can also provide the paths as text for regular editors.
    if !file_paths.is_empty() {
        PasteAction::AttachFiles
    } else if has_image && !has_text {
        PasteAction::AttachImage
    } else {
        PasteAction::PasteText
    }
}

impl Workbench {
    pub(crate) fn paste_attachment(
        &mut self,
        clipboard: &ClipboardItem,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.chat_capabilities.is_none() {
            return false;
        }
        let mut has_text = false;
        let mut image = None;
        let mut paths = Vec::new();
        for entry in clipboard.entries() {
            match entry {
                ClipboardEntry::String(text) => has_text |= !text.text.is_empty(),
                ClipboardEntry::Image(entry) => {
                    image.get_or_insert(entry);
                }
                ClipboardEntry::ExternalPaths(entry) => paths.extend_from_slice(entry.paths()),
            }
        }
        let session = self.active.clone();
        match paste_action(has_text, image.is_some(), &paths) {
            PasteAction::PasteText => return false,
            PasteAction::AttachFiles => self.add_attachments(session, paths, cx),
            PasteAction::AttachImage => {
                if !self.has_attachment_room(&session, 1, cx) {
                    return true;
                }
                let image = image.expect("the paste decision found an image").clone();
                let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                let name = format!("clipboard-{timestamp}.png");
                self.queue_attachment(&session, name, move || png_bytes(image), cx);
                cx.notify();
            }
        }
        true
    }

    pub(super) fn has_attachment_room(
        &mut self,
        session: &str,
        count: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let chat = self.chat(session, cx);
        if chat.attachments.len() + count > MAX_ATTACHMENTS {
            chat.push_error(
                tr!(
                    "chat-up-to-attachments-per-message",
                    max_attachments = MAX_ATTACHMENTS.to_string()
                ),
                cx,
            );
            return false;
        }
        true
    }

    pub(super) fn queue_attachment(
        &mut self,
        session: &str,
        name: String,
        read: impl FnOnce() -> anyhow::Result<Vec<u8>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let session = session.to_owned();
        let id = NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed);
        self.chat(&session, cx).attachments.push(DraftAttachment {
            id,
            name: name.clone(),
            session: session.clone(),
            state: AttachmentState::Reading,
            preview: None,
        });
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move {
                read().map(|bytes| {
                    let preview = decode_attachment_image(&name, &bytes);
                    (bytes, preview)
                })
            }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(draft) = this
                    .chats
                    .get_mut(&session)
                    .and_then(|chat| chat.attachments.iter_mut().find(|draft| draft.id == id))
                else {
                    return;
                };
                if !matches!(draft.state, AttachmentState::Reading) {
                    return;
                }
                match result {
                    Ok((bytes, preview)) => {
                        draft.preview = preview;
                        draft.state = AttachmentState::Uploading { bytes, sent: 0 };
                        draft.send_chunk(0, &this.out);
                    }
                    Err(err) => draft.state = AttachmentState::Failed(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn png_bytes(image: Image) -> anyhow::Result<Vec<u8>> {
    let bytes = match image.format {
        ImageFormat::Png => {
            anyhow::ensure!(
                image::guess_format(&image.bytes).ok() == Some(image::ImageFormat::Png),
                tr!("chat-cannot-read-clipboard-image")
            );
            image.bytes
        }
        ImageFormat::Svg => anyhow::bail!(tr!("chat-unsupported-clipboard-image")),
        format => {
            // The clipboard backends can supply BMP (Windows DIB), TIFF and
            // other raster formats. GPUI enables their image crate codecs.
            let format = image::ImageFormat::from_extension(format.extension())
                .ok_or_else(|| anyhow::anyhow!(tr!("chat-unsupported-clipboard-image")))?;
            let bitmap = image::load_from_memory_with_format(&image.bytes, format)
                .map_err(|_| anyhow::anyhow!(tr!("chat-cannot-read-clipboard-image")))?;
            let mut png = Cursor::new(Vec::new());
            bitmap
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|_| anyhow::anyhow!(tr!("chat-cannot-read-clipboard-image")))?;
            png.into_inner()
        }
    };
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_ATTACHMENT_BYTES,
        tr!("chat-attachments-are-limited-to-50-mib-per-file")
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_paste_decision_table() {
        use PasteAction::*;

        let paths = [PathBuf::from("screenshot.png")];
        for (has_text, has_image, file_paths, expected) in [
            (false, false, &[][..], PasteText),
            (true, false, &[][..], PasteText),
            (false, true, &[][..], AttachImage),
            (true, true, &[][..], PasteText),
            (false, false, &paths[..], AttachFiles),
            (true, false, &paths[..], AttachFiles),
            (false, true, &paths[..], AttachFiles),
            (true, true, &paths[..], AttachFiles),
        ] {
            assert_eq!(paste_action(has_text, has_image, file_paths), expected);
        }
    }

    #[test]
    fn clipboard_bitmap_is_encoded_as_png() {
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([42, 123, 182]),
        ));
        let mut bmp = Cursor::new(Vec::new());
        source.write_to(&mut bmp, image::ImageFormat::Bmp).unwrap();
        let png = png_bytes(Image {
            format: ImageFormat::Bmp,
            bytes: bmp.into_inner(),
            id: 0,
        })
        .unwrap();
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!(decoded.to_rgb8(), source.to_rgb8());
    }
}
