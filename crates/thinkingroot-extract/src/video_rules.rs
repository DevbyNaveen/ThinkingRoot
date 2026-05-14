//! Video-family Witness Mesh rules (catalog v1.3).
//!
//! Pure-Rust deterministic feature extraction over video files. Four
//! rules: container/codec duration, per-keyframe witness (one per
//! I-frame), scene-change inference (keyframe gap > threshold), and
//! the honest-absence skipped witness for unsupported containers /
//! parse failures.
//!
//! # Backend
//!
//! Uses the [`mp4`] crate — pure-Rust ISO base media file format
//! demuxer covering MP4, MOV, and 3GP containers. Other containers
//! (WebM, MKV, AVI) emit `video::skipped@v1` rather than failing
//! compile — researchers can transcode to MP4 to unlock the full
//! rule set.
//!
//! # Decoder boundary
//!
//! v1 ships **demux-only** — we never decode pixel data. Per-keyframe
//! perceptual hashing requires a frame decoder (H.264 / AV1 / etc.)
//! which transitively pulls in heavy C/C++ deps (openh264, dav1d).
//! Demuxing gives us byte-anchored keyframe witnesses + scene-change
//! inference; the witness payload is honest about what was extracted.
//!
//! No LLM. No ffmpeg. No shell-outs.

use std::io::Cursor;

use chrono::{DateTime, Utc};
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Byte budget per video file. 1 GiB covers most short-form research
/// uploads (lectures, interview clips, screen recordings); anything
/// larger emits `video::skipped@v1` rather than blocking compile for
/// minutes on a hostile 4K master.
const MAX_VIDEO_BYTES: usize = 1024 * 1024 * 1024;

/// Scene-change inference threshold. Keyframes whose gap from the
/// prior keyframe exceeds 5 seconds usually fall on a scene boundary
/// — most encoders insert keyframes at scene cuts plus a periodic
/// minimum (every 2-4s by default). The witness records the actual
/// gap so downstream consumers can re-threshold.
const SCENE_CHANGE_GAP_SECS: f64 = 5.0;

/// Cap on emitted keyframe witnesses to bound DB write cost. A
/// 90-minute lecture at 1-keyframe/4s yields ~1350 keyframes — still
/// fine. 4-hour talk dumps would balloon — cap at 2000 keyframes.
/// Beyond the cap we emit a single summary witness with the truncated
/// count so the workspace is honest about partial extraction.
const MAX_KEYFRAME_WITNESSES: usize = 2000;

/// File extensions this module accepts. Walker / parser wire-through
/// uses the same set. Note: `mp4` overlaps with audio M4A files —
/// `.m4a` continues to route to `audio_rules` per its extension list;
/// `.mp4` routes here because the dominant use is video.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "3gp", "3gpp",
    // Containers we don't demux but recognise so we emit an honest
    // skipped witness instead of silently routing them through the
    // text-chunk pipeline.
    "webm", "mkv", "avi", "flv", "wmv", "ogv",
];

/// Extensions for which we can actually demux + emit keyframes. The
/// `mp4` crate covers MP4 / MOV / 3GP / M4V; WebM and friends fall
/// through to `video::skipped@v1`.
const DEMUXABLE_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "3gp", "3gpp"];

/// True when `ext` is a recognised video extension (lower-case
/// comparison; caller normalises the dot off).
pub fn is_video_extension(ext: &str) -> bool {
    VIDEO_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// True when `ext` is a container we can actually demux. Non-demuxable
/// recognised containers (WebM, MKV, AVI) still emit a `video::skipped`
/// witness so the workspace's catalog stays honest.
fn is_demuxable(ext: &str) -> bool {
    DEMUXABLE_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(ext))
}

