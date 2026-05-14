//! Image-family Witness Mesh rules (catalog v1.1).
//!
//! Pure-Rust, deterministic feature extraction over raster images.
//! Every rule consumes the whole file bytes as a single span
//! (`spans[0] = (file_blake3, 0, bytes.len())`); the Witness's
//! payload is the encoded feature plus the standard provenance
//! triple. Image content is not text — there's no internal byte
//! range to anchor on, so a whole-file anchor is the honest model.
//!
//! Five active rules + one honest-absence rule:
//! - `image::phash@v1` — 8×8 DCT perceptual hash (near-duplicate)
//! - `image::color-histogram@v1` — 16-bucket-per-channel RGB hist
//! - `image::edge-summary@v1` — Sobel edge density + mean intensity
//! - `image::exif@v1` — EXIF key/value pairs
//! - `image::dominant-colors@v1` — top-K RGB clusters
//! - `image::skipped@v1` — emitted when decode fails (honest absence)
//!
//! No LLM. No shell-outs. No ffmpeg. The `image` crate's default
//! features are turned OFF in Cargo.toml so only the formats we
//! explicitly enable (jpeg/png/gif/webp/tiff/bmp/pnm) compile in.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Maximum byte budget for a single image input. Larger images are
/// declined with `image::skipped@v1` rather than attempted — the
/// `image` crate is single-threaded per decode and a 100 MiB JPEG
/// can stall a compile for minutes. 32 MiB covers ~99% of photos
/// and screenshots without making the pipeline a DoS vector.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Top-K dominant colours surfaced by `image::dominant-colors@v1`.
/// Five is the standard palette-fingerprint width (matches the
/// "five-color palette" UI convention).
const DOMINANT_K: usize = 5;

/// File extensions this module accepts. Walker / parser wire-through
/// uses the same set so an extension addition lands in one place.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "tiff", "tif", "bmp", "pnm", "ppm", "pgm", "pbm",
];

