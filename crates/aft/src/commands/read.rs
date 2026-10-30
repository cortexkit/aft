//! Handler for the `read` command: fast file/directory reading with line numbers.
//!
//! This is the simple "give me file contents" command, designed to replace
//! opencode's built-in read tool with a faster Rust implementation.
//! For symbol-based reading and call-graph annotations, use `zoom` instead.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::codecs::{gif::GifEncoder, jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ExtendedColorType, GenericImageView, ImageDecoder, ImageEncoder};
use serde_json::Value;

use crate::context::AppContext;
use crate::github_read::{
    parse_resource, sqlite_cache_store, GhCliFetcher, GithubReadCompletion, GithubReadEngine,
    GithubReadSelector, GithubReadStart, GithubReadView, ReqwestGithubImageDownloader,
    SystemGithubReadClock,
};
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024; // 50KB output cap
const MAX_FILE_READ_BYTES: u64 = 50 * 1024 * 1024; // 50MB input guard
const MAX_DIRECTORY_ENTRIES: usize = 1000;
const MAX_DIRECTORY_SCAN: usize = 10_000;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;
const MEDIA_MAGIC_BYTES: usize = 16;
const MAX_INLINE_BASE64_BYTES: usize = 9 * 1024 * 1024 / 2; // 4.5 MiB encoded payload cap
const MAX_INLINE_IMAGE_DIMENSION: u32 = 1024;
const MIN_FITTED_IMAGE_DIMENSION: u32 = 256;
const MAX_IMAGE_FIT_STEPS: usize = 6;
const IMAGE_REPLY_MARGIN_BYTES: usize = 64 * 1024;
const MAX_DECODE_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
/// Debug builds get a wider budget: unoptimized image codecs on large test
/// fixtures routinely exceed 10s under parallel test load, and the timeout
/// exists to bound production hangs, not to assert codec speed.
const IMAGE_PROCESS_TIMEOUT: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(40)
} else {
    Duration::from_secs(10)
};

static GITHUB_READ_ENGINES: LazyLock<Mutex<BTreeMap<PathBuf, Arc<GithubReadEngine>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageKind {
    fn mime(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::WebP => "image/webp",
        }
    }

    fn format(self) -> image::ImageFormat {
        match self {
            ImageKind::Png => image::ImageFormat::Png,
            ImageKind::Jpeg => image::ImageFormat::Jpeg,
            ImageKind::Gif => image::ImageFormat::Gif,
            ImageKind::WebP => image::ImageFormat::WebP,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SniffedMedia {
    Image(ImageKind),
    Pdf,
}

struct ProcessedImage {
    bytes: Vec<u8>,
    mime: &'static str,
    width: u32,
    height: u32,
    resized: bool,
    source_mime: Option<&'static str>,
    animation: &'static str,
    orientation_applied: bool,
}

/// Check if file content is binary using the content_inspector crate.
/// Detects null bytes, UTF-16 BOMs, and other binary indicators.
fn is_binary(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}

fn read_magic(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = crate::task_temp_reads::open(path)?;
    let mut magic = [0u8; MEDIA_MAGIC_BYTES];
    let len = file.read(&mut magic)?;
    Ok(magic[..len].to_vec())
}

fn sniff_media(bytes: &[u8]) -> Option<SniffedMedia> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(SniffedMedia::Image(ImageKind::Png));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(SniffedMedia::Image(ImageKind::Jpeg));
    }
    if bytes.starts_with(b"GIF8") {
        return Some(SniffedMedia::Image(ImageKind::Gif));
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(SniffedMedia::Image(ImageKind::WebP));
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(SniffedMedia::Pdf);
    }
    None
}

fn base64_len(raw_len: usize) -> Option<usize> {
    raw_len.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

fn encode_base64_checked(
    bytes: &[u8],
    too_large_message: impl FnOnce(usize) -> String,
) -> Result<String, String> {
    let encoded_len =
        base64_len(bytes.len()).ok_or_else(|| "attachment is too large to measure".to_string())?;
    if encoded_len > MAX_INLINE_BASE64_BYTES {
        return Err(too_large_message(encoded_len));
    }
    Ok(BASE64.encode(bytes))
}

fn attachment_size_note(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} bytes")
    }
}

fn media_omitted_response(id: &str, byte_size: usize, reason: String) -> Response {
    Response::success(
        id,
        serde_json::json!({
            "attachments": [],
            "attachment_omitted_reason": reason,
            "content": format!("Attachment omitted: {}", reason),
            "complete": false,
            "byte_size": byte_size,
        }),
    )
}

fn media_file_too_large_response(id: &str, byte_size: u64) -> Response {
    media_omitted_response(
        id,
        byte_size as usize,
        format!(
            "media file is too large to process ({} bytes > {} bytes)",
            byte_size, MAX_FILE_READ_BYTES
        ),
    )
}

enum PreparedPathRead {
    Response(Response),
    Media {
        raw_bytes: Vec<u8>,
        media: SniffedMedia,
    },
}

impl PreparedPathRead {
    fn into_response(self, id: &str) -> Response {
        let (raw_bytes, media) = match self {
            Self::Response(response) => return response,
            Self::Media { raw_bytes, media } => (raw_bytes, media),
        };
        let byte_size = raw_bytes.len();
        match media {
            SniffedMedia::Pdf => handle_pdf_media(id, raw_bytes),
            SniffedMedia::Image(kind) => match process_image_with_timeout(raw_bytes, kind) {
                Ok(image) => image_attachment_response(id, image),
                Err(reason) => media_omitted_response(id, byte_size, reason),
            },
        }
    }
}

fn prepare_media_read(
    req: &RawRequest,
    path: &Path,
    byte_size: u64,
    media: SniffedMedia,
    capture_source: bool,
    captured_source: &mut Option<CapturedRead>,
) -> PreparedPathRead {
    if byte_size > MAX_FILE_READ_BYTES {
        return PreparedPathRead::Response(media_file_too_large_response(&req.id, byte_size));
    }

    // Hashline tag publication needs source bytes and metadata, even to explain
    // why binary content cannot receive line tags. Capture them within the
    // filesystem deadline instead of reopening the file after a slow decode.
    let source = capture_source
        .then(|| crate::hashline::snapshot::read_source(path).ok())
        .flatten();
    let bytes = match source.as_ref().filter(|source| source.has_complete_bytes()) {
        Some(source) => Ok(source.bytes().to_vec()),
        None => crate::bounded_io::read(path, None),
    };
    let raw_bytes = match bytes {
        Ok(bytes) => bytes,
        Err(e) => {
            return PreparedPathRead::Response(Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            ));
        }
    };
    if let Some(source) = source {
        *captured_source = Some(CapturedRead {
            path: path.to_path_buf(),
            source,
        });
    }
    PreparedPathRead::Media { raw_bytes, media }
}

fn handle_pdf_media(id: &str, raw_bytes: Vec<u8>) -> Response {
    let byte_size = raw_bytes.len();
    let data = match encode_base64_checked(&raw_bytes, |encoded_len| {
        format!(
            "PDF too large to inline (base64 payload {} bytes > {} bytes)",
            encoded_len, MAX_INLINE_BASE64_BYTES
        )
    }) {
        Ok(data) => data,
        Err(reason) => return media_omitted_response(id, byte_size, reason),
    };
    let base64_bytes = data.len();

    Response::success(
        id,
        serde_json::json!({
            "attachments": [{
                "kind": "pdf",
                "mime": "application/pdf",
                "data": data,
                "bytes": byte_size,
                "base64_bytes": base64_bytes,
            }],
            "content": format!("Read PDF attachment ({}).", attachment_size_note(byte_size)),
            "complete": true,
            "byte_size": byte_size,
        }),
    )
}

fn image_attachment_response(id: &str, image: ProcessedImage) -> Response {
    image_attachment_response_with_budget(
        id,
        image,
        crate::subc::result_body::active().then_some(image_base64_budget()),
    )
}

fn image_base64_budget() -> usize {
    (crate::subc::result_body::MAX_RESULT_BYTES - IMAGE_REPLY_MARGIN_BYTES - 1) / 2
}

fn image_attachment_response_with_budget(
    id: &str,
    image: ProcessedImage,
    budget: Option<usize>,
) -> Response {
    let byte_size = image.bytes.len();
    if let Some(budget) = budget {
        // Both content and structuredContent carry the fitted image. The encoder
        // has already tried the floor before this final pre-base64 refusal.
        let wire_size = base64_len(byte_size)
            .unwrap_or(usize::MAX)
            .saturating_mul(2)
            .saturating_add(IMAGE_REPLY_MARGIN_BYTES);
        if base64_len(byte_size).is_none_or(|bytes| bytes > budget) {
            return crate::subc::result_body::too_large(id, wire_size, "read a smaller image");
        }
    }
    let data = match encode_base64_checked(&image.bytes, |encoded_len| {
        format!(
            "image too large to inline after resize (base64 payload {} bytes > {} bytes)",
            encoded_len, MAX_INLINE_BASE64_BYTES
        )
    }) {
        Ok(data) => data,
        Err(reason) => return media_omitted_response(id, byte_size, reason),
    };
    let base64_bytes = data.len();

    let mut attachment = serde_json::json!({
        "kind": "image",
        "mime": image.mime,
        "data": data,
        "bytes": byte_size,
        "base64_bytes": base64_bytes,
        "width": image.width,
        "height": image.height,
        "resized": image.resized,
        "animation": image.animation,
        "orientation_applied": image.orientation_applied,
    });
    if budget.is_some() {
        attachment["encoding"] = serde_json::json!(image.mime.trim_start_matches("image/"));
    }
    if let Some(source_mime) = image.source_mime {
        attachment
            .as_object_mut()
            .expect("attachment is an object")
            .insert("source_mime".to_string(), serde_json::json!(source_mime));
    }

    let mut content = format!(
        "Read image attachment ({}, {}×{}, {}).",
        image.mime,
        image.width,
        image.height,
        attachment_size_note(byte_size)
    );
    if image.animation == "first_frame" {
        content.push_str(" Animated image was resized from its first frame.");
    }

    Response::success(
        id,
        serde_json::json!({
            "attachments": [attachment],
            "content": content,
            "complete": true,
            "byte_size": byte_size,
        }),
    )
}

static ACTIVE_IMAGE_DECODERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
const MAX_IMAGE_DECODERS: usize = 2;

struct ImageDecoderPermit;

impl Drop for ImageDecoderPermit {
    fn drop(&mut self) {
        ACTIVE_IMAGE_DECODERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[allow(deprecated)] // fetch_update became try_update in Rust 1.99; the MSRV (1.92) lacks try_update
fn process_image_with_timeout(
    raw_bytes: Vec<u8>,
    kind: ImageKind,
) -> Result<ProcessedImage, String> {
    process_image_with_budget_timeout(
        raw_bytes,
        kind,
        crate::subc::result_body::active().then_some(image_base64_budget()),
        Instant::now() + IMAGE_PROCESS_TIMEOUT,
    )
}

#[allow(deprecated)] // Keep decoder admission compatible with the minimum supported Rust version.
fn process_image_with_budget_timeout(
    raw_bytes: Vec<u8>,
    kind: ImageKind,
    budget: Option<usize>,
    deadline: Instant,
) -> Result<ProcessedImage, String> {
    #[cfg(test)]
    IMAGE_PROCESSING_DELAY.with(|delay| std::thread::sleep(delay.get()));
    ACTIVE_IMAGE_DECODERS.fetch_update(
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
        |active| (active < MAX_IMAGE_DECODERS).then_some(active + 1),
    ).map_err(|active| format!("image decoder capacity reached ({active}/{MAX_IMAGE_DECODERS} active); retry after existing image reads finish"))?;
    let permit = ImageDecoderPermit;
    let (tx, rx) = crossbeam_channel::bounded(1);
    // The worker owns admission until decoding actually ends, including after a
    // caller timeout. Reject overload rather than queueing retained image bytes.
    let spawned = std::thread::Builder::new()
        .name("aft-read-media".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _permit = permit;
            let result = match budget {
                Some(budget) => process_image_fitting(raw_bytes, kind, budget, deadline),
                None => process_image(raw_bytes, kind),
            };
            let _ = tx.send(result);
        });
    if let Err(error) = spawned {
        return Err(format!("failed to start image processing: {error}"));
    }

    let timeout = if budget.is_some() {
        deadline.saturating_duration_since(Instant::now())
    } else {
        IMAGE_PROCESS_TIMEOUT
    };
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(format!(
            "image processing timed out after {} seconds",
            IMAGE_PROCESS_TIMEOUT.as_secs()
        )),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            Err("image processing worker stopped before returning a result".to_string())
        }
    }
}

fn image_is_opaque(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageLumaA8(image) => image.pixels().all(|pixel| pixel.0[1] == u8::MAX),
        DynamicImage::ImageRgba8(image) => image.pixels().all(|pixel| pixel.0[3] == u8::MAX),
        DynamicImage::ImageLumaA16(image) => image.pixels().all(|pixel| pixel.0[1] == u16::MAX),
        DynamicImage::ImageRgba16(image) => image.pixels().all(|pixel| pixel.0[3] == u16::MAX),
        DynamicImage::ImageRgba32F(image) => image.pixels().all(|pixel| pixel.0[3] == 1.0),
        _ => !image.color().has_alpha(),
    }
}

fn encode_fitted_image(
    image: &DynamicImage,
    kind: ImageKind,
    allow_lossy: bool,
) -> Result<(Vec<u8>, &'static str), String> {
    if kind == ImageKind::Jpeg && allow_lossy {
        return encode_jpeg(image).map(|bytes| (bytes, ImageKind::Jpeg.mime()));
    }
    // PNG retains alpha (including 16-bit alpha); GIF palette conversion and
    // JPEG's RGB conversion cannot be used for transparent fitted images.
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(format_image_processing_error)?;
    Ok((output.into_inner(), ImageKind::Png.mime()))
}

