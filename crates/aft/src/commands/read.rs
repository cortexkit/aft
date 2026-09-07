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
use std::time::Duration;

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
use crate::response_finalize::{DispatchOutcome, PendingResponse, PendingResponsePoll};

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024; // 50KB output cap
const MAX_FILE_READ_BYTES: u64 = 50 * 1024 * 1024; // 50MB input guard
const MAX_DIRECTORY_ENTRIES: usize = 1000;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;
const MEDIA_MAGIC_BYTES: usize = 16;
const MAX_INLINE_BASE64_BYTES: usize = 9 * 1024 * 1024 / 2; // 4.5 MiB encoded payload cap
const MAX_INLINE_IMAGE_DIMENSION: u32 = 1024;
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
    let mut file = fs::File::open(path)?;
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

fn handle_media_read(
    req: &RawRequest,
    path: &Path,
    byte_size: u64,
    media: SniffedMedia,
) -> Response {
    if byte_size > MAX_FILE_READ_BYTES {
        return media_file_too_large_response(&req.id, byte_size);
    }

    let raw_bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };
    let byte_size = raw_bytes.len();

    match media {
        SniffedMedia::Pdf => handle_pdf_media(&req.id, raw_bytes),
        SniffedMedia::Image(kind) => match process_image_with_timeout(raw_bytes, kind) {
            Ok(image) => image_attachment_response(&req.id, image),
            Err(reason) => media_omitted_response(&req.id, byte_size, reason),
        },
    }
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
    let byte_size = image.bytes.len();
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