/// Extract all video-family witnesses from a single file's bytes.
///
/// Returns:
/// - One `video::duration@v1` witness with movie duration + track summary.
/// - One `video::keyframe@v1` witness per I-frame, capped at
///   [`MAX_KEYFRAME_WITNESSES`].
/// - One `video::scene-change@v1` witness per detected scene break
///   (keyframe gap > [`SCENE_CHANGE_GAP_SECS`]).
/// - `video::skipped@v1` when the file exceeds [`MAX_VIDEO_BYTES`],
///   isn't a demuxable container, or fails to parse.
///
/// Never panics; video extraction failure is observability, not
/// pipeline-fatal.
pub fn extract_video_witnesses(
    bytes: &[u8],
    file_blake3: &str,
    extension: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    if file_blake3.is_empty() {
        return Vec::new();
    }
    if bytes.len() > MAX_VIDEO_BYTES {
        return vec![skipped_witness(
            file_blake3,
            bytes.len(),
            source_id,
            workspace_id,
            now,
            format!("file exceeds MAX_VIDEO_BYTES ({MAX_VIDEO_BYTES} bytes)"),
        )];
    }
    if !is_demuxable(extension) {
        return vec![skipped_witness(
            file_blake3,
            bytes.len(),
            source_id,
            workspace_id,
            now,
            format!(
                "container `.{ext}` recognised but not yet demuxable (v1 ships MP4/MOV/3GP only)",
                ext = extension
            ),
        )];
    }

    let demuxed = match demux_mp4(bytes) {
        Ok(d) => d,
        Err(reason) => {
            return vec![skipped_witness(
                file_blake3,
                bytes.len(),
                source_id,
                workspace_id,
                now,
                reason,
            )];
        }
    };

    let mut out: Vec<Witness> = Vec::with_capacity(demuxed.keyframes.len() + 4);
    let whole_file_span = WitnessSpan {
        file_blake3: file_blake3.into(),
        start: 0,
        end: bytes.len() as u64,
    };
    let whole_file_input = WitnessInput::ByteRef {
        file_blake3: file_blake3.into(),
        start: 0,
        end: bytes.len() as u64,
    };
    let content_blake3_whole = blake3::hash(bytes).to_hex().to_string();

    // Duration witness — one per file.
    out.push(make_witness(
        "video::duration@v1",
        "video::duration",
        format!(
            "duration_secs={dur:.3};video_tracks={vt};audio_tracks={at}",
            dur = demuxed.duration_secs,
            vt = demuxed.video_tracks,
            at = demuxed.audio_tracks,
        ),
        vec![whole_file_input.clone()],
        vec![whole_file_span.clone()],
        content_blake3_whole.clone(),
        source_id,
        workspace_id,
        now,
    ));

    // Keyframe witnesses — capped to MAX_KEYFRAME_WITNESSES. Cap
    // overflow surfaces as a single summary witness so the workspace
    // doesn't silently lose later keyframes.
    let total_keyframes = demuxed.keyframes.len();
    let render_cap = total_keyframes.min(MAX_KEYFRAME_WITNESSES);
    for (idx, kf) in demuxed.keyframes.iter().take(render_cap).enumerate() {
        // The mp4 crate exposes sample offset/size at the file
        // level. We anchor the keyframe witness on the sample's
        // byte range when known; otherwise fall back to the
        // whole-file anchor (still byte-anchored — CCC I-2).
        let kf_span = kf.span(file_blake3, bytes.len());
        let kf_input = WitnessInput::ByteRef {
            file_blake3: file_blake3.into(),
            start: kf_span.start,
            end: kf_span.end,
        };
        let kf_content_blake3 = keyframe_content_blake3(bytes, &kf_span);
        let payload = format!(
            "index={idx};timestamp_secs={ts:.3}",
            ts = kf.timestamp_secs,
        );
        out.push(make_witness(
            "video::keyframe@v1",
            "video::keyframe",
            payload,
            vec![kf_input],
            vec![kf_span],
            kf_content_blake3,
            source_id,
            workspace_id,
            now,
        ));
    }
    if total_keyframes > render_cap {
        let payload = format!(
            "total={total};emitted={emitted};truncated={truncated};cap={cap}",
            total = total_keyframes,
            emitted = render_cap,
            truncated = total_keyframes - render_cap,
            cap = MAX_KEYFRAME_WITNESSES,
        );
        out.push(make_witness(
            "video::keyframe-overflow@v1",
            "video::keyframe",
            payload,
            vec![whole_file_input.clone()],
            vec![whole_file_span.clone()],
            content_blake3_whole.clone(),
            source_id,
            workspace_id,
            now,
        ));
    }

    // Scene-change inference — for each pair of consecutive keyframes
    // whose gap exceeds SCENE_CHANGE_GAP_SECS, emit one
    // `video::scene-change@v1` witness anchored to the later keyframe.
    let mut prev_ts: Option<f64> = None;
    for kf in &demuxed.keyframes {
        if let Some(prev) = prev_ts {
            let gap = kf.timestamp_secs - prev;
            if gap >= SCENE_CHANGE_GAP_SECS {
                let kf_span = kf.span(file_blake3, bytes.len());
                let kf_input = WitnessInput::ByteRef {
                    file_blake3: file_blake3.into(),
                    start: kf_span.start,
                    end: kf_span.end,
                };
                let kf_content_blake3 = keyframe_content_blake3(bytes, &kf_span);
                let payload = format!(
                    "from_secs={prev:.3};to_secs={to:.3};gap_secs={gap:.3}",
                    to = kf.timestamp_secs,
                );
                out.push(make_witness(
                    "video::scene-change@v1",
                    "video::scene-change",
                    payload,
                    vec![kf_input],
                    vec![kf_span],
                    kf_content_blake3,
                    source_id,
                    workspace_id,
                    now,
                ));
            }
        }
        prev_ts = Some(kf.timestamp_secs);
    }

    out
}