fn process_image_fitting(
    raw_bytes: Vec<u8>,
    kind: ImageKind,
    budget: usize,
    deadline: Instant,
) -> Result<ProcessedImage, String> {
    if Instant::now() >= deadline {
        return Err("image processing timed out while fitting reply".into());
    }
    let mut reader =
        image::ImageReader::with_format(Cursor::new(raw_bytes.as_slice()), kind.format());
    reader.limits(image_decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(format_image_processing_error)?;
    let orientation = decoder
        .orientation()
        .map_err(format_image_processing_error)?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(format_image_processing_error)?;
    if Instant::now() >= deadline {
        return Err("image processing timed out while fitting reply".into());
    }
    let (width, height) = decoded.dimensions();
    let animated = is_animated_image(&raw_bytes, kind);
    if width.max(height) <= MAX_INLINE_IMAGE_DIMENSION
        && base64_len(raw_bytes.len()).is_some_and(|bytes| bytes <= budget)
    {
        return Ok(ProcessedImage {
            bytes: raw_bytes,
            mime: kind.mime(),
            width,
            height,
            resized: false,
            source_mime: None,
            animation: if animated { "preserved" } else { "none" },
            orientation_applied: false,
        });
    }
    let allow_lossy = image_is_opaque(&decoded) && !animated;
    let orientation_applied = orientation != Orientation::NoTransforms;
    if orientation_applied {
        decoded.apply_orientation(orientation);
    }
    let (width, height) = decoded.dimensions();
    let (target_width, target_height) = resized_dimensions(width, height);
    let mut current = if width.max(height) > MAX_INLINE_IMAGE_DIMENSION {
        decoded.resize_exact(target_width, target_height, FilterType::Lanczos3)
    } else {
        decoded
    };
    let mut result = if width.max(height) > MAX_INLINE_IMAGE_DIMENSION {
        let (bytes, mime) = encode_fitted_image(&current, kind, allow_lossy)?;
        ProcessedImage {
            bytes,
            mime,
            width: target_width,
            height: target_height,
            resized: true,
            source_mime: Some(kind.mime()),
            animation: if animated { "first_frame" } else { "none" },
            orientation_applied,
        }
    } else {
        ProcessedImage {
            bytes: raw_bytes,
            mime: kind.mime(),
            width,
            height,
            resized: false,
            source_mime: None,
            animation: if animated { "preserved" } else { "none" },
            orientation_applied: false,
        }
    };
    for _ in 0..MAX_IMAGE_FIT_STEPS {
        if Instant::now() >= deadline {
            return Err("image processing timed out while fitting reply".into());
        }
        if base64_len(result.bytes.len()).is_some_and(|bytes| bytes <= budget) {
            return Ok(result);
        }
        let long_side = current.width().max(current.height());
        let at_floor = long_side <= MIN_FITTED_IMAGE_DIMENSION;
        if at_floor && result.resized {
            break;
        }
        let next = if at_floor {
            long_side
        } else {
            (long_side * 3 / 4).max(MIN_FITTED_IMAGE_DIMENSION)
        };
        let width =
            ((u64::from(current.width()) * u64::from(next)) / u64::from(long_side)).max(1) as u32;
        let height =
            ((u64::from(current.height()) * u64::from(next)) / u64::from(long_side)).max(1) as u32;
        if !at_floor {
            current = current.resize_exact(width, height, FilterType::Lanczos3);
        }
        if Instant::now() >= deadline {
            return Err("image processing timed out while fitting reply".into());
        }
        let (mut bytes, mut mime) = encode_fitted_image(&current, kind, allow_lossy)?;
        if mime != ImageKind::Jpeg.mime()
            && base64_len(bytes.len()).is_none_or(|bytes| bytes > budget)
            && allow_lossy
        {
            if Instant::now() >= deadline {
                return Err("image processing timed out while fitting reply".into());
            }
            let jpeg = encode_jpeg(&current)?;
            if jpeg.len() < bytes.len() {
                bytes = jpeg;
                mime = ImageKind::Jpeg.mime();
            }
        }
        result = ProcessedImage {
            bytes,
            mime,
            width,
            height,
            resized: true,
            source_mime: Some(kind.mime()),
            animation: if animated { "first_frame" } else { "none" },
            orientation_applied,
        };
        if at_floor {
            break;
        }
    }
    Ok(result)
}

fn image_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_ALLOC);
    limits
}

fn process_image(raw_bytes: Vec<u8>, kind: ImageKind) -> Result<ProcessedImage, String> {
    let source_mime = kind.mime();
    let mut reader =
        image::ImageReader::with_format(Cursor::new(raw_bytes.as_slice()), kind.format());
    reader.limits(image_decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(format_image_processing_error)?;
    let orientation = decoder
        .orientation()
        .map_err(format_image_processing_error)?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(format_image_processing_error)?;
    let (decoded_width, decoded_height) = decoded.dimensions();
    let animated = is_animated_image(&raw_bytes, kind);

    if decoded_width.max(decoded_height) <= MAX_INLINE_IMAGE_DIMENSION {
        return Ok(ProcessedImage {
            bytes: raw_bytes,
            mime: source_mime,
            width: decoded_width,
            height: decoded_height,
            resized: false,
            source_mime: None,
            animation: if animated { "preserved" } else { "none" },
            orientation_applied: false,
        });
    }

    let orientation_applied = orientation != Orientation::NoTransforms;
    if orientation_applied {
        decoded.apply_orientation(orientation);
    }
    let (oriented_width, oriented_height) = decoded.dimensions();
    let (target_width, target_height) = resized_dimensions(oriented_width, oriented_height);
    let resized = decoded.resize_exact(target_width, target_height, FilterType::Lanczos3);
    let (bytes, mime) = encode_resized_image(&resized, kind)?;

    Ok(ProcessedImage {
        bytes,
        mime,
        width: target_width,
        height: target_height,
        resized: true,
        source_mime: Some(source_mime),
        animation: if animated { "first_frame" } else { "none" },
        orientation_applied,
    })
}

fn format_image_processing_error(err: image::ImageError) -> String {
    match err {
        image::ImageError::Limits(_) => format!("image exceeds processing limits: {err}"),
        _ => format!("image decode failed: {err}"),
    }
}

fn resized_dimensions(width: u32, height: u32) -> (u32, u32) {
    let longer = width.max(height) as f64;
    let scale = MAX_INLINE_IMAGE_DIMENSION as f64 / longer;
    let target_width = ((width as f64 * scale).round() as u32).max(1);
    let target_height = ((height as f64 * scale).round() as u32).max(1);
    (target_width, target_height)
}

fn encode_resized_image(
    image: &DynamicImage,
    kind: ImageKind,
) -> Result<(Vec<u8>, &'static str), String> {
    match kind {
        ImageKind::Png => encode_png(image).map(|bytes| (bytes, ImageKind::Png.mime())),
        ImageKind::Jpeg => encode_jpeg(image).map(|bytes| (bytes, ImageKind::Jpeg.mime())),
        ImageKind::Gif => encode_gif(image).map(|bytes| (bytes, ImageKind::Gif.mime())),
        ImageKind::WebP => {
            let mut candidates = Vec::new();
            if let Ok(bytes) = encode_webp_lossless(image) {
                candidates.push((bytes, ImageKind::WebP.mime()));
            }
            if let Ok(bytes) = encode_png(image) {
                candidates.push((bytes, ImageKind::Png.mime()));
            }
            if let Ok(bytes) = encode_jpeg(image) {
                candidates.push((bytes, ImageKind::Jpeg.mime()));
            }
            candidates
                .into_iter()
                .min_by_key(|(bytes, _)| bytes.len())
                .ok_or_else(|| "image encode failed: no WebP resize encoder succeeded".to_string())
        }
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut output = Vec::new();
    PngEncoder::new(&mut output)
        .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("image encode failed: {err}"))?;
    Ok(output)
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, 85)
        .write_image(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)
        .map_err(|err| format!("image encode failed: {err}"))?;
    Ok(output)
}

fn encode_gif(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut output = Vec::new();
    GifEncoder::new(&mut output)
        .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("image encode failed: {err}"))?;
    Ok(output)
}

fn encode_webp_lossless(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut output = Vec::new();
    WebPEncoder::new_lossless(&mut output)
        .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("image encode failed: {err}"))?;
    Ok(output)
}

fn is_animated_image(bytes: &[u8], kind: ImageKind) -> bool {
    match kind {
        ImageKind::Gif => gif_has_multiple_frames(bytes),
        ImageKind::Png => png_has_animation_control_chunk(bytes),
        ImageKind::WebP => webp_has_animation(bytes),
        ImageKind::Jpeg => false,
    }
}

fn gif_has_multiple_frames(bytes: &[u8]) -> bool {
    if bytes.len() < 13 || !bytes.starts_with(b"GIF8") {
        return false;
    }
    let packed = bytes[10];
    let global_color_table_len = if packed & 0x80 != 0 {
        3usize.saturating_mul(1usize << ((packed & 0x07) + 1))
    } else {
        0
    };
    let mut index = 13usize.saturating_add(global_color_table_len);
    let mut frames = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            0x2C => {
                frames += 1;
                if frames > 1 {
                    return true;
                }
                if index + 10 > bytes.len() {
                    return false;
                }
                let packed = bytes[index + 9];
                index += 10;
                if packed & 0x80 != 0 {
                    index = index
                        .saturating_add(3usize.saturating_mul(1usize << ((packed & 0x07) + 1)));
                }
                if index >= bytes.len() {
                    return false;
                }
                index += 1; // LZW minimum code size.
                index = skip_gif_sub_blocks(bytes, index);
            }
            0x21 => {
                if index + 2 > bytes.len() {
                    return false;
                }
                index = skip_gif_sub_blocks(bytes, index + 2);
            }
            0x3B => return false,
            _ => return false,
        }
    }
    false
}

fn skip_gif_sub_blocks(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        let len = bytes[index] as usize;
        index += 1;
        if len == 0 {
            break;
        }
        index = index.saturating_add(len);
    }
    index
}

fn png_has_animation_control_chunk(bytes: &[u8]) -> bool {
    if bytes.len() < 8 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut index = 8usize;
    while index + 12 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[index],
            bytes[index + 1],
            bytes[index + 2],
            bytes[index + 3],
        ]) as usize;
        let chunk_type = &bytes[index + 4..index + 8];
        if chunk_type == b"acTL" {
            return true;
        }
        if chunk_type == b"IDAT" || chunk_type == b"IEND" {
            return false;
        }
        index = index.saturating_add(12).saturating_add(length);
    }
    false
}

fn webp_has_animation(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WEBP" {
        return false;
    }
    let mut index = 12usize;
    while index + 8 <= bytes.len() {
        let chunk_type = &bytes[index..index + 4];
        let chunk_len = u32::from_le_bytes([
            bytes[index + 4],
            bytes[index + 5],
            bytes[index + 6],
            bytes[index + 7],
        ]) as usize;
        let payload_start = index + 8;
        if chunk_type == b"VP8X" && payload_start < bytes.len() && bytes[payload_start] & 0x02 != 0
        {
            return true;
        }
        if chunk_type == b"ANIM" || chunk_type == b"ANMF" {
            return true;
        }
        index = payload_start
            .saturating_add(chunk_len)
            .saturating_add(chunk_len % 2);
    }
    false
}

/// Refusal for a line window whose start comes after its end. Clamping such a
/// window used to return an empty success, which reads exactly like "those
/// lines exist and are empty"; agents drew wrong conclusions from it.
fn inverted_range_error(id: &str, start_line: u64, end_line: u64) -> Response {
    Response::error(
        id,
        "invalid_range",
        inverted_range_message("startLine", start_line, end_line),
    )
}

/// The refusal text for a start line after the end line. The usual cause is
/// an end line sent as a line count (`startLine: 232, endLine: 125` meaning
/// "125 lines from 232"), so the message spells out the end line that
/// reading would take, and the offset/limit form that takes a count.
pub(crate) fn inverted_range_message(start_name: &str, start: u64, end: u64) -> String {
    let count_end = start.saturating_add(end) - 1;
    format!(
        "invalid_range: {start_name} {start} is after endLine {end}; no lines selected. endLine is a line number, not a count: to read {end} lines from line {start}, use endLine={count_end} (or offset={start}, limit={end}). Otherwise use a start line at or before the end line."
    )
}

/// Refusal for a window that starts after the last line of a non-empty file,
/// for the same reason as [`inverted_range_error`]: zero lines must never
/// look like a successful read.
fn start_past_end_error(id: &str, start_line: u64, total_lines: u32) -> Response {
    Response::error(
        id,
        "invalid_range",
        format!(
            "invalid_range: startLine {start_line} is past the end of the file ({total_lines} lines); no lines selected. Use a start line between 1 and {total_lines}."
        ),
    )
}