fn process_image_with_timeout(
    raw_bytes: Vec<u8>,
    kind: ImageKind,
) -> Result<ProcessedImage, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    // One dedicated thread per request instead of a shared bounded pool: the
    // timeout below must measure image processing itself. With a shared pool,
    // queue wait counted against the budget, so unrelated parallel reads could
    // time out an image that never started processing.
    let spawned = std::thread::Builder::new()
        .name("aft-read-media".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(process_image(raw_bytes, kind));
        });
    if let Err(error) = spawned {
        return Err(format!("failed to start image processing: {error}"));
    }

    match rx.recv_timeout(IMAGE_PROCESS_TIMEOUT) {
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
        .map(|value| value as u32)
        .unwrap_or(DEFAULT_LIMIT);
    let start_line = req
        .params
        .get("start_line")
        .and_then(|value| value.as_u64())
        .map(|value| value.max(1) as u32)
        .unwrap_or(1);
    let end_line = req
        .params
        .get("end_line")
        .and_then(|value| value.as_u64())
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

fn github_read_engine(ctx: &AppContext) -> Arc<GithubReadEngine> {
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
    let gh_read = ctx.config().gh_read.clone();
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

pub(crate) fn wait_for_github_read(
    start: GithubReadStart,
) -> Result<GithubReadCompletion, crate::github_read::GithubReadError> {
    match start {
        GithubReadStart::Immediate(completion) => Ok(completion),
        GithubReadStart::Deferred(pending) => loop {
            if let Some(completion) = pending.try_complete() {
                return completion;
            }
            std::thread::sleep(Duration::from_millis(1));
        },
    }
}

fn github_read_response(
    req: &RawRequest,
    completion: GithubReadCompletion,
    start_line: usize,
) -> Response {
    let lines_read = completion.content.lines().count();
    let actual_end = start_line.saturating_add(lines_read.saturating_sub(1));
    let truncated = lines_read > 0 && (start_line > 1 || actual_end < completion.total_lines);
    let attachments = completion
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
        .collect::<Vec<_>>();
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
    let mut poll: PendingResponsePoll = Box::new(move |_| {
        pending.try_complete().map(|completion| match completion {
            Ok(completion) => github_read_response(&req, completion, start_line),
            Err(error) => Response::error(&req.id, error.code(), error.to_string()),
        })
    });
    if let Some(response) = poll(ctx) {
        return DispatchOutcome::Immediate(response);
    }

    DispatchOutcome::Deferred(PendingResponse {
        request_id,
        session_id,
        attach_command: "read".to_string(),
        poll,
        cancellation: None,
        on_shutdown: None,
    })
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
    if let Some(file) = req.params.get("file").and_then(Value::as_str) {
        if is_github_read_target(file) {
            return handle_github_read(req, ctx, file);
        }
    }

    let mut response = handle_read_legacy(req, ctx);
    if !response.success
        || response
            .data
            .get("content")
            .and_then(Value::as_str)
            .is_none()
    {
        return response;
    }

    let binding_root = ctx
        .canonical_cache_root_opt()
        .or_else(|| ctx.config().project_root.clone());
    let Some(binding_root) = binding_root else {
        return response;
    };
    let Some(guard) = ctx
        .hashline_bindings()
        .capture(binding_root, req.session().to_string())
    else {
        return response;
    };
    if !guard.effective() {
        return response;
    }

    let Some(file) = req.params.get("file").and_then(Value::as_str) else {
        return response;
    };
    let path = match ctx.validate_read_path(&req.id, req.session(), Path::new(file)) {
        Ok(path) => path,
        Err(_) => return response,
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
        return response;
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

    let start = req
        .params
        .get("start_line")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    let explicit_end = req.params.get("end_line").and_then(Value::as_u64);
    if req.params.get("start_line").is_some() || explicit_end.is_some() {
        let limit = req
            .params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64) as usize;
        let end = explicit_end
            .map(|value| value as usize)
            .unwrap_or_else(|| start.saturating_add(limit).saturating_sub(1));
        return crate::hashline::snapshot::ReadSelection::Range { start, end };
    }

    if response.data.get("total_lines").and_then(Value::as_u64) == Some(0) {
        crate::hashline::snapshot::ReadSelection::WholeFile
    } else {
        crate::hashline::snapshot::ReadSelection::Head(
            req.params
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIMIT as u64) as usize,
        )
    }
}

fn handle_read_legacy(req: &RawRequest, ctx: &AppContext) -> Response {
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

    // Check existence
    if !path.exists() {
        return Response::error(
            &req.id,
            "not_found",
            format!("read: file not found: {}", file),
        );
    }

    // Directory listing
    if path.is_dir() {
        return handle_directory(req, path.as_path());
    }

    let metadata = match fs::metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to stat file: {}", e),
            );
        }
    };

    let magic = match read_magic(path.as_path()) {
        Ok(magic) => magic,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file header: {}", e),
            );
        }
    };
    if let Some(media) = sniff_media(&magic) {
        return handle_media_read(req, path.as_path(), metadata.len(), media);
    }

    // Parse range parameters
    let limit = req
        .params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_LIMIT);

    let start_line = req
        .params
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v.max(1) as u32)
        .unwrap_or(1);

    let explicit_end_line = req.params.get("end_line").and_then(|v| v.as_u64());
    let has_explicit_range = req.params.get("start_line").is_some() || explicit_end_line.is_some();

    if has_explicit_range {
        return handle_streaming_range_read(
            req,
            path.as_path(),
            metadata.len(),
            start_line,
            explicit_end_line,
            limit,
        );
    }

    if metadata.len() > MAX_FILE_READ_BYTES {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "read: file is too large to load at once ({} bytes > {} bytes). Use start_line/end_line to read sections.",
                metadata.len(),
                MAX_FILE_READ_BYTES
            ),
        );
    }

    // Read raw bytes for binary detection
    let raw_bytes = match fs::read(path.as_path()) {
        Ok(b) => b,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    let byte_size = raw_bytes.len();

    // Binary detection
    if is_binary(&raw_bytes) {
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
    let content = match String::from_utf8(raw_bytes) {
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

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len() as u32;

    let end_line = req
        .params
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or_else(|| {
            start_line
                .saturating_add(limit)
                .saturating_sub(1)
                .min(total_lines)
        });

    // Clamp to actual line count. `.max(start_idx)` guards against agents
    // sending inverted ranges (e.g. end_line < start_line) which would
    // otherwise panic at `lines[start_idx..end_idx]` below. With this guard,
    // inverted ranges yield an empty slice and return zero lines.
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
        let display_line = if line.len() > MAX_LINE_LENGTH {
            // Find a safe UTF-8 boundary at or before MAX_LINE_LENGTH to avoid
            // panicking on multi-byte characters (e.g. emoji, CJK).
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
            // Add truncation notice
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

fn handle_streaming_range_read(
    req: &RawRequest,
    path: &Path,
    byte_size: u64,
    start_line: u32,
    explicit_end_line: Option<u64>,
    limit: u32,
) -> Response {
    let mut file = match fs::File::open(path) {
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
    let reader = std::io::BufReader::new(file);

    for (index, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(line) => line,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                invalid_utf8 = true;
                break;
            }
            Err(e) => {
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("read: failed to read file: {}", e),
                );
            }
        };

        observed_lines = observed_lines.saturating_add(1);
        if index >= requested_start_idx && index < requested_end_idx {
            selected_lines.push(line);
        }
        if index >= requested_end_idx {
            has_more_after_range = true;
            break;
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

    let exact_total_lines = (!has_more_after_range).then_some(observed_lines);

    if selected_lines.is_empty() {
        let mut data = serde_json::json!({
            "content": "",
            "complete": true,
            "lines_read": 0,
            "start_line": start_line,
            "end_line": start_line,
            "truncated": false,
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
    let truncated = has_more || truncated_by_size;

    let mut data = serde_json::json!({
        "content": output,
        "complete": !truncated,
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

    for entry_result in read_dir {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

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

    let total = entries.len();
    let truncated = total > MAX_DIRECTORY_ENTRIES;
    if truncated {
        entries.truncate(MAX_DIRECTORY_ENTRIES);
        entries.push(format!(
            "\n... and {} more entries (truncated, showing first 1000)",
            total - MAX_DIRECTORY_ENTRIES
        ));
    }
    Response::success(
        &req.id,
        serde_json::json!({
            "entries": entries,
            "complete": !truncated,
            "truncated": truncated,
            "total_entries": total,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use image::{DynamicImage, ImageBuffer, Rgba};
    use serde_json::{json, Value};

    use crate::config::Config;
    use crate::context::default_language_provider_factory;

    #[test]
    fn github_read_response_preserves_engine_text_and_attachment_metadata() {
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
            .expect("first attachment object")
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
        let temp = tempfile::tempdir().expect("tempdir");
        let noisy = rgba_image(1536, 1536, true);
        let bytes = encode_png(&noisy).unwrap();
        let path = write_fixture(temp.path(), "noisy.png", &bytes);

        let response = read_response(temp.path(), &path, json!({}));

        assert!(response.success);
        assert_eq!(response.data["attachments"].as_array().unwrap().len(), 0);
        assert!(response.data["attachment_omitted_reason"]
            .as_str()
            .unwrap()
            .contains("too large to inline"));
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
