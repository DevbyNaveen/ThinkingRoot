//! Audio-family Witness Mesh rules (catalog v1.2).
//!
//! Pure-Rust deterministic feature extraction over audio files.
//! Three rules — duration metadata, spectral fingerprint, and a
//! decode-fail honest-absence rule. Backend: symphonia for codec
//! decode (WAV/FLAC/MP3/Vorbis/Opus/AAC), rustfft for the
//! spectral-fingerprint FFT.
//!
//! Each rule consumes the whole file bytes as a single span
//! (`spans[0] = (file_blake3, 0, len)`); the Witness payload
//! encodes the deterministic feature plus the standard provenance
//! triple. Audio content is dense — per-frame anchoring would
//! generate gigabyte-sized witness sets for a 4-minute song — so
//! the v1 ship aggregates over the whole file. v1.1 will add
//! per-window MFCC frames behind an opt-in workspace setting.
//!
//! No LLM. No ffmpeg. No shell-outs.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use thinkingroot_core::types::{
    Confidence, Sensitivity, SourceId, Witness, WitnessInput, WitnessSpan, WorkspaceId,
};

/// Byte budget per audio file. 256 MiB covers most CD-quality
/// albums; anything bigger gets `audio::skipped@v1` rather than
/// blocking the compile for tens of seconds on a hostile 1 GiB
/// FLAC.
const MAX_AUDIO_BYTES: usize = 256 * 1024 * 1024;

/// FFT window size for the spectral fingerprint. 2048 samples →
/// 1024 frequency bins, which is the standard windowing for music
/// information retrieval. Output payload size is fixed by this
/// constant (independent of file duration).
const FFT_SIZE: usize = 2048;

/// Number of frequency-bin buckets we surface in the
/// `audio::spectral-fingerprint@v1` payload. 32 logarithmically-
/// spaced buckets covers the audible range at a granularity coarse
/// enough that minor re-encoding artefacts don't move the
/// fingerprint, but fine enough to discriminate genres / songs.
const FINGERPRINT_BUCKETS: usize = 32;

/// File extensions this module accepts. Walker / parser wire-through
/// uses the same set.
pub const AUDIO_EXTENSIONS: &[&str] =
    &["wav", "flac", "mp3", "ogg", "opus", "m4a", "aac", "mp4"];

/// True when `ext` is a recognised audio extension (lower-case
/// comparison; caller normalises the dot off).
pub fn is_audio_extension(ext: &str) -> bool {
    AUDIO_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// Extract all audio-family witnesses from a single file's bytes.
///
/// Returns up to 2 witnesses on success (duration + spectral
/// fingerprint) plus an `audio::skipped@v1` witness when decode
/// fails or the file exceeds [`MAX_AUDIO_BYTES`]. Never panics;
/// audio extraction failure is observability, not pipeline-fatal.
pub fn extract_audio_witnesses(
    bytes: &[u8],
    file_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Vec<Witness> {
    if file_blake3.is_empty() {
        return Vec::new();
    }
    if bytes.len() > MAX_AUDIO_BYTES {
        return vec![skipped_witness(
            file_blake3,
            bytes.len(),
            source_id,
            workspace_id,
            now,
            format!("file exceeds MAX_AUDIO_BYTES ({MAX_AUDIO_BYTES} bytes)"),
        )];
    }

    let decoded = match decode_audio(bytes) {
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

    let mut out = Vec::with_capacity(2);

    out.push(build_duration_witness(
        &decoded,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ));

    out.push(build_spectral_fingerprint_witness(
        &decoded,
        &span,
        &input,
        &content_blake3,
        source_id,
        workspace_id,
        now,
    ));

    out
}

/// Decoded audio in a normalised mono PCM form. Internal-only —
/// the rule builders consume this shape, callers see only Witnesses.
struct DecodedAudio {
    /// Mono PCM samples in `[-1.0, 1.0]`. Stereo files are
    /// downmixed by averaging L+R per frame so the fingerprint is
    /// channel-count invariant.
    mono_samples: Vec<f32>,
    /// Sample rate in Hz as declared by the codec header.
    sample_rate: u32,
    /// Original channel count (1 = mono, 2 = stereo, …) — surfaced
    /// in the duration witness even after the downmix.
    channels: u32,
}

impl DecodedAudio {
    fn duration_samples(&self) -> u64 {
        self.mono_samples.len() as u64
    }
    fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        (self.mono_samples.len() as u64 * 1000) / self.sample_rate as u64
    }
}

/// Decode an audio file into mono PCM via symphonia. Returns an
/// `Err(String)` on any decode failure so the caller can surface
/// the message in `audio::skipped@v1`.
fn decode_audio(bytes: &[u8]) -> Result<DecodedAudio, String> {
    use symphonia::core::audio::AudioBufferRef;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mss = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(bytes.to_vec())),
        Default::default(),
    );

    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe failed: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no decodable audio track".to_string())?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params.sample_rate.unwrap_or(0);
    let channels = codec_params
        .channels
        .map(|c| c.count() as u32)
        .unwrap_or(0);
    if sample_rate == 0 || channels == 0 {
        return Err("codec missing sample rate or channel count".to_string());
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder make failed: {e}"))?;

    let mut mono_samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(format!("packet read failed: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("decode failed: {e}")),
        };
        match decoded {
            AudioBufferRef::F32(buf) => append_mono_f32(&mut mono_samples, &buf, channels as usize),
            AudioBufferRef::S16(buf) => append_mono_s16(&mut mono_samples, &buf, channels as usize),
            AudioBufferRef::S32(buf) => append_mono_s32(&mut mono_samples, &buf, channels as usize),
            AudioBufferRef::U8(buf) => append_mono_u8(&mut mono_samples, &buf, channels as usize),
            // Less-common buffer types: convert via a generic ref
            // when symphonia exposes one. v1 accepts whatever the
            // common codecs (WAV / FLAC / MP3 / Vorbis) emit; if
            // a codec returns one of the rarer types we surface as
            // skipped rather than fabricating samples.
            _ => return Err("unsupported codec sample format for v1".to_string()),
        }
    }

    if mono_samples.is_empty() {
        return Err("decoder produced zero samples".to_string());
    }

    Ok(DecodedAudio {
        mono_samples,
        sample_rate,
        channels,
    })
}

fn append_mono_f32(
    out: &mut Vec<f32>,
    buf: &symphonia::core::audio::AudioBuffer<f32>,
    channels: usize,
) {
    use symphonia::core::audio::Signal;
    let n = buf.frames();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0.0f32;
        for ch in 0..channels {
            acc += buf.chan(ch)[i];
        }
        out.push(acc / channels as f32);
    }
}