fn handle_registered_artifact_read(req: &RawRequest, bytes: Vec<u8>) -> Response {
    let byte_size = bytes.len();
    if let Some(media) = sniff_media(&bytes[..bytes.len().min(MEDIA_MAGIC_BYTES)]) {
        if byte_size as u64 > MAX_FILE_READ_BYTES {
            return media_file_too_large_response(&req.id, byte_size as u64);
        }
        return match media {
            SniffedMedia::Pdf => handle_pdf_media(&req.id, bytes),
            SniffedMedia::Image(kind) => match process_image_with_timeout(bytes, kind) {
                Ok(image) => image_attachment_response(&req.id, image),
                Err(reason) => media_omitted_response(&req.id, byte_size, reason),
            },
        };
    }
    if is_binary(&bytes) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size,
                "message": format!("Binary file ({} bytes), cannot display as text", byte_size),
            }),
        );
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "binary": true,
                    "complete": true,
                    "byte_size": byte_size,
                    "message": format!("Binary file ({} bytes), not valid UTF-8", byte_size),
                }),
            );
        }
    };
    let lines = content.lines().collect::<Vec<_>>();
    let total_lines = lines.len() as u32;
    let limit = req
        .params
        .get("limit")
        .and_then(|value| value.as_u64())
        .filter(|value| *value > 0)
        .map(|value| value as u32)
        .unwrap_or(DEFAULT_LIMIT);
    let start_line = req
        .params
        .get("start_line")
        .and_then(|value| value.as_u64())
        .map(|value| value.max(1) as u32)
        .unwrap_or(1);
    let explicit_end_line = req
        .params
        .get("end_line")
        .and_then(|value| value.as_u64())
        .filter(|value| *value > 0);
    if let Some(end_line) = explicit_end_line {
        if u64::from(start_line) > end_line {
            return inverted_range_error(&req.id, u64::from(start_line), end_line);
        }
    }
    if start_line > total_lines && total_lines > 0 {
        return start_past_end_error(&req.id, u64::from(start_line), total_lines);
    }
    let end_line = explicit_end_line
        .map(|value| value as u32)
        .unwrap_or_else(|| {
            start_line
                .saturating_add(limit)
                .saturating_sub(1)
                .min(total_lines)
        });
    let start = (start_line.saturating_sub(1) as usize).min(lines.len());
    let end = (end_line as usize).min(lines.len()).max(start);
    if start >= lines.len() {
        return Response::success(
            &req.id,
            serde_json::json!({
                "content": "",
                "complete": true,
                "total_lines": total_lines,
                "lines_read": 0,
                "start_line": start_line,
                "end_line": start_line,
                "truncated": false,
                "byte_size": byte_size,
            }),
        );
    }
    let mut output = String::new();
    let mut output_bytes = 0usize;
    let mut lines_read = 0u32;
    let mut truncated_by_size = false;
    let width = end.to_string().len();
    for (offset, line) in lines[start..end].iter().enumerate() {
        let line_number = start + offset + 1;
        let display = if line.len() > MAX_LINE_LENGTH {
            let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
            format!(
                "{:>width$}: {}... (truncated)\n",
                line_number,
                &line[..safe_end],
                width = width
            )
        } else {
            format!("{:>width$}: {}\n", line_number, line, width = width)
        };
        output_bytes = output_bytes.saturating_add(display.len());
        if output_bytes > MAX_BYTES {
            truncated_by_size = true;
            output.push_str(&format!(
                "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                MAX_BYTES / 1024
            ));
            break;
        }
        output.push_str(&display);
        lines_read = lines_read.saturating_add(1);
    }
    let actual_end = start_line + lines_read - u32::from(lines_read > 0);
    let truncated = start > 0 || end < lines.len() || truncated_by_size;
    Response::success(
        &req.id,
        serde_json::json!({
            "content": output,
            "complete": !truncated,
            "total_lines": total_lines,
            "lines_read": lines_read,
            "start_line": start_line,
            "end_line": actual_end,
            "truncated": truncated,
            "byte_size": byte_size,
        }),
    )
}

pub(crate) fn is_github_read_target(file: &str) -> bool {
    file.starts_with("issue://") || file.starts_with("pr://")
}

pub(crate) fn github_read_engine(ctx: &AppContext) -> Arc<GithubReadEngine> {
    let database_path = ctx.storage_dir().join("aft.db");
    let mut engines = GITHUB_READ_ENGINES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    Arc::clone(engines.entry(database_path.clone()).or_insert_with(|| {
        Arc::new(GithubReadEngine::new(
            sqlite_cache_store(database_path),
            Arc::new(GhCliFetcher::system()),
            Arc::new(ReqwestGithubImageDownloader),
            Arc::new(SystemGithubReadClock),
        ))
    }))
}

fn github_read_selector(req: &RawRequest) -> (GithubReadSelector, usize) {
    let start_line = req
        .params
        .get("start_line")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let end_line = req
        .params
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let limit = req
        .params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LIMIT as u64) as usize;
    (
        GithubReadSelector::LineRange {
            start_line,
            end_line,
            limit,
        },
        start_line,
    )
}

