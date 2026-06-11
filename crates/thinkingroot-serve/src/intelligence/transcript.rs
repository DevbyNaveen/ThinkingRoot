//! §6 P2 — audio "claims with ears": turn a transcript (speaker- and
//! time-segmented) into a speaker-stamped document that flows through the
//! EXISTING extraction pipeline, with audio provenance.
//!
//! The on-box ASR (sherpa-onnx whisper + diarization) is a heavy C++ FFI
//! dependency we defer on the constrained CPU VM; like §6 images (which use the
//! customer's vision LLM, zero new models), audio ingest is **ASR-pluggable**:
//! the caller supplies the transcript (their Whisper, a meeting tool, etc.) and
//! we own the cognition — speaker-stamped extraction + provenance back to the
//! exact audio span. This module is the pure, testable text-shaping core.

/// One transcript segment (a contiguous utterance).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TranscriptSegment {
    pub text: String,
    #[serde(default)]
    pub speaker: Option<String>,
    /// Seconds from the start of the audio.
    #[serde(default)]
    pub t_start: Option<f64>,
    #[serde(default)]
    pub t_end: Option<f64>,
}

fn mmss(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Render segments into a speaker-stamped, time-stamped document. Each line is
/// `[Speaker mm:ss-mm:ss] text`, so the extractor attributes claims to the
/// speaker and the timestamps survive as inline provenance. Blank-text segments
/// are skipped (honest: no fabricated lines). Empty input → empty string.
pub fn format_transcript(segments: &[TranscriptSegment]) -> String {
    let mut out = String::new();
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let mut tag = String::new();
        if let Some(sp) = seg.speaker.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            tag.push_str(sp);
        }
        match (seg.t_start, seg.t_end) {
            (Some(a), Some(b)) => {
                if !tag.is_empty() {
                    tag.push(' ');
                }
                tag.push_str(&format!("{}-{}", mmss(a), mmss(b)));
            }
            (Some(a), None) => {
                if !tag.is_empty() {
                    tag.push(' ');
                }
                tag.push_str(&mmss(a));
            }
            _ => {}
        }
        if tag.is_empty() {
            out.push_str(text);
        } else {
            out.push('[');
            out.push_str(&tag);
            out.push_str("] ");
            out.push_str(text);
        }
        out.push('\n');
    }
    out
}

/// Structured, queryable per-segment provenance URI for an audio claim:
/// `audio://<sha>?t_start=..&t_end=..&speaker=..` (only present fields are
/// included). So every claim extracted from a segment traces to its exact
/// audio span + speaker — supersession and citation work at the utterance
/// level, not just the whole file. The speaker value is kept URI-safe and
/// queryable (spaces → `_`; reserved `&=?#/` → `-`). Pure + testable.
pub fn segment_source_uri(audio_sha: &str, seg: &TranscriptSegment) -> String {
    let mut params: Vec<String> = Vec::new();
    if let Some(a) = seg.t_start {
        params.push(format!("t_start={a:.2}"));
    }
    if let Some(b) = seg.t_end {
        params.push(format!("t_end={b:.2}"));
    }
    if let Some(sp) = seg.speaker.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let safe: String = sp
            .chars()
            .map(|c| match c {
                ' ' => '_',
                '&' | '=' | '?' | '#' | '/' => '-',
                c => c,
            })
            .collect();
        params.push(format!("speaker={safe}"));
    }
    if params.is_empty() {
        format!("audio://{audio_sha}")
    } else {
        format!("audio://{audio_sha}?{}", params.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str, speaker: Option<&str>, a: Option<f64>, b: Option<f64>) -> TranscriptSegment {
        TranscriptSegment {
            text: text.into(),
            speaker: speaker.map(|s| s.into()),
            t_start: a,
            t_end: b,
        }
    }

    #[test]
    fn stamps_speaker_and_time() {
        let doc = format_transcript(&[
            seg("Let's ship the launch on Friday.", Some("Alice"), Some(0.0), Some(4.0)),
            seg("Agreed, I'll prep the demo.", Some("Bob"), Some(4.0), Some(7.0)),
        ]);
        assert_eq!(
            doc,
            "[Alice 00:00-00:04] Let's ship the launch on Friday.\n\
             [Bob 00:04-00:07] Agreed, I'll prep the demo.\n"
        );
    }

    #[test]
    fn tolerates_missing_speaker_or_time() {
        assert_eq!(format_transcript(&[seg("hello", None, None, None)]), "hello\n");
        assert_eq!(
            format_transcript(&[seg("hi", Some("S1"), None, None)]),
            "[S1] hi\n"
        );
        assert_eq!(
            format_transcript(&[seg("hi", None, Some(65.0), None)]),
            "[01:05] hi\n"
        );
    }

    #[test]
    fn skips_blank_segments_and_empty_input() {
        assert_eq!(format_transcript(&[]), "");
        assert_eq!(format_transcript(&[seg("   ", Some("X"), Some(1.0), Some(2.0))]), "");
    }

    #[test]
    fn segment_source_uri_is_structured_and_queryable() {
        let s = seg("x", Some("Alice"), Some(12.5), Some(18.0));
        assert_eq!(
            segment_source_uri("abc123", &s),
            "audio://abc123?t_start=12.50&t_end=18.00&speaker=Alice"
        );
        // missing fields drop out; bare file anchor when nothing structured
        assert_eq!(
            segment_source_uri("abc123", &seg("x", None, None, None)),
            "audio://abc123"
        );
        assert_eq!(
            segment_source_uri("abc123", &seg("x", Some("Dr. O'Neil & Co"), Some(1.0), None)),
            "audio://abc123?t_start=1.00&speaker=Dr._O'Neil_-_Co"
        );
    }
}