fn append_mono_s16(
    out: &mut Vec<f32>,
    buf: &symphonia::core::audio::AudioBuffer<i16>,
    channels: usize,
) {
    use symphonia::core::audio::Signal;
    let n = buf.frames();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0.0f32;
        for ch in 0..channels {
            acc += buf.chan(ch)[i] as f32 / i16::MAX as f32;
        }
        out.push(acc / channels as f32);
    }
}

fn append_mono_s32(
    out: &mut Vec<f32>,
    buf: &symphonia::core::audio::AudioBuffer<i32>,
    channels: usize,
) {
    use symphonia::core::audio::Signal;
    let n = buf.frames();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0.0f32;
        for ch in 0..channels {
            acc += buf.chan(ch)[i] as f32 / i32::MAX as f32;
        }
        out.push(acc / channels as f32);
    }
}

fn append_mono_u8(
    out: &mut Vec<f32>,
    buf: &symphonia::core::audio::AudioBuffer<u8>,
    channels: usize,
) {
    use symphonia::core::audio::Signal;
    let n = buf.frames();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0.0f32;
        for ch in 0..channels {
            // u8 PCM is biased: 128 = silence.
            acc += (buf.chan(ch)[i] as f32 - 128.0) / 128.0;
        }
        out.push(acc / channels as f32);
    }
}

