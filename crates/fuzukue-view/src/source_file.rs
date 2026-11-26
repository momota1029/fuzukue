//! Content and resource limits for files opened in the workspace pane.

use std::{
    io::{Cursor, Read},
    path::Path,
};

use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits};

pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EDGE: u32 = 8192;
const MAX_PIXELS: u64 = 24 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewError {
    Unavailable,
    Unsupported,
    TooLarge,
}

impl PreviewError {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Unavailable => "editor-preview-unavailable",
            Self::Unsupported => "editor-preview-unsupported",
            Self::TooLarge => "editor-preview-too-large",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image(ImageFormat),
}

impl Kind {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Text => "text/plain; charset=utf-8",
            Self::Image(format) => format.to_mime_type(),
        }
    }
}

fn supported_image(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp
    )
}

/// Use the extension only for routing from the file browser. Validate the actual bytes on open.
pub fn has_image_extension(path: &str) -> bool {
    ImageFormat::from_path(Path::new(path)).is_ok_and(supported_image)
}

pub fn read(input: impl Read) -> Result<Vec<u8>, PreviewError> {
    let mut bytes = Vec::new();
    input
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PreviewError::Unavailable)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(PreviewError::TooLarge);
    }
    Ok(bytes)
}

pub fn classify(bytes: &[u8]) -> Result<Kind, PreviewError> {
    // Some binary signatures, such as BMP's "BM", can also start ordinary text.
    if !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok() {
        return Ok(Kind::Text);
    }
    image::guess_format(bytes)
        .ok()
        .filter(|format| supported_image(*format))
        .map(Kind::Image)
        .ok_or(PreviewError::Unsupported)
}

pub fn inspect_image(bytes: &[u8], format: ImageFormat) -> Result<(), PreviewError> {
    image_decoder(bytes, format).map(drop)
}

pub fn decode_image(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, PreviewError> {
    DynamicImage::from_decoder(image_decoder(bytes, format)?).map_err(decode_error)
}

fn image_decoder(
    bytes: &[u8],
    format: ImageFormat,
) -> Result<impl ImageDecoder + '_, PreviewError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(PreviewError::Unsupported);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(PreviewError::TooLarge);
    }
    // Include the decoded buffer in the decoder's allocation budget.
    limits
        .reserve(decoder.total_bytes())
        .map_err(decode_error)?;
    decoder.set_limits(limits).map_err(decode_error)?;
    Ok(decoder)
}

fn decode_error(error: ImageError) -> PreviewError {
    match error {
        ImageError::Limits(_) => PreviewError::TooLarge,
        _ => PreviewError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_text_and_rejects_unknown_binary() {
        assert_eq!(classify(b"BM is the start of this text"), Ok(Kind::Text));
        assert_eq!(classify(b""), Ok(Kind::Text));
        assert_eq!(classify(b"text\x00"), Err(PreviewError::Unsupported));
        assert_eq!(classify(b"\xFF\xFE"), Err(PreviewError::Unsupported));
    }

    #[test]
    fn bounds_reads_at_the_preview_limit() {
        assert_eq!(
            read(std::io::repeat(b'a').take(MAX_BYTES)).unwrap().len() as u64,
            MAX_BYTES
        );
        assert_eq!(read(std::io::repeat(b'a')), Err(PreviewError::TooLarge));
    }

    #[test]
    fn checks_image_content_and_dimensions() {
        let png = |width| {
            let mut bytes = Cursor::new(Vec::new());
            DynamicImage::new_rgba16(width, 1)
                .write_to(&mut bytes, ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let bytes = png(2464);
        assert_eq!(classify(&bytes), Ok(Kind::Image(ImageFormat::Png)));
        assert_eq!(
            decode_image(&bytes, ImageFormat::Png).unwrap().width(),
            2464
        );
        assert!(matches!(
            decode_image(&bytes[..16], ImageFormat::Png),
            Err(PreviewError::Unsupported)
        ));
        assert!(matches!(
            decode_image(&png(MAX_EDGE + 1), ImageFormat::Png),
            Err(PreviewError::TooLarge)
        ));
    }
}