/// True when `ext` is a recognised image extension (lower-case
/// comparison; caller normalises the dot off).
pub fn is_image_extension(ext: &str) -> bool {
    IMAGE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// Extract all image-family witnesses from a single file's bytes.
///
/// Returns up to 5 witnesses on success — one per active rule that
/// produced output — plus an `image::skipped@v1` witness when
/// decode fails or the file exceeds [`MAX_IMAGE_BYTES`]. Never
/// panics; never returns an `Err` — image extraction failure is an
/// observability event, not a pipeline-fatal error.
///
/// `file_blake3` must be the BLAKE3 hex of the exact bytes passed
/// as `bytes`. Callers (pipeline, backfill) compute this off the
/// disk once and reuse it across every rule.
pub fn extract_image_witnesses(
    bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    if file_blake3.is_empty() {
        return Vec::new();
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return vec![skipped_witness(
            file_blake3,
            bytes.len(),
            source_id,
            workspace_id,
            now,
            format!("file exceeds MAX_IMAGE_BYTES ({MAX_IMAGE_BYTES} bytes)"),
        )];
    }

    // Decode once and reuse — every active rule reads the same
    // pixel buffer. Decoding twice would be a hot-loop waste.
    let img = match image::load_from_memory(bytes) {
        Ok(img) => img,
        Err(e) => {
            return vec![skipped_witness(
                file_blake3,
                bytes.len(),
                source_id,
                workspace_id,
                now,
                format!("decode failed: {e}"),
            )];
        }
    };

    let span = WitnessSpan {
        file_blake3: file_blake3.into(),
        start: 0,
        end: bytes.len() as u64,
    };
    let input = WitnessInput::ByteRef {
        file_blake3: file_blake3.into(),
        start: 0,
        end: bytes.len() as u64,
    };
    let content_blake3 = blake3::hash(bytes).to_hex().to_string();

    let mut out = Vec::with_capacity(5);

    // image::phash@v1 — 8x8 DCT perceptual hash via img_hash crate.
    if let Some(w) = build_phash_witness(
        &img,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ) {
        out.push(w);
    }

    // image::color-histogram@v1
    out.push(build_color_histogram_witness(
        &img,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ));

    // image::edge-summary@v1
    out.push(build_edge_summary_witness(
        &img,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ));

    // image::dominant-colors@v1
    out.push(build_dominant_colors_witness(
        &img,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ));

    // image::exif@v1 — only emitted when the file actually carries
    // EXIF (JPEGs, some TIFFs). Honest absence: no witness for an
    // EXIF-less PNG, not an empty-payload row.
    if let Some(w) = build_exif_witness(
        bytes,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ) {
        out.push(w);
    }

    out
}

fn build_phash_witness(
    img: &image::DynamicImage,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Option<Witness> {
    // 8x8 mean-hash. Resize → luma → 64 thresholded bits packed
    // into 8 hex bytes. Inline implementation (vs the `img_hash`
    // crate) keeps us on a single `image` crate version and the
    // determinism story stays tight: `nearest` resampling produces
    // byte-identical output across runs.
    let small = img.resize_exact(8, 8, image::imageops::FilterType::Nearest);
    let luma = small.to_luma8();
    let pixels: Vec<u8> = luma.pixels().map(|p| p[0]).collect();
    debug_assert_eq!(pixels.len(), 64);
    let mean: u32 = (pixels.iter().map(|&p| u32::from(p)).sum::<u32>()) / 64;
    let mut bits: u64 = 0;
    for (i, &p) in pixels.iter().enumerate() {
        if u32::from(p) >= mean {
            bits |= 1u64 << (63 - i);
        }
    }
    let payload = format!("{bits:016x}");
    let mut w = Witness::new(
        "image::phash@v1",
        "image::phash",
        vec![input.clone()],
        vec![span.clone()],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(payload);
    Some(w)
}

fn build_color_histogram_witness(
    img: &image::DynamicImage,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    let rgb = img.to_rgb8();
    // 16 buckets per channel = 4096 total. Each bucket = a u32 count.
    let mut buckets = vec![0u32; 4096];
    for px in rgb.pixels() {
        let r = (px[0] >> 4) as usize;
        let g = (px[1] >> 4) as usize;
        let b = (px[2] >> 4) as usize;
        buckets[r * 256 + g * 16 + b] = buckets[r * 256 + g * 16 + b].saturating_add(1);
    }
    let total: u64 = buckets.iter().map(|&v| v as u64).sum();
    // Encode as a compact `<idx>:<count>;...` string sorted by idx,
    // dropping zero buckets. Same input image → identical payload
    // byte-for-byte across runs.
    let mut payload = String::with_capacity(buckets.len() * 6);
    for (idx, &count) in buckets.iter().enumerate() {
        if count == 0 {
            continue;
        }
        if !payload.is_empty() {
            payload.push(';');
        }
        let _ = std::fmt::Write::write_fmt(&mut payload, format_args!("{idx}:{count}"));
    }
    let mut w = Witness::new(
        "image::color-histogram@v1",
        "image::color-histogram",
        vec![input.clone()],
        vec![span.clone()],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(format!("total={total};{payload}"));
    w
}

fn build_edge_summary_witness(
    img: &image::DynamicImage,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    // Sobel edge density: count of pixels whose gradient magnitude
    // exceeds a fixed threshold, normalised by total pixel count.
    // Mean intensity = average luma. Both quantised to integer
    // hundredths to make the payload integer-stable across float
    // ordering differences between platforms.
    let luma = img.to_luma8();
    let (w_px, h_px) = luma.dimensions();
    if w_px < 3 || h_px < 3 {
        // Too small for a 3x3 Sobel kernel — emit zero density.
        let mut w = Witness::new(
            "image::edge-summary@v1",
            "image::edge-summary",
            vec![input.clone()],
            vec![span.clone()],
            source_id,
            workspace_id,
            Sensitivity::Public,
            Confidence::new(0.99),
            content_blake3,
            now,
        );
        w.symbol = Some(format!(
            "edge_density_pct=0;mean_intensity={};width={};height={}",
            mean_intensity(&luma),
            w_px,
            h_px
        ));
        return w;
    }
    let mut edge_px = 0u64;
    let total_px = ((w_px - 2) as u64) * ((h_px - 2) as u64);
    let threshold = 60i32; // empirical mid-range; tuned for screenshot + photo edges to register comparably
    for y in 1..h_px - 1 {
        for x in 1..w_px - 1 {
            let p = |dx: i32, dy: i32| -> i32 {
                luma.get_pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32)[0] as i32
            };
            let gx = -p(-1, -1) - 2 * p(-1, 0) - p(-1, 1)
                + p(1, -1) + 2 * p(1, 0) + p(1, 1);
            let gy = -p(-1, -1) - 2 * p(0, -1) - p(1, -1)
                + p(-1, 1) + 2 * p(0, 1) + p(1, 1);
            // |∇| ≈ |gx| + |gy| — Manhattan approximation is enough
            // for thresholded edge counting and skips a sqrt per
            // pixel.
            if gx.abs() + gy.abs() > threshold {
                edge_px += 1;
            }
        }
    }
    let edge_density_pct = if total_px == 0 {
        0
    } else {
        // 0..=10000 in hundredths-of-a-percent. Saturates to u32.
        ((edge_px as u128 * 10_000) / total_px as u128) as u32
    };
    let mut w = Witness::new(
        "image::edge-summary@v1",
        "image::edge-summary",
        vec![input.clone()],
        vec![span.clone()],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(format!(
        "edge_density_pct={edge_density_pct};mean_intensity={};width={};height={}",
        mean_intensity(&luma),
        w_px,
        h_px
    ));
    w
}

fn mean_intensity(luma: &image::GrayImage) -> u32 {
    let total: u64 = luma.pixels().map(|p| u64::from(p[0])).sum();
    let n = luma.pixels().len().max(1) as u64;
    (total / n) as u32
}

fn build_dominant_colors_witness(
    img: &image::DynamicImage,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    // Online quantisation: bucket every pixel into the 4096 16^3
    // grid used by the histogram, then take the top-K buckets. Same
    // input → byte-identical payload (sort is stable on count desc
    // and key asc).
    let rgb = img.to_rgb8();
    let mut counts: BTreeMap<u16, u32> = BTreeMap::new();
    for px in rgb.pixels() {
        let r = (px[0] >> 4) as u16;
        let g = (px[1] >> 4) as u16;
        let b = (px[2] >> 4) as u16;
        let key = r * 256 + g * 16 + b;
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut ranked: Vec<(u16, u32)> = counts.into_iter().collect();
    // Sort by count desc, tie-break by bucket id asc — deterministic.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut payload = String::new();
    for (idx, (key, count)) in ranked.iter().take(DOMINANT_K).enumerate() {
        let r = (*key / 256) as u8 * 16;
        let g = ((*key / 16) % 16) as u8 * 16;
        let b = (*key % 16) as u8 * 16;
        if idx > 0 {
            payload.push(';');
        }
        let _ = std::fmt::Write::write_fmt(
            &mut payload,
            format_args!("#{r:02x}{g:02x}{b:02x}:{count}"),
        );
    }
    let mut w = Witness::new(
        "image::dominant-colors@v1",
        "image::dominant-colors",
        vec![input.clone()],
        vec![span.clone()],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(payload);
    w
}

fn build_exif_witness(
    bytes: &[u8],
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Option<Witness> {
    let exifreader = exif::Reader::new();
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = match exifreader.read_from_container(&mut cursor) {
        Ok(e) => e,
        Err(_) => return None, // No EXIF — honest absence, no witness.
    };
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    for f in exif.fields() {
        let tag = f.tag.to_string();
        let value = f.display_value().with_unit(&exif).to_string();
        kv.insert(tag, value);
    }
    if kv.is_empty() {
        return None;
    }
    // Encode as `key=value\nkey=value\n...` sorted by key (BTreeMap
    // iteration). UTF-8 by construction; we cap each value at 256
    // chars to keep one EXIF value from blowing the payload.
    let mut payload = String::new();
    for (k, v) in &kv {
        let trimmed: String = v.chars().take(256).collect();
        let _ = std::fmt::Write::write_fmt(&mut payload, format_args!("{k}={trimmed}\n"));
    }
    let mut w = Witness::new(
        "image::exif@v1",
        "image::exif",
        vec![input.clone()],
        vec![span.clone()],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(payload);
    Some(w)
}

fn skipped_witness(
    file_blake3: &str,
    byte_len: usize,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
    reason: String,
) -> Witness {
    let span = WitnessSpan {
        file_blake3: file_blake3.into(),
        start: 0,
        end: byte_len as u64,
    };
    let input = WitnessInput::ByteRef {
        file_blake3: file_blake3.into(),
        start: 0,
        end: byte_len as u64,
    };
    // No bytes to hash for a skipped row — anchor on the file
    // hash. The pipeline reads the same file from disk on the next
    // compile and re-checks the rule; a skipped row that turned
    // into a decodable image surfaces as the actual feature
    // witnesses (and this row gets dropped as an orphan by the
    // water-flow cascade).
    let content_blake3 = file_blake3.to_string();
    let mut w = Witness::new(
        "image::skipped@v1",
        "image::skipped",
        vec![input],
        vec![span],
        source_id,
        workspace_id,
        Sensitivity::Public,
        Confidence::new(0.99),
        content_blake3,
        now,
    );
    w.symbol = Some(reason);
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbImage};

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture_png(width: u32, height: u32, fill: Rgb<u8>) -> Vec<u8> {
        let mut buf: RgbImage = ImageBuffer::from_pixel(width, height, fill);
        // Overlay a few non-fill pixels so edge detection has signal.
        if width > 4 && height > 4 {
            for x in 1..width - 1 {
                buf.put_pixel(x, 1, Rgb([0, 0, 0]));
                buf.put_pixel(x, height - 2, Rgb([0, 0, 0]));
            }
        }
        let mut bytes: Vec<u8> = Vec::new();
        buf.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn is_image_extension_recognises_common_formats() {
        assert!(is_image_extension("png"));
        assert!(is_image_extension("JPG"));
        assert!(is_image_extension("jpeg"));
        assert!(is_image_extension("webp"));
        assert!(!is_image_extension("md"));
        assert!(!is_image_extension(""));
    }

    #[test]
    fn extract_image_witnesses_returns_four_for_png_without_exif() {
        let bytes = fixture_png(16, 16, Rgb([128, 200, 64]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        // PNG has no EXIF by default → exif witness is suppressed
        // (honest absence). Expect: phash, histogram, edges, dominant.
        assert_eq!(witnesses.len(), 4, "got {} witnesses", witnesses.len());
        let rule_names: Vec<&str> = witnesses.iter().map(|w| w.rule.as_str()).collect();
        assert!(rule_names.contains(&"image::phash@v1"));
        assert!(rule_names.contains(&"image::color-histogram@v1"));
        assert!(rule_names.contains(&"image::edge-summary@v1"));
        assert!(rule_names.contains(&"image::dominant-colors@v1"));
        // The skipped rule must NOT fire on a valid decode.
        assert!(!rule_names.contains(&"image::skipped@v1"));
    }

    #[test]
    fn extract_image_witnesses_is_deterministic_across_runs() {
        let bytes = fixture_png(32, 32, Rgb([200, 50, 100]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let src = SourceId::new();
        let ws = WorkspaceId::new();
        let a = extract_image_witnesses(&bytes, &file_blake3, src, ws, fixed_now());
        let b = extract_image_witnesses(&bytes, &file_blake3, src, ws, fixed_now());
        assert_eq!(a.len(), b.len());
        for (wa, wb) in a.iter().zip(b.iter()) {
            assert_eq!(wa.rule, wb.rule);
            assert_eq!(wa.symbol, wb.symbol, "same input must produce same symbol payload");
            assert_eq!(wa.content_blake3, wb.content_blake3);
        }
    }

    #[test]
    fn extract_image_witnesses_emits_skipped_for_garbage_bytes() {
        // Random bytes — image::load_from_memory returns Err.
        let bytes = b"\x00not an image\x00\xff\xff";
        let file_blake3 = blake3::hash(bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "image::skipped@v1");
        assert!(witnesses[0]
            .symbol
            .as_deref()
            .unwrap_or("")
            .contains("decode failed"));
    }

    #[test]
    fn extract_image_witnesses_returns_empty_when_file_blake3_is_blank() {
        let bytes = fixture_png(8, 8, Rgb([1, 2, 3]));
        let witnesses = extract_image_witnesses(
            &bytes,
            "",
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert!(witnesses.is_empty(), "blank file_blake3 = no witnesses");
    }

    #[test]
    fn extract_image_witnesses_skips_oversized_input() {
        // Build a header that decodes happily up to MAX_IMAGE_BYTES,
        // but artificially extend the bytes to trip the size gate
        // BEFORE decode. We use raw bytes (no need to be a real image
        // — the size check fires first).
        let mut bytes = vec![0u8; MAX_IMAGE_BYTES + 1];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "image::skipped@v1");
        assert!(witnesses[0]
            .symbol
            .as_deref()
            .unwrap_or("")
            .contains("exceeds MAX_IMAGE_BYTES"));
    }

    #[test]
    fn phash_witness_payload_is_16_hex_chars() {
        let bytes = fixture_png(64, 64, Rgb([10, 20, 30]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let phash = witnesses.iter().find(|w| w.rule == "image::phash@v1").unwrap();
        let payload = phash.symbol.as_deref().unwrap();
        // 64 bits → exactly 16 hex chars. Pin the size so a future
        // tweak to the resize / quantisation surfaces here.
        assert_eq!(payload.len(), 16, "phash should be 16 hex chars, got: {payload:?}");
        assert!(payload.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn dominant_colors_payload_lists_at_most_five_entries() {
        let bytes = fixture_png(32, 32, Rgb([200, 100, 50]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let dom = witnesses
            .iter()
            .find(|w| w.rule == "image::dominant-colors@v1")
            .unwrap();
        let payload = dom.symbol.as_deref().unwrap();
        let count = payload.matches(';').count() + 1;
        assert!(count <= DOMINANT_K, "got {count} dominant entries");
    }

    #[test]
    fn edge_summary_reports_higher_density_on_busy_image() {
        let solid = fixture_png(64, 64, Rgb([200, 200, 200]));
        let busy = {
            // Vertical stripes 4 pixels wide. 3x3 Sobel sees real
            // gradients at every stripe boundary (a 1-pixel
            // checkerboard cancels symmetrically — high-frequency
            // pattern blind spot — so we use a slightly lower
            // frequency that still surfaces every stripe edge).
            let mut buf: RgbImage = ImageBuffer::new(64, 64);
            for y in 0..64 {
                for x in 0..64 {
                    let v = if (x / 4) % 2 == 0 { 0 } else { 255 };
                    buf.put_pixel(x, y, Rgb([v, v, v]));
                }
            }
            let mut bytes = Vec::new();
            buf.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        let solid_h = blake3::hash(&solid).to_hex().to_string();
        let busy_h = blake3::hash(&busy).to_hex().to_string();
        let src = SourceId::new();
        let ws = WorkspaceId::new();
        let solid_ws = extract_image_witnesses(&solid, &solid_h, src, ws, fixed_now());
        let busy_ws = extract_image_witnesses(&busy, &busy_h, src, ws, fixed_now());
        let solid_edge = solid_ws
            .iter()
            .find(|w| w.rule == "image::edge-summary@v1")
            .unwrap();
        let busy_edge = busy_ws
            .iter()
            .find(|w| w.rule == "image::edge-summary@v1")
            .unwrap();
        let solid_density = parse_edge_density_pct(solid_edge.symbol.as_deref().unwrap());
        let busy_density = parse_edge_density_pct(busy_edge.symbol.as_deref().unwrap());
        assert!(
            busy_density > solid_density,
            "checkerboard (busy_density={busy_density}) must be busier than the solid fill (solid_density={solid_density})"
        );
    }

    fn parse_edge_density_pct(symbol: &str) -> u32 {
        for piece in symbol.split(';') {
            if let Some(rest) = piece.strip_prefix("edge_density_pct=") {
                return rest.parse().unwrap_or(0);
            }
        }
        0
    }

    #[test]
    fn color_histogram_payload_total_matches_pixel_count() {
        let bytes = fixture_png(8, 8, Rgb([10, 20, 30]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let hist = witnesses
            .iter()
            .find(|w| w.rule == "image::color-histogram@v1")
            .unwrap();
        let payload = hist.symbol.as_deref().unwrap();
        let total_str = payload
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("total=")
            .unwrap();
        let total: u64 = total_str.parse().unwrap();
        // The fixture is 8x8 = 64 pixels; the overlay logic only
        // fires for width > 4, so 8x8 carries the overlay too.
        assert_eq!(total, 64);
    }

    #[test]
    fn witness_content_blake3_matches_file_blake3_for_whole_file_anchor() {
        let bytes = fixture_png(8, 8, Rgb([1, 2, 3]));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_image_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        for w in &witnesses {
            assert_eq!(
                w.content_blake3, file_blake3,
                "whole-file anchor: content_blake3 must equal file_blake3 (rule={})",
                w.rule
            );
        }
    }
}