fn start_github_read(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<(GithubReadStart, usize), Response> {
    start_github_read_with_view(req, ctx, file, GithubReadView::Document)
}

fn start_github_read_with_view(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    view: GithubReadView,
) -> Result<(GithubReadStart, usize), Response> {
    if ctx.request_force_restrict(&req.id) {
        return Err(Response::error(
            &req.id,
            "external_fetch_restricted",
            "Network-backed GitHub reads are unavailable on restricted binds.",
        ));
    }
    let working_directory = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
    let (selector, start_line) = github_read_selector(req);
    let vision_capability = req.params.get("vision_capability").and_then(Value::as_bool);
    let authentication_identity = format!("session:{}", req.session());
    let gh_read = ctx.config().github.clone();
    github_read_engine(ctx)
        .start_resource_with_view(
            &gh_read,
            file,
            working_directory,
            authentication_identity,
            vision_capability,
            selector,
            view,
        )
        .map(|start| (start, start_line))
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
}

/// Longest a tool call waits for a live GitHub read, whatever budget it has.
const GITHUB_READ_WAIT_CEILING: Duration = Duration::from_secs(20);
/// Time kept back from the call's budget so the reply can still be built.
const GITHUB_READ_RETURN_MARGIN: Duration = Duration::from_secs(1);

/// Wait for a GitHub read no longer than the call's budget (less a reply
/// margin) or [`GITHUB_READ_WAIT_CEILING`]. A read still running then is
/// answered with its cached copy, or as pending; it keeps running and fills
/// the cache for the next call.
pub(crate) fn wait_for_github_read(
    start: GithubReadStart,
) -> Result<GithubReadCompletion, crate::github_read::GithubReadError> {
    match start {
        GithubReadStart::Immediate(completion) => Ok(completion),
        GithubReadStart::Deferred(pending) => pending.wait_until(github_read_deadline()),
    }
}

fn github_read_deadline() -> std::time::Instant {
    let now = std::time::Instant::now();
    let ceiling = now + GITHUB_READ_WAIT_CEILING;
    match crate::bounded_io::current() {
        Some(budget) => budget
            .deadline
            .checked_sub(GITHUB_READ_RETURN_MARGIN)
            .unwrap_or(now)
            .clamp(now, ceiling),
        None => ceiling,
    }
}

fn github_read_response(
    req: &RawRequest,
    mut completion: GithubReadCompletion,
    start_line: usize,
) -> Response {
    let (start_line, lines_read, actual_end, truncated) = if let Some(page) = &completion.diff_page
    {
        (
            page.start_line,
            page.lines_read,
            page.end_line,
            page.truncated,
        )
    } else {
        let lines_read = completion.content.lines().count();
        let actual_end = start_line.saturating_add(lines_read.saturating_sub(1));
        (
            start_line,
            lines_read,
            actual_end,
            lines_read > 0 && (start_line > 1 || actual_end < completion.total_lines),
        )
    };
    let attachments = if crate::subc::result_body::active() {
        match fit_github_images(req, &mut completion) {
            Ok(attachments) => attachments,
            Err(response) => return response,
        }
    } else {
        completion
            .attachments
            .into_iter()
            .map(|attachment| {
                let bytes = attachment.bytes.len();
                let data = BASE64.encode(&attachment.bytes);
                let base64_bytes = data.len();
                serde_json::json!({
                    "kind": "image",
                    "mime": attachment.mime,
                    "data": data,
                    "bytes": bytes,
                    "base64_bytes": base64_bytes,
                    "source_url": attachment.source_url,
                })
            })
            .collect::<Vec<_>>()
    };
    let mut data = serde_json::json!({
        "content": completion.content,
        "attachments": attachments,
        "complete": !truncated,
        "total_lines": completion.total_lines,
        "lines_read": lines_read,
        "start_line": start_line,
        "end_line": actual_end,
        "truncated": truncated,
    });
    if let (Some(note), Some(data)) = (completion.freshness.note(), data.as_object_mut()) {
        data.insert(
            "freshness_note".to_string(),
            Value::String(note.to_string()),
        );
    }
    Response::success(&req.id, data)
}

fn fit_github_images(
    req: &RawRequest,
    completion: &mut GithubReadCompletion,
) -> Result<Vec<Value>, Response> {
    let images = std::mem::take(&mut completion.attachments);
    let links = images
        .iter()
        .map(|image| image.source_url.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // Reserve the complete omission note before choosing any image. Replies with
    // structuredContent serialize text three times and images twice; reserving
    // that worst case also fits connections that receive content only.
    let reserved_text = format!(
        "{}\n\n{} more images not attached: {links}",
        completion.content,
        images.len()
    );
    let text_bytes = serde_json::to_vec(&reserved_text)
        .expect("read text is JSON")
        .len()
        .saturating_mul(3);
    if text_bytes.saturating_add(IMAGE_REPLY_MARGIN_BYTES)
        >= crate::subc::result_body::MAX_RESULT_BYTES
    {
        return Err(crate::subc::result_body::too_large(
            &req.id,
            text_bytes.saturating_add(IMAGE_REPLY_MARGIN_BYTES),
            "read a page with fewer or smaller images",
        ));
    }
    let mut remaining = crate::subc::result_body::MAX_RESULT_BYTES
        .saturating_sub(IMAGE_REPLY_MARGIN_BYTES)
        .saturating_sub(text_bytes)
        .saturating_sub(1)
        / 2;
    // All images share one processing window, without consuming the time already
    // reserved to return a network-backed read to its caller.
    let now = Instant::now();
    let deadline = crate::bounded_io::current().map_or(now + IMAGE_PROCESS_TIMEOUT, |budget| {
        budget
            .deadline
            .checked_sub(GITHUB_READ_RETURN_MARGIN)
            .unwrap_or(now)
            .max(now)
            .min(now + IMAGE_PROCESS_TIMEOUT)
    });
    let mut attached = Vec::new();
    let mut omitted = Vec::new();
    let mut stopped = false;
    for image in images {
        if !stopped && remaining > 0 && Instant::now() < deadline {
            if let Some(SniffedMedia::Image(kind)) = sniff_media(&image.bytes) {
                if let Ok(processed) =
                    process_image_with_budget_timeout(image.bytes, kind, Some(remaining), deadline)
                {
                    let encoded = base64_len(processed.bytes.len()).unwrap_or(usize::MAX);
                    if encoded <= remaining {
                        let mut response = image_attachment_response_with_budget(
                            &req.id,
                            processed,
                            Some(remaining),
                        );
                        if let Some(mut attachment) = response
                            .data
                            .get_mut("attachments")
                            .and_then(Value::as_array_mut)
                            .and_then(Vec::pop)
                        {
                            attachment["source_url"] = serde_json::json!(image.source_url);
                            attachment["alt"] = serde_json::json!(image.source_url);
                            remaining -= encoded;
                            attached.push(attachment);
                            continue;
                        }
                    }
                }
            }
        }
        // Attach a prefix only. A failure or exhausted budget never makes a later
        // image jump ahead of an earlier one; every omitted source remains visible.
        stopped = true;
        omitted.push(image.source_url);
    }
    if !omitted.is_empty() {
        let more = if attached.is_empty() { "" } else { "more " };
        let noun = if omitted.len() == 1 {
            "image"
        } else {
            "images"
        };
        completion.content.push_str(&format!(
            "\n\n{} {more}{noun} not attached: {}",
            omitted.len(),
            omitted.join(", ")
        ));
    }
    Ok(attached)
}

fn handle_github_read(req: &RawRequest, ctx: &AppContext, file: &str) -> Response {
    let (start, start_line) = match start_github_read(req, ctx, file) {
        Ok(start) => start,
        Err(response) => return response,
    };
    match wait_for_github_read(start) {
        Ok(completion) => github_read_response(req, completion, start_line),
        Err(error) => Response::error(&req.id, error.code(), error.to_string()),
    }
}

/// Render a concise live GitHub index through the same gate and fetch engine as
/// `read`. A failed refresh returns the cache's explicitly labelled read copy.
pub(crate) fn handle_github_outline(req: &RawRequest, ctx: &AppContext, target: &str) -> Response {
    let (start, _) = match start_github_read_with_view(req, ctx, target, GithubReadView::Outline) {
        Ok(start) => start,
        Err(response) => return response,
    };
    match wait_for_github_read(start) {
        Ok(completion) => Response::success(
            &req.id,
            serde_json::json!({
                "text": completion.content,
                "complete": matches!(completion.freshness, crate::github_read::GithubReadFreshness::Fetched),
            }),
        ),
        Err(error) => Response::error(&req.id, error.code(), error.to_string()),
    }
}

/// Zoom GitHub discussion ordinals by reusing `read`'s selector renderer.
pub(crate) fn handle_github_zoom(
    req: &RawRequest,
    ctx: &AppContext,
    target: &str,
    selector: &str,
) -> Response {
    let resource = match parse_resource(target) {
        Ok(resource) if resource.diff_path.is_some() => {
            return Response::error(
                &req.id,
                "invalid_resource",
                "Use read for diffs; aft_zoom does not support diff links",
            )
        }
        Ok(resource) if resource.comment_selector.is_none() => resource,
        Ok(_) => return Response::error(
            &req.id,
            "invalid_resource",
            "GitHub zoom target must not include /comments; pass discussion ordinals in symbols",
        ),
        Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
    };
    let selected = format!("{}/comments/{selector}", resource.base_spelling());
    let (start, _) = match start_github_read(req, ctx, &selected) {
        Ok(start) => start,
        Err(response) => return response,
    };
    match wait_for_github_read(start) {
        Ok(completion) => {
            let end_line = completion.content.lines().count().max(1) as u32;
            Response::success(
                &req.id,
                serde_json::json!({
                    "name": format!("items {selector}"),
                    "kind": "github-discussion",
                    "range": {
                        "start_line": 1,
                        "start_col": 1,
                        "end_line": end_line,
                        "end_col": 1,
                    },
                    "content": completion.content,
                    "context_before": [],
                    "context_after": [],
                    "annotations": { "calls_out": [], "called_by": [] },
                }),
            )
        }
        Err(error) => Response::error(&req.id, error.code(), error.to_string()),
    }
}

/// Build a standalone `read` outcome without waiting on network-backed GitHub work.
pub fn build_read_outcome(req: RawRequest, ctx: &AppContext) -> DispatchOutcome {
    let Some(file) = req
        .params
        .get("file")
        .and_then(Value::as_str)
        .filter(|file| is_github_read_target(file))
        .map(str::to_owned)
    else {
        return DispatchOutcome::Immediate(handle_read(&req, ctx));
    };
    let (start, start_line) = match start_github_read(&req, ctx, &file) {
        Ok(start) => start,
        Err(response) => return DispatchOutcome::Immediate(response),
    };
    let pending = match start {
        GithubReadStart::Immediate(completion) => {
            return DispatchOutcome::Immediate(github_read_response(&req, completion, start_line));
        }
        GithubReadStart::Deferred(pending) => pending,
    };

    let request_id = req.id.clone();
    let session_id = req.session().to_string();
    if let Some(completion) = pending.try_complete() {
        return DispatchOutcome::Immediate(match completion {
            Ok(completion) => github_read_response(&req, completion, start_line),
            Err(error) => Response::error(&req.id, error.code(), error.to_string()),
        });
    }

    // The deferred route has the same deadline as a blocking wait: once it
    // passes, the late answer (the cached copy, or pending) is given, and the
    // poll timer is set to wake the response at the deadline even if the
    // fetch never sends anything.
    let started = std::time::Instant::now();
    let deadline = github_read_deadline();
    let (receiver, late) = pending.into_parts();
    let deferred = PendingResponse::from_receiver(
        request_id,
        session_id,
        "read".to_string(),
        receiver,
        move |_, result| {
            let completion = match result {
                Ok(completion) => completion,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if std::time::Instant::now() < deadline {
                        return None;
                    }
                    late.answer(deadline.saturating_duration_since(started))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err(crate::github_read::worker_stopped())
                }
            };
            Some(match completion {
                Ok(completion) => github_read_response(&req, completion, start_line),
                Err(error) => Response::error(&req.id, error.code(), error.to_string()),
            })
        },
    );
    let until_deadline = deadline
        .saturating_duration_since(started)
        .max(Duration::from_millis(1));
    let wake = deferred
        .poll_interval()
        .map_or(until_deadline, |interval| interval.min(until_deadline));
    DispatchOutcome::Deferred(deferred.with_poll_interval(wake))
}

/// Handle a `read` request.
///
/// Params:
///   - `file` (string, required) — path to file or directory
///   - `start_line` (u32, optional) — 1-based start line (default: 1)
///   - `end_line` (u32, optional) — 1-based end line (default: start_line + limit - 1)
///   - `limit` (u32, optional) — max lines to return (default: 2000)
///
/// Returns for files:
///   `{ content, complete, total_lines?, lines_read, start_line, end_line, truncated, byte_size }`
///
/// `complete` is false whenever the returned content is a slice/truncated. Full
/// reads report exact `total_lines`; explicit ranged reads omit `total_lines`
/// when they stop after the one-line lookahead and therefore do not know EOF.
///
/// Returns for directories:
///   `{ entries[], total_entries }`
///
/// Returns for binary files:
///   `{ binary: true, byte_size }`
pub fn handle_read(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut response = crate::bounded_io::command(req, ctx, handle_read_inner);
    if crate::subc::result_body::active() {
        if let Some(attachments) = response
            .data
            .get_mut("attachments")
            .and_then(Value::as_array_mut)
        {
            for attachment in attachments
                .iter_mut()
                .filter(|part| part["kind"] == "image")
            {
                if !attachment["alt"].is_string() {
                    attachment["alt"] = serde_json::json!(req
                        .params
                        .get("file")
                        .and_then(Value::as_str)
                        .unwrap_or("read image"));
                }
            }
        }
    }
    response
}

fn handle_read_inner(req: &RawRequest, ctx: &AppContext) -> Response {
    if let Some(reference) = crate::revision_read::requested_ref(req) {
        return handle_revision_read(req, ctx, reference);
    }
    if let Some(file) = req.params.get("file").and_then(Value::as_str) {
        if is_github_read_target(file) {
            return handle_github_read(req, ctx, file);
        }
    }

    let binding_root = ctx
        .canonical_cache_root_opt()
        .or_else(|| ctx.config().project_root.clone());
    let guard = binding_root.and_then(|root| {
        ctx.hashline_bindings()
            .capture(root, req.session().to_string())
    });
    let capture_source = guard.as_ref().is_some_and(|guard| guard.effective())
        && (req.params.get("_hashline_bash_read_kind").is_none()
            || ctx.config().experimental_bash_rewrite);
    let mut captured_source = None;
    let mut response = handle_read_local(req, ctx, capture_source, &mut captured_source);
    if !response.success
        || response
            .data
            .get("content")
            .and_then(Value::as_str)
            .is_none()
    {
        return response;
    }

    let Some(guard) = guard else {
        return response;
    };
    if !guard.effective() {
        return response;
    }

    let Some(file) = req.params.get("file").and_then(Value::as_str) else {
        return response;
    };
    let path = if let Some(captured) = &captured_source {
        captured.path.clone()
    } else {
        match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
            Ok(path) => path.as_path().to_path_buf(),
            Err(_) => return response,
        }
    };
    let requested_path = req
        .params
        .get("_hashline_requested_path")
        .and_then(Value::as_str)
        .unwrap_or(file)
        .to_string();
    let bash_kind = req
        .params
        .get("_hashline_bash_read_kind")
        .and_then(Value::as_str)
        .and_then(|kind| match kind {
            "cat" => Some(crate::hashline::snapshot::BashReadKind::Cat),
            "head" => Some(crate::hashline::snapshot::BashReadKind::Head {
                lines: req
                    .params
                    .get("_hashline_bash_read_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(10) as usize,
            }),
            "tail" => Some(crate::hashline::snapshot::BashReadKind::Tail {
                lines: req
                    .params
                    .get("_hashline_bash_read_lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(10) as usize,
            }),
            _ => None,
        });
    let selection = read_selection(req, &response);
    let publication = guard.with_binding_mut(|binding| {
        if let Some(captured) = &captured_source {
            #[cfg(test)]
            let reuse = !LEGACY_HASHLINE_READ.with(|legacy| legacy.get());
            #[cfg(not(test))]
            let reuse = true;
            if reuse {
                return crate::hashline::snapshot::capture_taggable_source_with_options(
                    binding.snapshots_mut(),
                    &path,
                    requested_path.clone(),
                    selection,
                    crate::hashline::snapshot::RenderOptions::default(),
                    &captured.source,
                );
            }
        }
        if let Some(kind) = bash_kind {
            crate::hashline::snapshot::capture_bash_rewrite_read(
                binding.snapshots_mut(),
                &path,
                requested_path.clone(),
                kind,
                ctx.config().experimental_bash_rewrite,
                true,
                true,
            )
        } else {
            crate::hashline::snapshot::capture_taggable_read(
                binding.snapshots_mut(),
                &path,
                requested_path.clone(),
                selection,
            )
        }
    });
    let Ok(publication) = publication else {
        return if captured_source.is_some() {
            handle_read_legacy(req, ctx)
        } else {
            response
        };
    };

    if let Some(data) = response.data.as_object_mut() {
        match publication {
            crate::hashline::snapshot::ReadPublication::Tagged {
                snapshot,
                rendering,
            } => {
                let first_line = rendering.rendered_lines.iter().next().copied().unwrap_or(1);
                let last_line = rendering
                    .rendered_lines
                    .iter()
                    .next_back()
                    .copied()
                    .unwrap_or(first_line);
                data.insert("content".into(), Value::String(rendering.text));
                data.insert(
                    "requested_path".into(),
                    Value::String(rendering.requested_path),
                );
                data.insert("hashline_tag".into(), Value::String(rendering.tag));
                data.insert(
                    "lines_read".into(),
                    Value::from(rendering.rendered_lines.len()),
                );
                data.insert("start_line".into(), Value::from(first_line));
                data.insert("end_line".into(), Value::from(last_line));
                data.insert("total_lines".into(), Value::from(snapshot.total_lines));
            }
            crate::hashline::snapshot::ReadPublication::Tagless { rendering, reason } => {
                if !rendering.text.is_empty() {
                    data.insert("content".into(), Value::String(rendering.text));
                }
                data.insert(
                    "requested_path".into(),
                    Value::String(rendering.requested_path),
                );
                data.insert(
                    "hashline_tag_unavailable_reason".into(),
                    Value::String(reason.to_string()),
                );
                if matches!(
                    reason,
                    crate::hashline::snapshot::UntaggableReason::Oversize { .. }
                ) {
                    let content = data
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    data.insert(
                        "content".into(),
                        Value::String(format!("{content}[hashline tag unavailable: {reason}]\n")),
                    );
                }
            }
        }
    }
    response
}

/// Read `file` as it is at the git revision `reference`.
///
/// The path passes the same read checks as a working-tree read; then the blob
/// is copied out of the object store and rendered by the ordinary file path,
/// so ranges, binary detection, media and size limits behave as they do for
/// a file on disk. No hashline tag is minted: the content is not the working
/// file an edit would change.
fn handle_revision_read(req: &RawRequest, ctx: &AppContext, reference: &str) -> Response {
    if let Err(refusal) = crate::revision_read::check_enabled(req, ctx) {
        return refusal;
    }
    let Some(file) = req.params.get("file").and_then(Value::as_str) else {
        return Response::error(
            &req.id,
            "invalid_request",
            "read: missing required param 'file'",
        );
    };
    if is_github_read_target(file) {
        return Response::error(
            &req.id,
            "invalid_request",
            "read: 'ref' reads repository files; a GitHub issue or pull request has no git revision",
        );
    }
    if let Err(refusal) = ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
        return refusal;
    }
    let session = match crate::revision_read::RevisionSession::open(
        req,
        ctx,
        reference,
        Some(Path::new(file)),
    ) {
        Ok(session) => session,
        Err(refusal) => return refusal,
    };
    let staged = match session.materialize(req, ctx, Path::new(file), file) {
        Ok(staged) => staged,
        Err(refusal) => return refusal,
    };
    let mut response = handle_read_path(req, &staged, false, &mut None).into_response(&req.id);
    session.annotate(&mut response);
    response
}

struct ReadWindow {
    start: u32,
    end: Option<u64>,
    limit: u32,
}

/// Normalize zero placeholders and line bounds once so physical reads and
/// hashline snapshots select the same 1-based window.
fn read_window(req: &RawRequest) -> Result<ReadWindow, Response> {
    let positive = |field: &str| -> Result<Option<u32>, Response> {
        req.params
            .get(field)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .map(|value| {
                u32::try_from(value).map_err(|_| {
                    Response::error(
                        &req.id,
                        "invalid_request",
                        format!(
                            "read: '{field}' exceeds the maximum line value {}",
                            u32::MAX
                        ),
                    )
                })
            })
            .transpose()
    };
    Ok(ReadWindow {
        start: positive("start_line")?.unwrap_or(1),
        end: positive("end_line")?.map(u64::from),
        limit: positive("limit")?.unwrap_or(DEFAULT_LIMIT),
    })
}

fn read_selection(
    req: &RawRequest,
    response: &Response,
) -> crate::hashline::snapshot::ReadSelection {
    if let Some(kind) = req
        .params
        .get("_hashline_bash_read_kind")
        .and_then(Value::as_str)
    {
        let lines = req
            .params
            .get("_hashline_bash_read_lines")
            .and_then(Value::as_u64)
            .unwrap_or(10) as usize;
        return match kind {
            "cat" => crate::hashline::snapshot::ReadSelection::WholeFile,
            "head" => crate::hashline::snapshot::ReadSelection::Head(lines),
            "tail" => crate::hashline::snapshot::ReadSelection::Tail(lines),
            _ => crate::hashline::snapshot::ReadSelection::WholeFile,
        };
    }

    let Ok(window) = read_window(req) else {
        return crate::hashline::snapshot::ReadSelection::WholeFile;
    };
    let start = window.start as usize;
    let explicit_end = window.end;
    if req.params.get("start_line").is_some() || explicit_end.is_some() {
        let limit = window.limit as usize;
        let end = explicit_end
            .map(|value| value as usize)
            .unwrap_or_else(|| start.saturating_add(limit).saturating_sub(1));
        return crate::hashline::snapshot::ReadSelection::Range { start, end };
    }

    if response.data.get("total_lines").and_then(Value::as_u64) == Some(0) {
        crate::hashline::snapshot::ReadSelection::WholeFile
    } else {
        crate::hashline::snapshot::ReadSelection::Head(window.limit as usize)
    }
}

fn handle_read_legacy(req: &RawRequest, ctx: &AppContext) -> Response {
    handle_read_local(req, ctx, false, &mut None)
}

struct CapturedRead {
    path: std::path::PathBuf,
    source: crate::hashline::snapshot::ReadSource,
}

#[cfg(test)]
thread_local! {
    pub(crate) static LEGACY_HASHLINE_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static LEGACY_READ_RENDER_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static IMAGE_PROCESSING_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

#[cfg(test)]
pub(crate) struct HelperStateScope(bool, usize, Duration);
#[cfg(test)]
impl Drop for HelperStateScope {
    fn drop(&mut self) {
        LEGACY_HASHLINE_READ.with(|flag| flag.set(self.0));
        LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.set(self.1));
        IMAGE_PROCESSING_DELAY.with(|delay| delay.set(self.2));
    }
}
#[cfg(test)]
pub(crate) fn helper_state_scope() -> HelperStateScope {
    HelperStateScope(
        LEGACY_HASHLINE_READ.with(|flag| flag.replace(false)),
        LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.replace(0)),
        IMAGE_PROCESSING_DELAY.with(|delay| delay.replace(Duration::ZERO)),
    )
}

