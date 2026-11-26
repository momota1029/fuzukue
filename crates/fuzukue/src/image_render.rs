use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};

pub(super) fn from_rgba(mut pixels: RgbaImage) -> Arc<RenderImage> {
    // GPUI's render images use BGRA.
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(RenderImage::new([Frame::new(pixels)]))
}