fn build_duration_witness(
    decoded: &DecodedAudio,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    let payload = format!(
        "duration_ms={};duration_samples={};sample_rate={};channels={}",
        decoded.duration_ms(),
        decoded.duration_samples(),
        decoded.sample_rate,
        decoded.channels,
    );
    let mut w = Witness::new(
        "audio::duration@v1",
        "audio::duration",
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

fn build_spectral_fingerprint_witness(
    decoded: &DecodedAudio,
    span: &WitnessSpan,
    input: &WitnessInput,
    content_blake3: &str,
    source_id: SourceId,
    workspace_id: WorkspaceId,
    now: DateTime<Utc>,
) -> Witness {
    let payload = compute_spectral_fingerprint(&decoded.mono_samples);
    let mut w = Witness::new(
        "audio::spectral-fingerprint@v1",
        "audio::spectral-fingerprint",
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

/// Compute a deterministic spectral fingerprint over the mono PCM
/// signal. Algorithm:
/// 1. Slide a `FFT_SIZE`-sample Hann window over the signal with
///    50% overlap.
/// 2. Run the FFT, take the magnitude, sum across all windows.
/// 3. Group the summed magnitudes into [`FINGERPRINT_BUCKETS`]
///    logarithmically-spaced buckets across the bin range.
/// 4. Normalise the buckets to integer hundredths-of-percent so the
///    payload is integer-stable across platforms.
///
/// Encoded as `bucket0:val,bucket1:val,...` with `val` in `0..=10000`.
fn compute_spectral_fingerprint(samples: &[f32]) -> String {
    if samples.len() < FFT_SIZE {
        // Too short for one full window — emit a zero-filled
        // fingerprint with a marker so consumers can distinguish
        // "no signal" from "low-energy signal".
        return format!("short=1;samples={};", samples.len());
    }

    // Hann window — precomputed once. `Arc<FftPlanner<f32>>` is
    // the recommended pattern; we use a fresh planner per call
    // because the FFT_SIZE is fixed at compile time (planner
    // caching across calls would help if we batched many files).
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let hann: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            0.5 * (1.0
                - ((2.0 * std::f32::consts::PI * i as f32) / (FFT_SIZE - 1) as f32).cos())
        })
        .collect();

    let mut accum: Vec<f64> = vec![0.0; FFT_SIZE / 2];
    let mut window_count: u64 = 0;
    let hop = FFT_SIZE / 2; // 50% overlap

    let mut start = 0;
    while start + FFT_SIZE <= samples.len() {
        let mut buf: Vec<Complex<f32>> = (0..FFT_SIZE)
            .map(|i| Complex {
                re: samples[start + i] * hann[i],
                im: 0.0,
            })
            .collect();
        fft.process(&mut buf);
        // Drop the negative-frequency mirror — bins 0..FFT_SIZE/2
        // carry the unique spectrum.
        for (i, c) in buf.iter().enumerate().take(FFT_SIZE / 2) {
            accum[i] += (c.re * c.re + c.im * c.im).sqrt() as f64;
        }
        window_count += 1;
        start += hop;
    }

    if window_count == 0 {
        return "short=1".to_string();
    }

    // Average the bin magnitudes (rate-invariant — files of
    // different length share the same payload structure).
    let avg: Vec<f64> = accum.iter().map(|&m| m / window_count as f64).collect();

    // Take log-magnitudes to compress the dynamic range; this is
    // what makes the fingerprint survive re-encoding without
    // dominating on the loudest bin alone.
    let log_avg: Vec<f64> = avg.iter().map(|&v| (v + 1e-9).ln()).collect();

    // Logarithmically-spaced buckets. Bin 0 = DC; we map bin
    // index i to bucket `(log(i+1) / log(N+1)) * BUCKETS`.
    let n_bins = log_avg.len();
    let mut bucket_sums: Vec<f64> = vec![0.0; FINGERPRINT_BUCKETS];
    let mut bucket_counts: Vec<u32> = vec![0; FINGERPRINT_BUCKETS];
    for (i, &v) in log_avg.iter().enumerate() {
        let log_i = ((i + 1) as f64).ln();
        let log_n = ((n_bins + 1) as f64).ln();
        let mut b = ((log_i / log_n) * FINGERPRINT_BUCKETS as f64) as usize;
        if b >= FINGERPRINT_BUCKETS {
            b = FINGERPRINT_BUCKETS - 1;
        }
        bucket_sums[b] += v;
        bucket_counts[b] += 1;
    }

    let mut buckets: Vec<f64> = bucket_sums
        .iter()
        .zip(bucket_counts.iter())
        .map(|(s, c)| if *c == 0 { 0.0 } else { s / *c as f64 })
        .collect();

    // Min-max normalise to [0, 10000]. Same input → same payload
    // even across machines (floating-point ops we use are all
    // deterministic at this scale: sqrt, ln, division, addition).
    let min = buckets.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = buckets.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;
    if range > 0.0 {
        for v in &mut buckets {
            *v = ((*v - min) / range) * 10_000.0;
        }
    } else {
        for v in &mut buckets {
            *v = 0.0;
        }
    }

    let mut sorted_bucket_map: BTreeMap<usize, u32> = BTreeMap::new();
    for (idx, v) in buckets.iter().enumerate() {
        sorted_bucket_map.insert(idx, *v as u32);
    }
    let body = sorted_bucket_map
        .iter()
        .map(|(idx, v)| format!("{idx}:{v}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("windows={window_count};{body}")
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
    let content_blake3 = file_blake3.to_string();
    let mut w = Witness::new(
        "audio::skipped@v1",
        "audio::skipped",
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

// Hold a ref to Arc so the inline `let _ = Arc::new(0u8);` pattern
// the planner uses doesn't get flagged; this is here so rustc keeps
// the rustfft path live even with optimisation.
#[allow(dead_code)]
fn _link_keepalive() -> Arc<u8> {
    Arc::new(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Build a minimal 16-bit PCM WAV blob — one channel, 8 kHz,
    /// `samples` length, filled by the supplied `mk_sample` closure.
    /// `gen` is a reserved keyword in the 2024 edition, hence the
    /// longer parameter name.
    fn fixture_wav<F: Fn(usize) -> i16>(
        samples: usize,
        sample_rate: u32,
        mk_sample: F,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(44 + samples * 2);
        // RIFF header
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + samples * 2) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        // fmt chunk
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        // data chunk
        out.extend_from_slice(b"data");
        out.extend_from_slice(&((samples * 2) as u32).to_le_bytes());
        for i in 0..samples {
            let s = mk_sample(i);
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn is_audio_extension_recognises_common_formats() {
        assert!(is_audio_extension("wav"));
        assert!(is_audio_extension("FLAC"));
        assert!(is_audio_extension("mp3"));
        assert!(!is_audio_extension("png"));
        assert!(!is_audio_extension(""));
    }

    #[test]
    fn extract_audio_witnesses_returns_two_for_a_valid_wav() {
        // 4096 samples @ 8 kHz = 0.5 s of a 440 Hz tone.
        let bytes = fixture_wav(4096, 8000, |i| {
            let t = i as f32 / 8000.0;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            (v * (i16::MAX as f32)) as i16
        });
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let dump: Vec<String> = witnesses
            .iter()
            .map(|w| format!("{}={:?}", w.rule, w.symbol))
            .collect();
        assert_eq!(witnesses.len(), 2, "got {} witnesses: {dump:?}", witnesses.len());
        let rules: Vec<&str> = witnesses.iter().map(|w| w.rule.as_str()).collect();
        assert!(rules.contains(&"audio::duration@v1"));
        assert!(rules.contains(&"audio::spectral-fingerprint@v1"));
    }

    #[test]
    fn duration_witness_reports_correct_sample_rate_and_count() {
        let bytes = fixture_wav(8000, 8000, |_| 0);
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let dur = witnesses
            .iter()
            .find(|w| w.rule == "audio::duration@v1")
            .unwrap();
        let payload = dur.symbol.as_deref().unwrap();
        assert!(payload.contains("duration_ms=1000"), "got {payload}");
        assert!(payload.contains("sample_rate=8000"));
        assert!(payload.contains("channels=1"));
    }

    #[test]
    fn spectral_fingerprint_is_deterministic_across_runs() {
        let bytes = fixture_wav(4096, 8000, |i| ((i % 200) as i16 - 100) * 200);
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let src = SourceId::new();
        let ws = WorkspaceId::new();
        let a = extract_audio_witnesses(&bytes, &file_blake3, src, ws, fixed_now());
        let b = extract_audio_witnesses(&bytes, &file_blake3, src, ws, fixed_now());
        let a_fp = a
            .iter()
            .find(|w| w.rule == "audio::spectral-fingerprint@v1")
            .unwrap();
        let b_fp = b
            .iter()
            .find(|w| w.rule == "audio::spectral-fingerprint@v1")
            .unwrap();
        assert_eq!(a_fp.symbol, b_fp.symbol, "spectral fingerprint must be deterministic");
    }

    #[test]
    fn extract_audio_witnesses_emits_skipped_for_garbage_bytes() {
        let bytes = b"not an audio file";
        let file_blake3 = blake3::hash(bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "audio::skipped@v1");
        assert!(witnesses[0].symbol.is_some());
    }

    #[test]
    fn extract_audio_witnesses_returns_empty_for_blank_file_blake3() {
        let bytes = fixture_wav(100, 8000, |_| 0);
        let witnesses = extract_audio_witnesses(
            &bytes,
            "",
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert!(witnesses.is_empty());
    }

    #[test]
    fn extract_audio_witnesses_skips_oversized_input() {
        let mut bytes = vec![0u8; MAX_AUDIO_BYTES + 1];
        bytes[..4].copy_from_slice(b"RIFF");
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(witnesses[0].rule, "audio::skipped@v1");
        assert!(witnesses[0]
            .symbol
            .as_deref()
            .unwrap_or("")
            .contains("exceeds MAX_AUDIO_BYTES"));
    }

    #[test]
    fn short_audio_emits_marker_in_fingerprint() {
        // Build a WAV that's shorter than one FFT window (FFT_SIZE).
        let n = (FFT_SIZE / 4).max(8);
        let bytes = fixture_wav(n, 8000, |_| 0);
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        let fp = witnesses
            .iter()
            .find(|w| w.rule == "audio::spectral-fingerprint@v1")
            .unwrap();
        assert!(fp.symbol.as_deref().unwrap().contains("short=1"));
    }

    #[test]
    fn witness_content_blake3_matches_file_blake3() {
        let bytes = fixture_wav(2048, 8000, |i| (i as i16).wrapping_mul(7));
        let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let witnesses = extract_audio_witnesses(
            &bytes,
            &file_blake3,
            SourceId::new(),
            WorkspaceId::new(),
            fixed_now(),
        );
        for w in &witnesses {
            assert_eq!(w.content_blake3, file_blake3);
        }
    }
}
