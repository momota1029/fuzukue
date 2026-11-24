//! Fetch and normalize public GitHub avatars independently of OAuth credentials.

use std::io::{Cursor, Read};
use std::time::Duration;

use base64::Engine as _;
use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use url::Url;

const HOST: &str = "avatars.githubusercontent.com";
const SIZE: u32 = 128;
const MAX_BYTES: u64 = 256 * 1024;

pub(super) fn safe_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let id = url.path().strip_prefix("/u/")?;
    if url.scheme() != "https"
        || url.host_str() != Some(HOST)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // Keep only GitHub's numeric user route. The source query is untrusted.
    Some(format!("https://{HOST}/u/{id}?s={SIZE}"))
}

pub(super) fn fetch(raw: &str) -> Option<(String, String)> {
    let url = safe_url(raw)?;
    let response = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("fuzukue/", env!("CARGO_PKG_VERSION")))
        .build()
        .get(&url)
        .set("Accept", "image/png, image/jpeg")
        .call()
        .ok()?;
    if response.status() != 200 {
        return None;
    }
    let mime = response
        .header("Content-Type")?
        .split(';')
        .next()?
        .trim()
        .to_owned();
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let png = normalize(&mime, &bytes)?;
    let etag = format!("{:x}", Sha256::digest(&png));
    Some((etag, base64::engine::general_purpose::STANDARD.encode(png)))
}

pub(super) fn normalize(mime: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return None,
    };
    if bytes.len() as u64 > MAX_BYTES || image::guess_format(bytes).ok()? != format {
        return None;
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?.thumbnail(SIZE, SIZE).into_rgba8();
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png).ok()?;
    let output = output.into_inner();
    (output.len() as u64 <= MAX_BYTES).then_some(output)
}
