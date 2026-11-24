//! Image attachments in the composer and the transcript.

use std::{io::Cursor, path::Path, sync::Arc};

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    WindowExt as _,
};
use gpui_kit::{
    img, prelude::*, px, App, Asset, IntoElement, ObjectFit, RenderImage, SharedString, Window,
};
use image::{Frame, ImageFormat, ImageReader};

use super::{attachment_chip, status_speech};
use crate::conn::HttpTarget;

fn attachment_image_format(name: &str) -> Option<ImageFormat> {
    match Path::new(name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        "bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

fn thumbnail_size(width: u32, height: u32) -> (f32, f32) {
    let (width, height) = (f64::from(width), f64::from(height));
    let scale = (240. / width).min(160. / height).min(1.);
    ((width * scale) as f32, (height * scale) as f32)
}

pub(super) struct AttachmentImage {
    render: Arc<RenderImage>,
    width: u32,
    height: u32,
}

pub(super) fn decode_attachment_image(name: &str, bytes: &[u8]) -> Option<Arc<AttachmentImage>> {
    let format = attachment_image_format(name)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut decoded = reader.decode().ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return None;
    }
    if width > 1600 || height > 1200 {
        decoded = decoded.resize(1600, 1200, image::imageops::FilterType::Triangle);
    }
    let mut pixels = decoded.into_rgba8();
    // GPUI's render images use BGRA.
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    let render = Arc::new(RenderImage::new([Frame::new(pixels)]));
    Some(Arc::new(AttachmentImage {
        render,
        width,
        height,
    }))
}

struct SavedImage;

impl Asset for SavedImage {
    type Source = (u64, HttpTarget, String);
    type Output = Option<Arc<AttachmentImage>>;

    fn load(
        (_, http, path): Self::Source,
        _: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        async move {
            let bytes = http.fetch_attachment(&path)?;
            decode_attachment_image(&path, &bytes)
        }
    }
}

#[derive(IntoElement)]
pub(super) struct AttachmentThumbnail {
    pub id: SharedString,
    pub name: String,
    pub label: String,
    pub decoded: Option<Arc<AttachmentImage>>,
    pub saved: Option<(u64, HttpTarget, String)>,
}

impl RenderOnce for AttachmentThumbnail {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let decoded = self.decoded.or_else(|| {
            let source = self.saved.as_ref()?;
            attachment_image_format(&source.2)?;
            window.use_asset::<SavedImage>(source, cx).flatten()
        });
        let Some(image) = decoded else {
            return attachment_chip(&self.label, cx).into_any_element();
        };
        let (width, height) = thumbnail_size(image.width, image.height);
        let name = self.name;
        Button::new(self.id)
            .ghost()
            .p_0()
            .w(px(width))
            .h(px(height))
            .min_w_0()
            .max_w_full()
            .rounded_md()
            .overflow_hidden()
            .tooltip(status_speech(&self.label))
            .accessibility_label(tr!("chat-attachment-enlarge", name = name.as_str()))
            .child(
                img(image.render.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .rounded_md(),
            )
            .on_click(move |_, window, cx| {
                let (name, render) = (name.clone(), image.render.clone());
                window.open_dialog(cx, move |dialog, window, _| {
                    let viewport = window.viewport_size();
                    dialog
                        .title(name.clone())
                        .overlay_closable(true)
                        .w(viewport.width * 0.9)
                        .max_w(px(1440.))
                        .child(
                            img(render.clone())
                                .id("chat-attachment-full-image")
                                .w_full()
                                .h(viewport.height * 0.76)
                                .object_fit(ObjectFit::Contain)
                                .rounded_md(),
                        )
                });
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_supported_attachment_extensions() {
        for (name, format) in [
            ("inbox/test.PNG", ImageFormat::Png),
            ("test.jpg", ImageFormat::Jpeg),
            ("test.JPEG", ImageFormat::Jpeg),
            ("test.gif", ImageFormat::Gif),
            ("test.webp", ImageFormat::WebP),
            ("test.bmp", ImageFormat::Bmp),
        ] {
            assert_eq!(attachment_image_format(name), Some(format));
        }
        for name in [
            "test",
            "test.svg",
            "file.png.txt",
            "dir.png/test",
            "test.png/other",
        ] {
            assert_eq!(attachment_image_format(name), None);
        }
    }

    #[test]
    fn thumbnails_preserve_aspect_ratio_and_do_not_upscale() {
        assert_eq!(thumbnail_size(1200, 600), (240., 120.));
        assert_eq!(thumbnail_size(300, 600), (80., 160.));
        assert_eq!(thumbnail_size(64, 32), (64., 32.));
    }

    #[test]
    fn decodes_the_supported_formats_for_gpui() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            12,
            16,
            image::Rgba([40, 100, 200, 255]),
        ));
        for extension in ["png", "jpg", "gif", "webp", "bmp"] {
            let name = format!("attachment.{extension}");
            let mut bytes = Cursor::new(Vec::new());
            image
                .write_to(&mut bytes, attachment_image_format(&name).unwrap())
                .unwrap();
            let decoded = decode_attachment_image(&name, bytes.get_ref()).unwrap();
            assert_eq!((decoded.width, decoded.height), (12, 16));
            let pixel = &decoded.render.as_bytes(0).unwrap()[..4];
            if extension == "jpg" {
                assert!(pixel[0] > pixel[1] && pixel[1] > pixel[2]);
            } else {
                assert_eq!(pixel, [200, 100, 40, 255]);
            }
        }
        assert!(decode_attachment_image("broken.png", b"invalid png").is_none());
    }
}