fn handle_read_local(
    req: &RawRequest,
    ctx: &AppContext,
    capture_source: bool,
    captured_source: &mut Option<CapturedRead>,
) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "read: missing required param 'file'",
            );
        }
    };

    let requested = Path::new(file);
    if ctx
        .bash_background()
        .is_session_owned_artifact_path(req.session(), requested)
        && !ctx
            .bash_background()
            .is_readable_session_owned_artifact_path(req.session(), requested)
    {
        return Response::error(
            &req.id,
            "artifact_refused",
            "read: task artifact has multiple hard links",
        );
    }

    let path = match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    if let Some(result) = ctx
        .bash_background()
        .read_artifact_path(req.session(), path.as_path())
    {
        return match result {
            Ok(bytes) => handle_registered_artifact_read(req, bytes),
            Err(error) => Response::error(
                &req.id,
                "artifact_refused",
                format!("read: task artifact refused: {error}"),
            ),
        };
    }

    // The helper owns only request data and file bytes, never the actor or its
    // reader hold. Include stat, directory iteration, open, and streaming reads:
    // any of those can wait in the kernel on a protected or remote volume.
    let owned_req = req.clone();
    #[cfg(test)]
    let legacy = LEGACY_HASHLINE_READ.with(|legacy| legacy.get());
    #[cfg(test)]
    let image_delay = IMAGE_PROCESSING_DELAY.with(|delay| delay.get());
    let result = crate::bounded_io::run(path.as_path(), None, move |path| {
        #[cfg(test)]
        LEGACY_HASHLINE_READ.with(|value| value.set(legacy));
        #[cfg(test)]
        IMAGE_PROCESSING_DELAY.with(|value| value.set(image_delay));
        let mut captured = None;
        let response = handle_read_path(&owned_req, &path, capture_source, &mut captured);
        #[cfg(test)]
        let observations = (
            crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.get()),
            LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.get()),
        );
        #[cfg(not(test))]
        let observations = ();
        Ok((response, captured, observations))
    });
    match result {
        Ok((response, captured, _observations)) => {
            #[cfg(test)]
            {
                crate::hashline::snapshot::SOURCE_READS
                    .with(|reads| reads.set(reads.get() + _observations.0));
                LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.set(bytes.get() + _observations.1));
            }
            *captured_source = captured;
            // Decoding, resizing and base64 encoding are CPU work, not OS access.
            // Run them after the filesystem helper returns so a slow image uses
            // the image-processing timeout rather than the filesystem deadline.
            response.into_response(&req.id)
        }
        Err(error) => Response::error(&req.id, "read_blocked", error.to_string()),
    }
}

fn handle_read_path(
    req: &RawRequest,
    path: &Path,
    capture_source: bool,
    captured_source: &mut Option<CapturedRead>,
) -> PreparedPathRead {
    let file = path.to_string_lossy();
    // Check existence
    if !path.exists() {
        return PreparedPathRead::Response(Response::error(
            &req.id,
            "not_found",
            format!("read: file not found: {}", file),
        ));
    }

    // Directory listing
    if path.is_dir() {
        return PreparedPathRead::Response(handle_directory(req, path));
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) => {
            return PreparedPathRead::Response(Response::error(
                &req.id,
                "io_error",
                format!("read: failed to stat file: {}", e),
            ));
        }
    };

    if !metadata.is_file() {
        return PreparedPathRead::Response(Response::error(
            &req.id,
            "unsupported_file_type",
            "read: only regular files and directories can be read",
        ));
    }

    let magic = match read_magic(path) {
        Ok(magic) => magic,
        Err(e) => {
            return PreparedPathRead::Response(Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file header: {}", e),
            ));
        }
    };
    if let Some(media) = sniff_media(&magic) {
        return prepare_media_read(
            req,
            path,
            metadata.len(),
            media,
            capture_source,
            captured_source,
        );
    }

    PreparedPathRead::Response(handle_text_or_binary_read(
        req,
        path,
        metadata.len(),
        capture_source,
        captured_source,
    ))
}

fn handle_text_or_binary_read(
    req: &RawRequest,
    path: &Path,
    file_byte_size: u64,
    capture_source: bool,
    captured_source: &mut Option<CapturedRead>,
) -> Response {
    // Parse range parameters. Zero is outside the 1-based domain of every
    // range field, so a `0` placeholder counts as absent rather than selecting
    // an empty window (a zero limit or end line would read no lines at all).
    let window = match read_window(req) {
        Ok(window) => window,
        Err(response) => return response,
    };
    let limit = window.limit;
    let start_line = window.start;
    let explicit_end_line = window.end;
    let has_explicit_range = req.params.get("start_line").is_some()
        || explicit_end_line.is_some()
        || req.params.get("limit").is_some();

    if let Some(end_line) = explicit_end_line {
        if u64::from(start_line) > end_line {
            return inverted_range_error(&req.id, u64::from(start_line), end_line);
        }
    }

    if has_explicit_range {
        return handle_streaming_range_read(
            req,
            path,
            file_byte_size,
            start_line,
            explicit_end_line,
            limit,
        );
    }

    if file_byte_size > MAX_FILE_READ_BYTES {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "read: file is too large to load at once ({} bytes > {} bytes). Use start_line/end_line to read sections.",
                file_byte_size,
                MAX_FILE_READ_BYTES
            ),
        );
    }

    // Read raw bytes for binary detection
    let read_source = capture_source
        .then(|| crate::hashline::snapshot::read_source(path).ok())
        .flatten()
        .filter(|source| source.has_complete_bytes());
    let fallback_bytes;
    let raw_bytes = if let Some(source) = &read_source {
        source.bytes()
    } else {
        fallback_bytes = match crate::bounded_io::read(path, None) {
            Ok(b) => b,
            Err(e) => {
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("read: failed to read file: {}", e),
                );
            }
        };
        &fallback_bytes
    };

    let byte_size = raw_bytes.len();

    // Binary detection
    if is_binary(raw_bytes) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size,
                "message": format!("Binary file ({} bytes), cannot display as text", byte_size),
            }),
        );
    }

    // Convert to string
    let content = match std::str::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "binary": true,
                    "complete": true,
                    "byte_size": byte_size,
                    "message": format!("Binary file ({} bytes), not valid UTF-8", byte_size),
                }),
            );
        }
    };

    #[cfg(test)]
    let skip_render = read_source.is_some() && !LEGACY_HASHLINE_READ.with(|legacy| legacy.get());
    #[cfg(not(test))]
    let skip_render = read_source.is_some();
    let response = render_text_read(
        req,
        content,
        byte_size,
        start_line,
        explicit_end_line,
        limit,
        skip_render,
    );
    if let Some(source) = read_source {
        *captured_source = Some(CapturedRead {
            path: path.to_path_buf(),
            source,
        });
    }
    response
}

fn render_text_read(
    req: &RawRequest,
    content: &str,
    byte_size: usize,
    start_line: u32,
    explicit_end_line: Option<u64>,
    limit: u32,
    skip_render: bool,
) -> Response {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len() as u32;

    let end_line = explicit_end_line.map(|v| v as u32).unwrap_or_else(|| {
        start_line
            .saturating_add(limit)
            .saturating_sub(1)
            .min(total_lines)
    });

    // Clamp to actual line count. `.max(start_idx)` keeps the slice below
    // from panicking; inverted windows were already refused above, so it
    // never turns a real request into an empty one.
    let start_idx = (start_line.saturating_sub(1) as usize).min(lines.len());
    let end_idx = (end_line as usize).min(lines.len()).max(start_idx);

    if start_idx >= lines.len() {
        return Response::success(
            &req.id,
            serde_json::json!({
                "content": "",
                "complete": true,
                "total_lines": total_lines,
                "lines_read": 0,
                "start_line": start_line,
                "end_line": start_line,
                "truncated": false,
                "byte_size": byte_size,
            }),
        );
    }

    // Build line-numbered output with truncation
    let mut output = String::new();
    let mut output_bytes = 0usize;
    let mut lines_read = 0u32;
    let mut truncated_by_size = false;

    let line_num_width = format!("{}", end_idx).len();

    for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
        let line_num = start_idx + i + 1; // 1-based
        let (display_text, truncated_line) = if line.len() > MAX_LINE_LENGTH {
            // Find a safe UTF-8 boundary at or before MAX_LINE_LENGTH to avoid
            // panicking on multi-byte characters (e.g. emoji, CJK).
            let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
            (&line[..safe_end], true)
        } else {
            (*line, false)
        };
        let display_size = line_num_width
            + 2
            + display_text.len()
            + 1
            + if truncated_line {
                "... (truncated)".len()
            } else {
                0
            };
        let display_line = if skip_render {
            String::new()
        } else if truncated_line {
            format!(
                "{:>width$}: {}... (truncated)\n",
                line_num,
                display_text,
                width = line_num_width
            )
        } else {
            format!(
                "{:>width$}: {}\n",
                line_num,
                display_text,
                width = line_num_width
            )
        };

        output_bytes += display_size;
        if output_bytes > MAX_BYTES {
            truncated_by_size = true;
            // Add truncation notice
            if !skip_render {
                output.push_str(&format!(
                    "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                    MAX_BYTES / 1024
                ));
            }
            break;
        }

        output.push_str(&display_line);
        #[cfg(test)]
        LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.set(bytes.get() + display_line.len()));
        lines_read += 1;
    }

    let actual_end = start_line + lines_read - if lines_read > 0 { 1 } else { 0 };
    let has_more = (start_idx > 0) || (end_idx as u32) < total_lines || truncated_by_size;

    Response::success(
        &req.id,
        serde_json::json!({
            "content": output,
            "complete": !has_more,
            "total_lines": total_lines,
            "lines_read": lines_read,
            "start_line": start_line,
            "end_line": actual_end,
            "truncated": has_more,
            "byte_size": byte_size,
        }),
    )
}

/// Discard a line using only the reader's fixed-size buffer. Skipped bytes count
/// toward the 50 MiB input ceiling, not the much smaller rendered-response budget.
fn skip_streamed_line(reader: &mut impl BufRead, budget: usize) -> std::io::Result<(usize, bool)> {
    let mut consumed = 0;
    while consumed < budget {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((consumed, true));
        }
        let available = &buffer[..buffer.len().min(budget - consumed)];
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        reader.consume(count);
        consumed += count;
        if newline.is_some() {
            return Ok((consumed, true));
        }
    }
    Ok((consumed, false))
}

fn handle_streaming_range_read(
    req: &RawRequest,
    path: &Path,
    byte_size: u64,
    start_line: u32,
    explicit_end_line: Option<u64>,
    limit: u32,
) -> Response {
    let mut file = match crate::task_temp_reads::open(path) {
        Ok(file) => file,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    let mut sample = [0u8; BINARY_SAMPLE_BYTES];
    let sample_len = match file.read(&mut sample) {
        Ok(len) => len,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    if is_binary(&sample[..sample_len]) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size as usize,
                "message": format!("Binary file ({} bytes), cannot display as text", byte_size),
            }),
        );
    }

    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        return Response::error(
            &req.id,
            "io_error",
            format!("read: failed to read file: {}", e),
        );
    }

    let requested_end_line = explicit_end_line
        .map(|v| v as u32)
        .unwrap_or_else(|| start_line.saturating_add(limit).saturating_sub(1));
    let requested_start_idx = start_line.saturating_sub(1) as usize;
    let requested_end_idx = (requested_end_line as usize).max(requested_start_idx);

    let mut selected_lines = Vec::new();
    let mut observed_lines = 0u32;
    let mut invalid_utf8 = false;
    let mut has_more_after_range = false;
    let mut reader = std::io::BufReader::new(file);
    let mut scanned_bytes = 0usize;
    let mut retained_bytes = 0usize;
    let mut scan_gap = false;
    let scan_ceiling = MAX_FILE_READ_BYTES as usize;
    loop {
        let index = observed_lines as usize;
        if index >= requested_end_idx {
            has_more_after_range = reader.fill_buf().map_or(true, |bytes| !bytes.is_empty());
            break;
        }
        if index < requested_start_idx {
            let (consumed, finished) =
                match skip_streamed_line(&mut reader, scan_ceiling.saturating_sub(scanned_bytes)) {
                    Ok(result) => result,
                    Err(error) => {
                        return Response::error(
                            &req.id,
                            "io_error",
                            format!("read: failed to skip prefix: {error}"),
                        )
                    }
                };
            scanned_bytes += consumed;
            if !finished {
                scan_gap = true;
                break;
            }
            if consumed == 0 {
                break;
            }
            observed_lines = observed_lines.saturating_add(1);
            continue;
        }
        let allowance = (MAX_LINE_LENGTH + 4).min(scan_ceiling.saturating_sub(scanned_bytes));
        if allowance == 0 || retained_bytes >= MAX_BYTES {
            scan_gap = true;
            break;
        }
        let mut bytes = Vec::new();
        let len = match reader
            .by_ref()
            .take(allowance as u64)
            .read_until(b'\n', &mut bytes)
        {
            Ok(len) => len,
            Err(error) => {
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("read: failed to read file: {error}"),
                )
            }
        };
        if len == 0 {
            break;
        }
        scanned_bytes += len;
        let partial_line = bytes.last() != Some(&b'\n') && len == allowance;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        let line = match std::str::from_utf8(&bytes) {
            Ok(line) => line,
            Err(error) if partial_line && error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix")
            }
            Err(_) => {
                invalid_utf8 = true;
                break;
            }
        };
        observed_lines = observed_lines.saturating_add(1);
        if index >= requested_start_idx {
            retained_bytes += line.len() + 16;
            selected_lines.push(line.to_string());
        }
        if partial_line {
            scan_gap = true;
            if observed_lines as usize >= requested_end_idx {
                break;
            }
            let (consumed, finished) =
                match skip_streamed_line(&mut reader, scan_ceiling.saturating_sub(scanned_bytes)) {
                    Ok(result) => result,
                    Err(error) => {
                        return Response::error(
                            &req.id,
                            "io_error",
                            format!("read: failed to skip line suffix: {error}"),
                        )
                    }
                };
            scanned_bytes += consumed;
            if !finished {
                break;
            }
        }
    }

    if invalid_utf8 {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size as usize,
                "message": format!("Binary file ({} bytes), not valid UTF-8", byte_size),
            }),
        );
    }

    let exact_total_lines = (!has_more_after_range && !scan_gap).then_some(observed_lines);

    if selected_lines.is_empty() && !scan_gap && observed_lines > 0 {
        // The whole file was scanned and the window still selected nothing,
        // so the start lies past the last line. Saying so by name keeps the
        // reader from taking an empty result as "those lines are blank".
        return start_past_end_error(&req.id, u64::from(start_line), observed_lines);
    }

    if selected_lines.is_empty() {
        let mut data = serde_json::json!({
            "content": if scan_gap { format!("shown 0 of at least {observed_lines} examined lines (scan budget before requested range; {scanned_bytes} bytes scanned) · narrow: earlier start_line or smaller file") } else { String::new() },
            "complete": !scan_gap,
            "scan_bytes_examined": scanned_bytes,
            "scan_gap": scan_gap.then_some("line or response scan budget; narrow: earlier start_line, smaller range"),
            "lines_read": 0,
            "start_line": start_line,
            "end_line": start_line,
            "truncated": scan_gap,
            "byte_size": byte_size as usize,
        });
        if let Some(total_lines) = exact_total_lines {
            data.as_object_mut()
                .expect("read response data is an object")
                .insert("total_lines".to_string(), serde_json::json!(total_lines));
        }
        return Response::success(&req.id, data);
    }

    let mut output = String::new();
    let mut output_bytes = 0usize;
    let mut lines_read = 0u32;
    let mut truncated_by_size = false;
    let line_num_width = format!("{}", requested_start_idx + selected_lines.len()).len();

    for (i, line) in selected_lines.iter().enumerate() {
        let line_num = requested_start_idx + i + 1;
        let display_line = if line.len() > MAX_LINE_LENGTH {
            let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
            format!(
                "{:>width$}: {}... (truncated)\n",
                line_num,
                &line[..safe_end],
                width = line_num_width
            )
        } else {
            format!("{:>width$}: {}\n", line_num, line, width = line_num_width)
        };

        output_bytes += display_line.len();
        if output_bytes > MAX_BYTES {
            truncated_by_size = true;
            output.push_str(&format!(
                "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                MAX_BYTES / 1024
            ));
            break;
        }

        output.push_str(&display_line);
        lines_read += 1;
    }

    let actual_end = start_line + lines_read - if lines_read > 0 { 1 } else { 0 };
    let has_more = requested_start_idx > 0 || has_more_after_range;
    let truncated = has_more || truncated_by_size || scan_gap;
    if scan_gap {
        output.push_str(&format!("\nshown {lines_read} of at least {observed_lines} examined lines (line or response scan budget) · narrow: start_line/end_line; {scanned_bytes} bytes scanned\n"));
    }

    let mut data = serde_json::json!({
        "content": output,
        "complete": !truncated,
        "scan_bytes_examined": scanned_bytes,
        "scan_gap": scan_gap.then_some("line or response scan budget"),
        "lines_read": lines_read,
        "start_line": start_line,
        "end_line": actual_end,
        "truncated": truncated,
        "byte_size": byte_size as usize,
    });
    if let Some(total_lines) = exact_total_lines {
        data.as_object_mut()
            .expect("read response data is an object")
            .insert("total_lines".to_string(), serde_json::json!(total_lines));
    }

    Response::success(&req.id, data)
}