/// BLAKE3 over the keyframe's exact byte slice when in range, else
/// over the whole file. CCC I-4 contract: `content_blake3` is the
/// BLAKE3 over the source bytes the witness anchors to.
fn keyframe_content_blake3(bytes: &[u8], span: &WitnessSpan) -> String {
    let start = span.start as usize;
    let end = span.end as usize;
    if start < end && end <= bytes.len() {
        blake3::hash(&bytes[start..end]).to_hex().to_string()
    } else {
        blake3::hash(bytes).to_hex().to_string()
    }
}

/// Result of a successful MP4 demux.
struct DemuxedVideo {
    duration_secs: f64,
    video_tracks: usize,
    audio_tracks: usize,
    keyframes: Vec<KeyframeInfo>,
}

/// One demuxed I-frame.
struct KeyframeInfo {
    timestamp_secs: f64,
    /// Byte offset in the file. `None` when the mp4 crate doesn't
    /// expose it for the current sample (rare; falls back to
    /// whole-file anchor).
    byte_offset: Option<u64>,
    /// Sample size in bytes, if known.
    byte_size: Option<u32>,
}

impl KeyframeInfo {
    fn span(&self, file_blake3: &str, total_bytes: usize) -> WitnessSpan {
        match (self.byte_offset, self.byte_size) {
            (Some(off), Some(size)) if (off as usize + size as usize) <= total_bytes => {
                WitnessSpan {
                    file_blake3: file_blake3.into(),
                    start: off,
                    end: off + size as u64,
                }
            }
            _ => WitnessSpan {
                file_blake3: file_blake3.into(),
                start: 0,
                end: total_bytes as u64,
            },
        }
    }
}