/// Handle directory listing.
fn handle_directory(req: &RawRequest, path: &Path) -> Response {
    // This server-owned field lets transparent bash rewrites preserve `ls`
    // visibility. Direct read calls omit it and retain the existing show-all default.
    let include_hidden = req
        .params
        .get("include_hidden")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let mut entries: Vec<String> = Vec::new();

    let read_dir = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read directory: {}", e),
            );
        }
    };

    let mut examined = 0;
    let mut enumeration_cut = false;
    for entry_result in read_dir.take(MAX_DIRECTORY_SCAN + 1) {
        examined += 1;
        if examined > MAX_DIRECTORY_SCAN {
            enumeration_cut = true;
            break;
        }
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if crate::watcher_backend::flush::is_marker_path(&entry.path()) {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

        if is_dir {
            entries.push(format!("{}/", name));
        } else {
            entries.push(name);
        }
    }

    entries.sort();

    // Hosts normalize ranges to snake_case; accept the public aliases too for
    // direct protocol callers. Positions address the sorted, bounded listing.
    let number = |key: &str| {
        req.params.get(key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str()?.parse::<u64>().ok())
        })
    };
    let start = number("start_line")
        .or_else(|| number("startLine"))
        .or_else(|| number("offset"))
        .unwrap_or(1)
        .max(1);
    let requested_limit = number("limit")
        .filter(|value| *value > 0)
        .unwrap_or(MAX_DIRECTORY_ENTRIES as u64);
    let limit = requested_limit.min(MAX_DIRECTORY_ENTRIES as u64) as usize;
    let end = number("end_line").or_else(|| number("endLine"));
    let count = end
        .map(|end| {
            end.saturating_sub(start.saturating_sub(1))
                .min(MAX_DIRECTORY_ENTRIES as u64) as usize
        })
        .unwrap_or(limit);
    let total = entries.len();
    let skip = start.saturating_sub(1).min(total as u64) as usize;
    entries = entries.into_iter().skip(skip).take(count).collect();
    let shown = entries.len();
    let truncated = enumeration_cut || shown < total;
    let envelope =
        crate::list_surfaces::read::build_directory_envelope(shown, total, enumeration_cut);
    let offset_note = (start > total as u64 && !enumeration_cut && start > 1)
        .then(|| format!("offset past end: offset {start} exceeds {total} entries"));
    // Put the offset-past-end note before the trailer so the final line keeps its standard format.
    let mut trailer_text = offset_note.clone().unwrap_or_default();
    if let Some(envelope) = &envelope {
        let trailer = crate::ndjson_text::build_ndjson_text(
            "",
            &serde_json::json!({"entries_list_envelope": envelope}),
            Some("payload.entries"),
            false,
        );
        if !trailer_text.is_empty() {
            trailer_text.push('\n');
        }
        trailer_text.push_str(&trailer);
    }
    if !trailer_text.is_empty() {
        entries.push(format!("\n{trailer_text}"));
    }
    let mut data = serde_json::json!({
        "entries": entries,
        "complete": !truncated,
        "truncated": truncated,
        "total_entries": total,
        "entries_shown": shown,
        "total_entries_exact": !enumeration_cut,
        "entries_examined": examined,
        "enumeration_gap": enumeration_cut.then_some("directory entry budget; narrow: subdirectory"),
    });
    if let Some(note) = offset_note {
        data["offset_note"] = note.into();
    }
    if let Some(envelope) = envelope {
        data["entries_list_envelope"] = serde_json::to_value(envelope).expect("directory envelope");
    }
    Response::success(&req.id, data)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bash_flush_markers_are_not_directory_entries() {
        let root = tempfile::tempdir().unwrap();
        for name in [".aft-flush-123-1", ".aft-flush-not-private", "visible.rs"] {
            std::fs::write(root.path().join(name), "").unwrap();
        }
        let request = serde_json::from_value(
            serde_json::json!({"id":"flush-list", "command":"read", "params":{}}),
        )
        .unwrap();
        let result = super::handle_directory(&request, root.path());
        assert_eq!(
            result.data["entries"],
            serde_json::json!([".aft-flush-not-private", "visible.rs"])
        );
    }
    use super::*;
    use std::path::{Path, PathBuf};

    use image::{DynamicImage, ImageBuffer, Rgba};
    use serde_json::{json, Value};

    use crate::config::Config;
    use crate::context::default_language_provider_factory;

    #[test]
    fn github_read_response_preserves_engine_text_and_attachment_metadata() {
        let _decoder_guard = image_decoder_test_guard();
        let request = RawRequest {
            id: "github-response".to_string(),
            command: "read".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "file": "issue://7" }),
        };
        let response = github_read_response(
            &request,
            GithubReadCompletion {
                content: "canonical text\n".to_string(),
                total_lines: 1,
                freshness: crate::github_read::GithubReadFreshness::Fetched,
                attachments: vec![crate::github_read::GithubImageAttachment {
                    source_url: "https://github.com/user-attachments/files/7/image.png".to_string(),
                    mime: "image/png".to_string(),
                    bytes: vec![137, 80, 78, 71, 13, 10, 26, 10],
                }],
                document: None,
                diff_page: None,
            },
            1,
        );

        assert!(response.success);
        assert_eq!(response.data["content"], "canonical text\n");
        let attachment = first_attachment(&response.data);
        assert_eq!(
            attachment["source_url"],
            "https://github.com/user-attachments/files/7/image.png"
        );
        assert_eq!(attachment["mime"], "image/png");
        assert_eq!(attachment["bytes"], 8);
        assert_eq!(
            decoded_attachment_bytes(attachment),
            vec![137, 80, 78, 71, 13, 10, 26, 10]
        );
    }

    #[test]
    fn github_image_result_budget_attaches_prefix_and_lists_omitted_links() {
        let _decoder_guard = image_decoder_test_guard();
        let request = RawRequest {
            id: "github-budget".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file":"issue://7"}),
        };
        let mut state = 0x12345678u32;
        let image = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_fn(256, 256, |_, _| {
            let mut channel = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u16
            };
            image::Rgba([channel(), channel(), channel(), channel()])
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba16(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let images: Vec<_> = (0..8)
            .map(|index| crate::github_read::GithubImageAttachment {
                source_url: format!("https://github.com/user-attachments/files/{index}/image.png"),
                mime: "image/png".into(),
                bytes: encoded.get_ref().clone(),
            })
            .collect();
        let completion = || GithubReadCompletion {
            content: "canonical text\n".into(),
            total_lines: 1,
            freshness: crate::github_read::GithubReadFreshness::Fetched,
            attachments: images.clone(),
            document: None,
            diff_page: None,
        };
        let legacy = github_read_response(&request, completion(), 1);
        assert!(legacy.success);
        assert_eq!(legacy.data["attachments"].as_array().unwrap().len(), 8);
        assert_eq!(legacy.data["content"], "canonical text\n");
        let _budget = crate::subc::result_body::enter(true);
        let bounded = github_read_response(&request, completion(), 1);
        assert!(bounded.success, "{:?}", bounded.data);
        let attached = bounded.data["attachments"].as_array().unwrap();
        assert_eq!(attached.len(), 2);
        for (index, attachment) in attached.iter().enumerate() {
            assert_eq!(attachment["source_url"], images[index].source_url);
            assert_eq!(attachment["width"], 256);
            assert_eq!(attachment["height"], 256);
            assert_eq!(attachment["encoding"], "png");
            assert_eq!(
                decoded_attachment_bytes(attachment.as_object().unwrap()),
                images[index].bytes
            );
        }
        let expected_links = images[2..]
            .iter()
            .map(|image| image.source_url.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            bounded.data["content"],
            format!("canonical text\n\n\n6 more images not attached: {expected_links}")
        );
        assert_eq!(
            bounded.data["complete"], true,
            "image omission does not truncate page text"
        );
        let image_bytes: usize = attached
            .iter()
            .map(|image| image["data"].as_str().unwrap().len())
            .sum();
        assert!(image_bytes * 2 + 64 * 1024 < crate::subc::result_body::MAX_RESULT_BYTES);
    }

    #[test]
    fn github_image_budget_refuses_when_page_text_itself_cannot_fit() {
        let request = RawRequest {
            id: "github-text-budget".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file":"issue://7"}),
        };
        let completion = || GithubReadCompletion {
            content: "x".repeat(1024 * 1024),
            total_lines: 1,
            freshness: crate::github_read::GithubReadFreshness::Fetched,
            attachments: Vec::new(),
            document: None,
            diff_page: None,
        };
        assert!(github_read_response(&request, completion(), 1).success);
        let _budget = crate::subc::result_body::enter(true);
        let response = github_read_response(&request, completion(), 1);
        assert!(!response.success);
        assert_eq!(response.data["code"], "response_too_large");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .ends_with("; read a page with fewer or smaller images"));
    }

    #[test]
    fn github_image_fitting_keeps_the_reserved_return_window() {
        let request = RawRequest {
            id: "github-return-window".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file":"issue://7"}),
        };
        let completion = GithubReadCompletion {
            content: "page".into(),
            total_lines: 1,
            freshness: crate::github_read::GithubReadFreshness::Fetched,
            attachments: vec![crate::github_read::GithubImageAttachment {
                source_url: "https://github.com/user-attachments/files/7/image.png".into(),
                mime: "image/png".into(),
                bytes: encode_png(&rgba_image(8, 8, false)).unwrap(),
            }],
            document: None,
            diff_page: None,
        };
        let _budget = crate::subc::result_body::enter(true);
        let _io = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(
            Duration::from_millis(1),
        )));
        let response = github_read_response(&request, completion, 1);
        assert!(response.success);
        assert!(response.data["attachments"].as_array().unwrap().is_empty());
        assert_eq!(
            response.data["content"],
            "page\n\n1 image not attached: https://github.com/user-attachments/files/7/image.png"
        );
    }

    #[test]
    fn test_is_binary_detects_null_bytes() {
        assert!(is_binary(&[0x48, 0x65, 0x6c, 0x00, 0x6f]));
        assert!(!is_binary(b"Hello, world!"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_is_binary_checks_first_8kb() {
        let mut data = vec![0x41u8; 16384]; // 16KB of 'A'
        data[10000] = 0; // null byte after 8KB boundary
        assert!(!is_binary(&data)); // should not detect — null is past 8KB

        data[100] = 0; // null byte within 8KB
        assert!(is_binary(&data));
    }

    fn ctx_for(root: &Path) -> AppContext {
        let config = Config {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        };
        AppContext::new(default_language_provider_factory(), config)
    }

    fn request(file: &Path, extra: Value) -> RawRequest {
        let mut params = extra.as_object().cloned().unwrap_or_default();
        params.insert(
            "file".to_string(),
            Value::String(file.to_string_lossy().to_string()),
        );
        RawRequest {
            id: "test".to_string(),
            command: "read".to_string(),
            lsp_hints: None,
            session_id: None,
            params: Value::Object(params),
        }
    }

    fn read_response(root: &Path, file: &Path, extra: Value) -> Response {
        let ctx = ctx_for(root);
        handle_read(&request(file, extra), &ctx)
    }

    fn first_attachment(data: &Value) -> &serde_json::Map<String, Value> {
        data["attachments"]
            .as_array()
            .and_then(|attachments| attachments.first())
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("first attachment object: {data}"))
    }

    /// Image decoding admits at most `MAX_IMAGE_DECODERS` process-wide. Tests
    /// that decode images run serially so a parallel test holding the slots
    /// cannot turn an expected attachment into a capacity omission.
    fn image_decoder_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn noisy_fit_fixture(transparent: bool) -> Vec<u8> {
        let mut state = 0x12345678u32;
        let image = image::RgbaImage::from_fn(1024, 1024, |_, _| {
            let mut channel = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            image::Rgba([
                channel(),
                channel(),
                channel(),
                if transparent { channel() % 255 } else { 255 },
            ])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn tool_result_noisy_image_already_at_1024_shrinks_to_fit() {
        let _decoder_guard = image_decoder_test_guard();
        let bytes = noisy_fit_fixture(false);
        assert!(base64_len(bytes.len()).unwrap() > image_base64_budget());
        let legacy = process_image_with_budget_timeout(
            bytes.clone(),
            ImageKind::Png,
            None,
            Instant::now() + IMAGE_PROCESS_TIMEOUT,
        )
        .unwrap();
        assert_eq!(legacy.bytes, bytes);
        assert_eq!(legacy.width, 1024);
        assert!(!legacy.resized);
        let _budget = crate::subc::result_body::enter(true);
        let image = process_image_with_timeout(bytes, ImageKind::Png).unwrap();
        assert!(image.width < 1024 && image.width >= MIN_FITTED_IMAGE_DIMENSION);
        assert_eq!(image.height, image.width);
        assert!(image.resized);
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(
            image::guess_format(&image.bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert_eq!(
            image::load_from_memory(&image.bytes).unwrap().dimensions(),
            (image.width, image.height)
        );
        assert!(base64_len(image.bytes.len()).unwrap() <= image_base64_budget());
        let response = image_attachment_response("fitted", image);
        assert!(response.success);
        assert_eq!(first_attachment(&response.data)["encoding"], "jpeg");
    }

    #[test]
    fn tool_result_transparent_image_is_never_lossy_encoded() {
        let _decoder_guard = image_decoder_test_guard();
        let image = process_image_with_budget_timeout(
            noisy_fit_fixture(true),
            ImageKind::Png,
            Some(512 * 1024),
            Instant::now() + IMAGE_PROCESS_TIMEOUT,
        )
        .unwrap();
        assert_eq!(image.mime, "image/png");
        assert!(image.resized);
        assert!(image.width < 1024);
        let decoded = image::load_from_memory(&image.bytes).unwrap();
        assert_eq!(decoded.dimensions(), (image.width, image.height));
        assert!(decoded.color().has_alpha());
        assert!(decoded.to_rgba8().pixels().any(|pixel| pixel.0[3] < 255));
        assert!(base64_len(image.bytes.len()).unwrap() <= 512 * 1024);
        let almost_opaque = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([0, 0, 0, 65534]),
        ));
        assert!(
            !image_is_opaque(&almost_opaque),
            "16-bit alpha must not be rounded to opaque"
        );
    }

    #[test]
    fn tool_result_image_floor_that_still_exceeds_budget_refuses() {
        let _decoder_guard = image_decoder_test_guard();
        // A private one-byte budget makes the otherwise unreachable floor refusal
        // testable without changing the provider's real three-MiB contract.
        let image = process_image_with_budget_timeout(
            noisy_fit_fixture(true),
            ImageKind::Png,
            Some(1),
            Instant::now() + IMAGE_PROCESS_TIMEOUT,
        )
        .unwrap();
        assert_eq!(image.width.max(image.height), 256);
        assert_eq!(image.mime, "image/png");
        let response = image_attachment_response_with_budget("floor", image, Some(1));
        assert!(!response.success);
        assert_eq!(response.data["code"], "response_too_large");
        let text = response.data["message"].as_str().unwrap();
        assert!(text.starts_with("reply too large to send ("));
        assert!(text.ends_with(" MiB, limit 3 MiB); read a smaller image"));
    }

    fn decoded_attachment_bytes(attachment: &serde_json::Map<String, Value>) -> Vec<u8> {
        BASE64
            .decode(attachment["data"].as_str().expect("attachment data"))
            .expect("valid base64 attachment")
    }

    fn rgba_image(width: u32, height: u32, noisy: bool) -> DynamicImage {
        let mut state = 0x1234_5678u32;
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            if noisy {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let r = (state >> 24) as u8;
                let g = (state >> 16) as u8;
                let b = (state >> 8) as u8;
                Rgba([r, g, b, 255])
            } else {
                Rgba([(x % 251) as u8, (y % 251) as u8, ((x + y) % 251) as u8, 255])
            }
        });
        DynamicImage::ImageRgba8(image)
    }

    fn write_fixture(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, bytes).expect("write fixture");
        path
    }

    #[test]
    fn media_sniff_supports_images_and_passthrough_for_small_files() {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let small = rgba_image(32, 16, false);
        let fixtures = [
            (
                "small.png",
                ImageKind::Png.mime(),
                encode_png(&small).unwrap(),
            ),
            (
                "small.jpg",
                ImageKind::Jpeg.mime(),
                encode_jpeg(&small).unwrap(),
            ),
            (
                "small.gif",
                ImageKind::Gif.mime(),
                encode_gif(&small).unwrap(),
            ),
            (
                "small.webp",
                ImageKind::WebP.mime(),
                encode_webp_lossless(&small).unwrap(),
            ),
        ];

        for (name, mime, bytes) in fixtures {
            let path = write_fixture(temp.path(), name, &bytes);
            let response = read_response(temp.path(), &path, json!({}));
            assert!(response.success, "{name} should read successfully");
            let attachment = first_attachment(&response.data);
            assert_eq!(attachment["kind"], "image");
            assert_eq!(attachment["mime"], mime);
            assert_eq!(attachment["width"], 32);
            assert_eq!(attachment["height"], 16);
            assert_eq!(attachment["resized"], false);
            assert_eq!(attachment["animation"], "none");
            assert_eq!(attachment["orientation_applied"], false);
            assert_eq!(attachment["bytes"], bytes.len());
            assert_eq!(decoded_attachment_bytes(attachment), bytes);
        }
    }

    #[test]
    fn media_sniff_happens_before_explicit_range_and_resizes_large_images() {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let large = rgba_image(2048, 512, false);
        let bytes = encode_png(&large).unwrap();
        let path = write_fixture(temp.path(), "large.png", &bytes);

        let response = read_response(temp.path(), &path, json!({ "start_line": 1 }));

        assert!(response.success);
        let attachment = first_attachment(&response.data);
        assert_eq!(attachment["kind"], "image");
        assert_eq!(attachment["mime"], ImageKind::Png.mime());
        assert_eq!(attachment["width"], 1024);
        assert_eq!(attachment["height"], 256);
        assert_eq!(attachment["resized"], true);
        assert_eq!(attachment["source_mime"], ImageKind::Png.mime());
        assert!(response.data.get("binary").is_none());
    }

    #[test]
    fn resized_webp_reencodes_with_source_mime() {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let large = rgba_image(2048, 512, false);
        let bytes = encode_webp_lossless(&large).unwrap();
        let path = write_fixture(temp.path(), "large.webp", &bytes);

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success);
        let attachment = first_attachment(&response.data);
        assert_eq!(attachment["kind"], "image");
        assert!(matches!(
            attachment["mime"].as_str(),
            Some("image/webp" | "image/png" | "image/jpeg")
        ));
        assert_eq!(attachment["resized"], true);
        assert_eq!(attachment["source_mime"], ImageKind::WebP.mime());
        assert_eq!(attachment["width"], 1024);
        assert_ne!(decoded_attachment_bytes(attachment), bytes);
    }

    #[test]
    fn corrupt_images_are_omitted_with_reason() {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(temp.path(), "corrupt.png", b"\x89PNG\r\n\x1a\nnot a png");

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success);
        assert_eq!(response.data["attachments"].as_array().unwrap().len(), 0);
        assert!(response.data["attachment_omitted_reason"]
            .as_str()
            .unwrap()
            .contains("image decode failed"));
        assert!(response.data["content"]
            .as_str()
            .unwrap()
            .contains("Attachment omitted"));
    }

    #[test]
    fn pdf_returns_raw_attachment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bytes = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n%%EOF\n";
        let path = write_fixture(temp.path(), "doc.pdf", bytes);

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success);
        let attachment = first_attachment(&response.data);
        assert_eq!(attachment["kind"], "pdf");
        assert_eq!(attachment["mime"], "application/pdf");
        assert_eq!(attachment["bytes"], bytes.len());
        assert_eq!(decoded_attachment_bytes(attachment), bytes);
    }

    #[test]
    fn non_media_binary_and_utf8_svg_keep_existing_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let zip_path = write_fixture(temp.path(), "archive.zip", b"PK\x03\x04\x00\x00\x00\x00");
        let zip_response = read_response(temp.path(), &zip_path, json!({}));
        assert!(zip_response.success);
        assert_eq!(zip_response.data["binary"], true);
        assert!(zip_response.data.get("attachments").is_none());

        let svg_path = write_fixture(
            temp.path(),
            "vector.svg",
            br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10"/></svg>"#,
        );
        let svg_response = read_response(temp.path(), &svg_path, json!({}));
        assert!(svg_response.success);
        assert!(svg_response.data["content"]
            .as_str()
            .unwrap()
            .starts_with("1: <svg"));
        assert!(svg_response.data.get("attachments").is_none());
        assert!(svg_response.data.get("binary").is_none());
    }

    #[test]
    fn oversized_image_after_resize_is_omitted_with_reason() {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let noisy = rgba_image(1536, 1536, true);
        let bytes = encode_png(&noisy).unwrap();
        let path = write_fixture(temp.path(), "noisy.png", &bytes);

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success, "{response:?}");
        assert_eq!(response.data["attachments"].as_array().unwrap().len(), 0);
        assert!(response.data["attachment_omitted_reason"]
            .as_str()
            .unwrap()
            .contains("too large to inline"));
    }

    fn image_read_beyond_filesystem_deadline(hashline: bool) {
        let _decoder_guard = image_decoder_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let bytes = encode_png(&rgba_image(2, 2, false)).unwrap();
        let path = write_fixture(temp.path(), "small.png", &bytes);
        let ctx = ctx_for(temp.path());
        let req = request(&path, json!({}));
        if hashline {
            assert!(
                ctx.hashline_bindings()
                    .register(
                        temp.path(),
                        req.session().to_string(),
                        crate::hashline::integration::RegistrationRequest {
                            configured_enabled: true,
                            edit_slot_survives: true,
                            read_slot_survives: true,
                        },
                    )
                    .effective
            );
        }
        crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.set(0));
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(1));
        // Simulate slow decoding without adding CPU load. Filesystem access must
        // finish within its budget, but image processing has a separate timeout.
        let response = crate::bounded_io::with_budget(Some(budget), || {
            let previous =
                IMAGE_PROCESSING_DELAY.with(|delay| delay.replace(Duration::from_millis(1100)));
            let response = handle_read(&req, &ctx);
            IMAGE_PROCESSING_DELAY.with(|delay| delay.set(previous));
            response
        });

        assert!(response.success, "{response:?}");
        let attachment = first_attachment(&response.data);
        assert_eq!(attachment["kind"], "image");
        assert_eq!(decoded_attachment_bytes(attachment), bytes);
        if hashline {
            assert!(response.data["hashline_tag_unavailable_reason"]
                .as_str()
                .unwrap()
                .contains("binary"));
            assert_eq!(
                crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.get()),
                1
            );
        }
    }

    #[test]
    fn image_processing_does_not_consume_filesystem_deadline() {
        image_read_beyond_filesystem_deadline(false);
    }

    #[test]
    fn registered_image_processing_does_not_consume_filesystem_deadline() {
        image_read_beyond_filesystem_deadline(true);
    }

    /// Render a read response through the agent-facing text formatter, the same
    /// path the plugins use. `agent_specified_range` mirrors whether the caller
    /// passed a range parameter (startLine/endLine or offset/limit).
    fn render_read(response: &Response, agent_specified_range: bool) -> String {
        crate::subc_format::format_response("read", response, agent_specified_range)
    }

    /// Build a text fixture whose rendered line-numbered output exceeds
    /// `MAX_BYTES` (50KB) so a whole-file read truncates.
    fn oversized_text_fixture(root: &Path, name: &str) -> PathBuf {
        let line = "x".repeat(80);
        let mut content = String::new();
        // ~2000 lines * ~85 bytes/line ≈ 170KB, well past the 50KB cap.
        for i in 1..=2000 {
            content.push_str(&format!("line-{i}-{line}\n"));
        }
        write_fixture(root, name, content.as_bytes())
    }

    #[test]
    fn truncated_read_carries_footer_and_structured_truncation_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = oversized_text_fixture(temp.path(), "big.txt");

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success);
        // Expose truncation in the structured response fields so callers can
        // tell when content was omitted, not just from the rendered footer.
        assert_eq!(response.data["truncated"], json!(true));
        assert!(response.data["total_lines"].as_u64().unwrap() >= 2000);
        assert!(response.data["lines_read"].as_u64().unwrap() < 2000);

        let text = render_read(&response, false);
        assert!(
            text.contains("Use startLine/endLine or offset/limit to read other sections."),
            "truncated read should carry the narrowing footer, got: {text}"
        );
        assert!(
            text.contains("Showing lines"),
            "footer should state the range: {text}"
        );
    }

    #[test]
    fn whole_large_read_carries_soft_note_but_ranged_read_of_same_file_does_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Build a file that is large (>20KB soft threshold) yet still fits
        // under the 50KB truncation cap when read whole.
        let line = "y".repeat(40);
        let mut content = String::new();
        // ~600 lines * ~45 bytes ≈ 27KB: over the 20KB soft threshold, under
        // the 50KB truncation cap.
        for i in 1..=600 {
            content.push_str(&format!("line-{i}-{line}\n"));
        }
        let path = write_fixture(temp.path(), "medium.txt", content.as_bytes());

        // Whole read: not truncated, but large enough for the soft note.
        let whole = read_response(temp.path(), &path, json!({}));
        assert!(whole.success);
        assert_eq!(whole.data["truncated"], json!(false));
        let whole_text = render_read(&whole, false);
        assert!(
            whole_text.contains(
                "File is large; use startLine/endLine or offset/limit to read a section."
            ),
            "whole large read should carry the soft note, got: {whole_text}"
        );

        // Ranged read of the same file: no soft note, no truncation footer.
        let ranged = read_response(
            temp.path(),
            &path,
            json!({ "start_line": 1, "end_line": 10 }),
        );
        assert!(ranged.success);
        let ranged_text = render_read(&ranged, true);
        assert!(
            !ranged_text.contains("File is large"),
            "ranged read must not carry the soft note, got: {ranged_text}"
        );
        assert!(
            !ranged_text.contains("Use startLine/endLine"),
            "ranged read must not carry the truncation footer, got: {ranged_text}"
        );
    }

    #[test]
    fn small_read_carries_no_footer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(temp.path(), "small.txt", b"hello\nworld\n");

        let response = read_response(temp.path(), &path, json!({}));
        assert!(response.success);
        assert_eq!(response.data["truncated"], json!(false));

        let text = render_read(&response, false);
        assert!(
            !text.contains("File is large") && !text.contains("Use startLine/endLine"),
            "small read should carry no footer, got: {text}"
        );
    }

    #[test]
    fn ranged_read_of_oversized_fixture_carries_no_footer() {
        // Negative control: the truncation footer must NOT appear when the
        // caller already picked a range on the same oversized fixture.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = oversized_text_fixture(temp.path(), "big.txt");

        let response = read_response(
            temp.path(),
            &path,
            json!({ "start_line": 1, "end_line": 5 }),
        );
        assert!(response.success);

        let text = render_read(&response, true);
        assert!(
            !text.contains("Use startLine/endLine"),
            "ranged read of oversized fixture must not carry the footer, got: {text}"
        );
        assert!(
            !text.contains("File is large"),
            "ranged read must not carry the soft note, got: {text}"
        );
    }
}

#[cfg(test)]
mod audit_regressions {
    use super::*;
    use crate::hashline::snapshot::ReadSelection;

    #[test]
    fn zero_read_parameters_select_the_physical_read_window() {
        let response = Response::success("selection", serde_json::json!({"total_lines": 2}));
        let request = serde_json::from_value(
            serde_json::json!({"id": "selection", "command": "read", "start_line": 0, "limit": 2}),
        )
        .unwrap();
        assert!(matches!(
            read_selection(&request, &response),
            ReadSelection::Range { start: 1, end: 2 }
        ));
        let request = serde_json::from_value(
            serde_json::json!({"id": "selection", "command": "read", "limit": 0}),
        )
        .unwrap();
        assert!(
            matches!(read_selection(&request, &response), ReadSelection::Head(lines) if lines == DEFAULT_LIMIT as usize)
        );
    }

    #[test]
    fn oversized_read_parameters_do_not_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        for field in ["start_line", "end_line", "limit"] {
            let mut value = serde_json::json!({"id": "oversized", "command": "read"});
            value[field] = serde_json::json!(u64::from(u32::MAX) + 2);
            let request = serde_json::from_value(value).unwrap();
            let response = handle_text_or_binary_read(&request, &path, 13, false, &mut None);
            assert!(
                !response.success,
                "{field} must be rejected: {:?}",
                response.data
            );
        }
    }

    #[test]
    fn zero_directory_limit_uses_default_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("entry.txt"), "text").unwrap();
        let request = serde_json::from_value(
            serde_json::json!({"id": "directory", "command": "read", "limit": 0}),
        )
        .unwrap();
        let response = handle_directory(&request, dir.path());
        assert!(response.success);
        assert_eq!(response.data["entries"][0], "entry.txt");
    }
}