fn demux_mp4(bytes: &[u8]) -> Result<DemuxedVideo, String> {
    let cursor = Cursor::new(bytes);
    let mut reader = mp4::Mp4Reader::read_header(cursor, bytes.len() as u64)
        .map_err(|e| format!("mp4 header parse failed: {e}"))?;
    let duration_secs = reader.duration().as_secs_f64();

    // Collect the per-track metadata we'll iterate against. Done in
    // an immutable borrow scope so the subsequent `read_sample`
    // calls can take `&mut reader` without aliasing.
    struct VideoTrackMeta {
        track_id: u32,
        timescale: u32,
        sample_count: u32,
    }
    let mut video_meta: Vec<VideoTrackMeta> = Vec::new();
    let mut video_tracks: usize = 0;
    let mut audio_tracks: usize = 0;
    for track in reader.tracks().values() {
        match track.track_type().ok() {
            Some(mp4::TrackType::Video) => {
                video_tracks += 1;
                let timescale = track.timescale();
                let sample_count = track.sample_count();
                if timescale == 0 || sample_count == 0 {
                    continue;
                }
                video_meta.push(VideoTrackMeta {
                    track_id: track.track_id(),
                    timescale,
                    sample_count,
                });
            }
            Some(mp4::TrackType::Audio) => {
                audio_tracks += 1;
            }
            _ => {}
        }
    }

    let mut keyframes: Vec<KeyframeInfo> = Vec::new();
    for meta in &video_meta {
        for sample_id in 1..=meta.sample_count {
            // `read_sample` reads the sample bytes too — wasteful
            // here since we only need is_sync + offset metadata. The
            // mp4 crate's metadata-only sample APIs are pub(crate),
            // so this is the available public surface. Worst case
            // we touch each sample once; for a 90-minute lecture at
            // 30fps that's ~160K samples, which still completes in
            // ~hundreds of ms on M-series.
            let sample = match reader.read_sample(meta.track_id, sample_id) {
                Ok(Some(s)) => s,
                _ => continue,
            };
            if !sample.is_sync {
                continue;
            }
            let ts_secs = sample.start_time as f64 / meta.timescale as f64;
            // `Mp4Sample.bytes.len()` is the sample size; the
            // `offset` field isn't directly exposed on the public
            // type so we anchor by sample length only. The
            // whole-file anchor remains the fallback in
            // KeyframeInfo::span when offset is None.
            let byte_size = sample.bytes.len() as u32;
            keyframes.push(KeyframeInfo {
                timestamp_secs: ts_secs,
                byte_offset: None, // offset is private on Mp4Sample
                byte_size: Some(byte_size),
            });
        }
    }

    // Sort by timestamp so scene-change inference + cap truncation
    // are deterministic regardless of track-walk order.
    keyframes.sort_by(|a, b| {
        a.timestamp_secs
            .partial_cmp(&b.timestamp_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(DemuxedVideo {
        duration_secs,
        video_tracks,
        audio_tracks,
        keyframes,
    })
}

/// Build a `video::skipped@v1` witness carrying the reason. The
/// reason is encoded into the `symbol` payload (mirrors the
/// `audio::skipped@v1` pattern).
fn skipped_witness(
    file_blake3: &str,
    file_size: usize,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
    reason: String,
) -> Witness {
    let span = WitnessSpan {
        file_blake3: file_blake3.into(),
        start: 0,
        end: file_size as u64,
    };
    let input = WitnessInput::ByteRef {
        file_blake3: file_blake3.into(),
        start: 0,
        end: file_size as u64,
    };
    let payload = format!("reason={reason};file_size={file_size}");
    make_witness(
        "video::skipped@v1",
        "video::skipped",
        payload,
        vec![input],
        vec![span],
        file_blake3.to_string(),
        source_id,
        workspace_id,
        now,
    )
}

/// Common witness builder. `rule` is the catalog id (e.g.
/// `video::keyframe@v1`); `witness_type` is the produced category
/// (e.g. `video::keyframe`). Confidence is fixed at the catalog
/// default (0.99 for every video rule) — there's no probabilistic
/// inference in demuxing.
fn make_witness(
    rule: &'static str,
    witness_type: &'static str,
    payload: String,
    inputs: Vec<WitnessInput>,
    spans: Vec<WitnessSpan>,
    content_blake3: String,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    let mut w = Witness::new(
        rule,
        witness_type,
        inputs,
        spans,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_video_extension_recognises_common_formats() {
        for ext in ["mp4", "MOV", "Webm", "mkv", "3gp"] {
            assert!(is_video_extension(ext), "{ext} not recognised");
        }
        for ext in ["wav", "png", "txt", ""] {
            assert!(!is_video_extension(ext), "{ext} should not be video");
        }
    }

    #[test]
    fn is_demuxable_returns_true_only_for_iso_bmff() {
        for ext in ["mp4", "mov", "m4v", "3gp"] {
            assert!(is_demuxable(ext));
        }
        for ext in ["webm", "mkv", "avi", "ogv"] {
            assert!(!is_demuxable(ext));
        }
    }

    #[test]
    fn empty_blake3_yields_no_witnesses() {
        let out = extract_video_witnesses(
            &[0u8; 100],
            "",
            "mp4",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn unsupported_container_emits_skipped_witness() {
        let out = extract_video_witnesses(
            &[0u8; 100],
            "abc123",
            "webm",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "video::skipped@v1");
        let payload = out[0].symbol.as_deref().unwrap_or("");
        assert!(
            payload.contains("not yet demuxable"),
            "expected 'not yet demuxable' in payload, got: {payload}"
        );
    }

    #[test]
    fn oversized_file_emits_skipped_witness() {
        // Synthesise a buffer over the cap. Allocation is cheap in
        // release; in debug it's a few hundred ms — still under test
        // budget.
        let huge = vec![0u8; MAX_VIDEO_BYTES + 1];
        let out = extract_video_witnesses(
            &huge,
            "abc123",
            "mp4",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "video::skipped@v1");
    }

    #[test]
    fn parse_failure_on_garbage_emits_skipped_witness() {
        let garbage = b"this is definitely not an mp4 file at all".to_vec();
        let out = extract_video_witnesses(
            &garbage,
            "abc123",
            "mp4",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "video::skipped@v1");
        let payload = out[0].symbol.as_deref().unwrap_or("");
        assert!(
            payload.contains("mp4 header parse failed"),
            "expected 'mp4 header parse failed' in payload, got: {payload}"
        );
    }

    #[test]
    fn skipped_witness_carries_file_blake3() {
        let out = extract_video_witnesses(
            b"garbage",
            "myhashvalue",
            "mp4",
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans[0].file_blake3, "myhashvalue");
    }

    #[test]
    fn make_witness_sets_anchor_columns_consistently() {
        let span = WitnessSpan {
            file_blake3: "hash".into(),
            start: 10,
            end: 30,
        };
        let input = WitnessInput::ByteRef {
            file_blake3: "hash".into(),
            start: 10,
            end: 30,
        };
        let w = make_witness(
            "video::duration@v1",
            "video::duration",
            "test".into(),
            vec![input],
            vec![span],
            "deadbeef".into(),
            SourceId::new(),
            WorkspaceId::new(),
            Utc::now(),
        );
        assert_eq!(w.spans[0].start, 10);
        assert_eq!(w.spans[0].end, 30);
        assert_eq!(w.spans[0].file_blake3, "hash");
        assert_eq!(w.content_blake3, "deadbeef");
        assert_eq!(w.symbol.as_deref(), Some("test"));
    }

    #[test]
    fn scene_change_threshold_is_five_seconds() {
        assert_eq!(SCENE_CHANGE_GAP_SECS, 5.0);
    }

    #[test]
    fn keyframe_cap_bounds_witness_growth() {
        // A workspace cannot ship more than MAX_KEYFRAME_WITNESSES
        // distinct video::keyframe@v1 witnesses per file. The cap
        // exists so a 4-hour talk doesn't balloon the DB; verify it
        // remains the pinned value the rule catalog documents.
        assert_eq!(MAX_KEYFRAME_WITNESSES, 2000);
    }
}