#[cfg(test)]
mod hashline_wiring_tests {
    use super::*;
    use serde_json::json;

    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};
    use crate::hashline::integration::RegistrationRequest;
    use crate::protocol::{RawRequest, DEFAULT_SESSION_ID};

    fn context(root: &Path) -> AppContext {
        AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Default::default()
            },
        )
    }

    fn request_for(path: &Path) -> RawRequest {
        RawRequest {
            id: "hashline-read-test".to_string(),
            command: "read".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "file": path.to_string_lossy() }),
        }
    }

    fn registered_context(root: &Path) -> AppContext {
        let ctx = context(root);
        assert!(
            ctx.hashline_bindings()
                .register(
                    root,
                    DEFAULT_SESSION_ID.to_string(),
                    RegistrationRequest {
                        configured_enabled: true,
                        edit_slot_survives: true,
                        read_slot_survives: true,
                    }
                )
                .effective
        );
        ctx
    }

    fn snapshots(
        ctx: &AppContext,
        root: &Path,
    ) -> Vec<(std::path::PathBuf, crate::hashline::snapshot::Snapshot)> {
        ctx.hashline_bindings()
            .capture(root.to_path_buf(), DEFAULT_SESSION_ID.to_string())
            .unwrap()
            .with_binding(|binding| {
                let mut snapshots: Vec<_> = binding
                    .snapshots()
                    .iter()
                    .map(|(path, snapshot)| (path.to_path_buf(), snapshot.clone()))
                    .collect();
                snapshots.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.tag.cmp(&b.1.tag)));
                snapshots
            })
    }

    #[test]
    fn hashline_whole_read_uses_one_source_and_matches_legacy_responses_and_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let fixtures = [
            ("normal", b"alpha\nbeta\n".to_vec(), false),
            ("crlf", b"alpha\r\nbeta\r\nlast".to_vec(), false),
            ("empty", Vec::new(), false),
            ("readonly", b"readonly\r\n".to_vec(), true),
            ("binary", b"a\0b".to_vec(), false),
            ("invalid_utf8", vec![0xff, 0xfe], false),
            (
                "large",
                "line with realistic log output\n"
                    .repeat(20000)
                    .into_bytes(),
                false,
            ),
            (
                "wide",
                format!("{}\r\nlast\n", "🦀".repeat(1000)).into_bytes(),
                false,
            ),
        ];
        for (name, bytes, readonly) in fixtures {
            let path = root.join(name);
            std::fs::write(&path, &bytes).unwrap();
            let permissions = std::fs::metadata(&path).unwrap().permissions();
            if readonly {
                let mut changed = permissions.clone();
                changed.set_readonly(true);
                std::fs::set_permissions(&path, changed).unwrap();
            }
            let legacy_ctx = registered_context(&root);
            let actual_ctx = registered_context(&root);
            let req = request_for(&path);
            LEGACY_HASHLINE_READ.with(|legacy| legacy.set(true));
            crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.set(0));
            LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.set(0));
            let reference = handle_read(&req, &legacy_ctx);
            let before = crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.get());
            let rendered_before = LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.get());
            LEGACY_HASHLINE_READ.with(|legacy| legacy.set(false));
            crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.set(0));
            LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.set(0));
            let actual = handle_read(&req, &actual_ctx);
            let after = crate::hashline::snapshot::SOURCE_READS.with(|reads| reads.get());
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&reference).unwrap(),
                "{name}"
            );
            assert_eq!(
                snapshots(&actual_ctx, &root),
                snapshots(&legacy_ctx, &root),
                "{name}"
            );
            assert_eq!(after, 1, "{name}: reuse the legacy read's source");
            assert_eq!(
                LEGACY_READ_RENDER_BYTES.with(|bytes| bytes.get()),
                0,
                "{name}: no discarded legacy rendering"
            );
            eprintln!("hashline {name}: source reads {before} -> {after}, discarded render bytes {rendered_before} -> 0");
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        // Refused paths must remain store-neutral and keep their error response.
        let path = root.join("missing");
        let legacy_ctx = registered_context(&root);
        let actual_ctx = registered_context(&root);
        let req = request_for(&path);
        LEGACY_HASHLINE_READ.with(|legacy| legacy.set(true));
        let reference = handle_read(&req, &legacy_ctx);
        LEGACY_HASHLINE_READ.with(|legacy| legacy.set(false));
        let actual = handle_read(&req, &actual_ctx);
        assert!(!actual.success);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&reference).unwrap()
        );
        assert!(snapshots(&actual_ctx, &root).is_empty());
    }

    #[test]
    fn unregistered_read_is_byte_identical_to_legacy_rendering() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "alpha\nbeta\n").expect("fixture write");
        let ctx = context(temp.path());
        let req = request_for(&path);

        let legacy = handle_read_legacy(&req, &ctx);
        let wrapped = handle_read(&req, &ctx);
        assert_eq!(wrapped.success, legacy.success);
        assert_eq!(wrapped.data, legacy.data);
    }

    #[test]
    fn effective_hashline_read_mints_snapshot_and_preserves_requested_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let canonical_root = std::fs::canonicalize(temp.path()).expect("canonical root");
        let path = canonical_root.join("sample.txt");
        std::fs::write(&path, "alpha\nbeta\n").expect("fixture write");
        let ctx = context(&canonical_root);
        let registration = ctx.hashline_bindings().register(
            &canonical_root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        assert!(registration.effective);

        let mut req = request_for(&path);
        req.params["_hashline_requested_path"] = Value::String("sample.txt".to_string());
        let response = handle_read(&req, &ctx);
        assert!(response.success);
        assert_eq!(response.data["requested_path"], "sample.txt");
        let content = response.data["content"].as_str().expect("tagged content");
        assert!(content.starts_with("[sample.txt#"), "{content}");
        assert!(content.contains("1:alpha\n2:beta\n"), "{content}");
        assert!(response.data["hashline_tag"]
            .as_str()
            .is_some_and(|tag| tag.len() == 4));
    }
}

#[cfg(test)]
mod github_deferred_deadline_tests {
    use super::*;
    use crate::db::github_read_cache::{GithubReadCacheEntry, GithubReadCacheKey};
    use crate::github_read::{
        DownloadedGithubImage, GithubDocument, GithubFetchRequest, GithubFetcher,
        GithubImageDownloader, GithubReadCacheStore, GithubReadClock, GithubReadError,
        GithubResourceKind,
    };

    /// A cache that always holds a copy of the requested resource.
    struct CachedCopy;

    impl GithubReadCacheStore for CachedCopy {
        fn lookup(
            &self,
            _key: &GithubReadCacheKey,
        ) -> Result<Option<GithubReadCacheEntry>, String> {
            Ok(Some(GithubReadCacheEntry {
                canonical_text: "# Cached issue\n".to_string(),
                fetched_at_ms: 1_234,
                updated_at_ms: 1_234,
            }))
        }

        fn upsert(&self, _key: &GithubReadCacheKey, _text: &str, _at: i64) -> Result<(), String> {
            Ok(())
        }

        fn invalidate(
            &self,
            _kind: GithubResourceKind,
            _repository: &str,
            _number: u64,
            _identity: Option<&str>,
        ) -> Result<usize, String> {
            Ok(0)
        }
    }

    /// A live fetch that stands in for a `gh` that never answers: it runs
    /// until the test lets it finish.
    struct HangingFetch(Mutex<std::sync::mpsc::Receiver<()>>);

    impl GithubFetcher for HangingFetch {
        fn fetch(&self, _request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
            let _ = self
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .recv();
            Err(GithubReadError::FetchFailed(
                "released by the test".to_string(),
            ))
        }
    }

    struct NoImages;

    impl GithubImageDownloader for NoImages {
        fn download(
            &self,
            _url: &url::Url,
            _maximum_bytes: usize,
        ) -> Result<Option<DownloadedGithubImage>, String> {
            Ok(None)
        }
    }

    struct FixedClock;

    impl GithubReadClock for FixedClock {
        fn now_ms(&self) -> i64 {
            2_000
        }
    }

    #[test]
    fn native_read_with_a_hanging_gh_answers_from_the_cache_within_the_call_budget() {
        let storage = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.path().to_path_buf()),
                github: crate::config::GithubConfig {
                    read: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let (release, hold) = std::sync::mpsc::channel();
        GITHUB_READ_ENGINES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                ctx.storage_dir().join("aft.db"),
                Arc::new(GithubReadEngine::new(
                    Arc::new(CachedCopy),
                    Arc::new(HangingFetch(Mutex::new(hold))),
                    Arc::new(NoImages),
                    Arc::new(FixedClock),
                )),
            );
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "native-read",
            "command": "read",
            "file": "issue://owner/repo/1",
        }))
        .unwrap();

        // A 1.5 s call budget: the wait ends one reply margin earlier.
        let budget = Duration::from_millis(1_500);
        let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
        let started = std::time::Instant::now();
        // As in the daemon, the result channel has a completion wake, so the
        // only timer is the one the read sets for its own deadline.
        let completion_wake = crate::response_finalize::DeferredResponseWake::default();
        let outcome = {
            let _scope = completion_wake.install();
            build_read_outcome(req, &ctx)
        };
        let DispatchOutcome::Deferred(mut pending) = outcome else {
            panic!("a live read with a running fetch is deferred");
        };
        let wake = pending
            .poll_interval()
            .expect("the deferred read has a timer for its deadline");
        // The deadline is one reply margin before the end of the budget.
        assert!(
            wake <= Duration::from_millis(500) && wake > Duration::from_millis(300),
            "the timer wakes at the deadline: {wake:?}"
        );
        assert!((pending.poll)(&ctx).is_none(), "nothing to answer yet");

        // Poll on the read's own timer, as the transport does.
        let response = loop {
            std::thread::sleep(wake);
            if let Some(response) = (pending.poll)(&ctx) {
                break response;
            }
            assert!(
                started.elapsed() < budget * 2,
                "the deferred read never gave its late answer"
            );
        };
        let elapsed = started.elapsed();
        let _ = release.send(());

        assert!(response.success, "{:?}", response.data);
        let content = response.data["content"].as_str().unwrap();
        assert!(
            content.starts_with("[cached copy from 1970-01-01T00:00:01.234Z; live fetch failed: GitHub did not answer within 1s;"),
            "{content}"
        );
        assert!(content.ends_with("# Cached issue\n"), "{content}");
        assert!(
            elapsed < budget,
            "the answer came {elapsed:?} into a {budget:?} call"
        );
    }
}
