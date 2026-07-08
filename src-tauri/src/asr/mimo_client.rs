use std::path::Path;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use super::{AsrConfig, AsrError};

/// Hard cap for a single batch request. Normal transcriptions complete in a few
/// seconds; without a timeout a stalled connection would freeze the finalize
/// loop indefinitely, so fail loudly instead.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Xiaomi MiMo batch ASR client.
///
/// MiMo exposes an OpenAI chat-completions compatible endpoint
/// (`POST {base_url}/chat/completions`). The audio is uploaded inline as a
/// base64 `data:` URI inside an `input_audio` content part, and the transcript
/// is returned in `choices[0].message.content`.
pub struct MimoTranscriptionClient {
    config: AsrConfig,
    http: reqwest::Client,
}

/// The transcript is emitted as completion tokens; `mimo-v2.5-asr` accepts at
/// most 4096. The service default is only 2048, which silently truncates longer
/// recordings (`finish_reason: "length"`), so always request the ceiling.
const MAX_COMPLETION_TOKENS: u32 = 4096;

#[derive(Debug, Serialize)]
struct MimoAsrRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<MimoMessage>,
    asr_options: MimoAsrOptions,
}

#[derive(Debug, Serialize)]
struct MimoMessage {
    role: &'static str,
    content: Vec<MimoContentPart>,
}

#[derive(Debug, Serialize)]
struct MimoContentPart {
    #[serde(rename = "type")]
    part_type: &'static str,
    input_audio: MimoInputAudio,
}

#[derive(Debug, Serialize)]
struct MimoInputAudio {
    /// `data:{mime};base64,{payload}`
    data: String,
}

#[derive(Debug, Serialize)]
struct MimoAsrOptions {
    language: String,
}

#[derive(Debug, Deserialize)]
struct MimoAsrResponse {
    choices: Option<Vec<MimoChoice>>,
    error: Option<MimoError>,
}

#[derive(Debug, Deserialize)]
struct MimoChoice {
    message: Option<MimoResponseMessage>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MimoResponseMessage {
    /// MiMo returns a plain string here, but tolerate the array-of-parts shape
    /// used by some OpenAI-compatible gateways.
    content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MimoError {
    message: Option<String>,
}

struct PreparedAudio {
    mime_type: &'static str,
    bytes: Vec<u8>,
}

impl MimoTranscriptionClient {
    pub fn new(config: AsrConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { config, http }
    }

    pub async fn transcribe_file(&self, path: &Path) -> Result<String, AsrError> {
        if !self.config.is_valid() {
            return Err(AsrError::ConnectionFailed(
                "Invalid MiMo ASR configuration".to_string(),
            ));
        }

        let audio = prepare_audio(path)?;
        let language = normalize_language(&self.config.mimo_language);
        let endpoint = format!(
            "{}/chat/completions",
            self.config.mimo_base_url.trim_end_matches('/')
        );

        log::info!(
            "Starting MiMo transcription (model={}, language={}, file={}, bytes={}, mime={})",
            self.config.mimo_model,
            language,
            path.display(),
            audio.bytes.len(),
            audio.mime_type,
        );

        let data_uri = format!(
            "data:{};base64,{}",
            audio.mime_type,
            STANDARD.encode(&audio.bytes)
        );

        let request = MimoAsrRequest {
            model: self.config.mimo_model.clone(),
            max_tokens: MAX_COMPLETION_TOKENS,
            messages: vec![MimoMessage {
                role: "user",
                content: vec![MimoContentPart {
                    part_type: "input_audio",
                    input_audio: MimoInputAudio { data: data_uri },
                }],
            }],
            asr_options: MimoAsrOptions { language },
        };

        let sent_at = Instant::now();
        let response = self
            .http
            .post(endpoint)
            .bearer_auth(&self.config.mimo_api_key)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                AsrError::ConnectionFailed(format!(
                    "MiMo request failed after {:?}: {}",
                    sent_at.elapsed(),
                    e
                ))
            })?;

        let status = response.status();
        log::debug!(
            "MiMo response headers received (status={}, elapsed={:?})",
            status.as_u16(),
            sent_at.elapsed()
        );
        let body = response.bytes().await.map_err(|e| {
            AsrError::ConnectionFailed(format!(
                "Failed to read MiMo response after {:?}: {}",
                sent_at.elapsed(),
                e
            ))
        })?;
        log::info!(
            "MiMo response complete (status={}, bytes={}, elapsed={:?})",
            status.as_u16(),
            body.len(),
            sent_at.elapsed()
        );

        if !status.is_success() {
            return Err(AsrError::ServerError(format!(
                "MiMo HTTP {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }

        let parsed: MimoAsrResponse = serde_json::from_slice(&body)
            .map_err(|e| AsrError::ProtocolError(format!("Invalid MiMo response JSON: {}", e)))?;

        extract_text(parsed)
    }
}

fn prepare_audio(path: &Path) -> Result<PreparedAudio, AsrError> {
    // MiMo accepts only wav/mp3/mpeg and caps `input_audio.data` at 10 MB of
    // base64. 16 kHz mono WAV crosses that at ~4 min, so compress everything to
    // 16 kHz mono MP3 (~0.36 MB/min) to keep multi-minute recordings well under
    // the cap.
    match extension(path).as_deref() {
        Some("opus" | "ogg") => {
            let pcm = crate::asr::ogg_decoder::decode_ogg_opus_to_pcm16k(path)
                .map_err(AsrError::ProtocolError)?;
            if pcm.is_empty() {
                return Err(AsrError::ProtocolError("音频文件解码后为空".to_string()));
            }
            encode_pcm16_mono_16k(&pcm)
        }
        Some("wav") => {
            // Fallback path (opus artifact missing): the capture is typically
            // 48 kHz mono, so downmix/resample to 16 kHz mono before encoding.
            let bytes = std::fs::read(path).map_err(|e| {
                AsrError::ConnectionFailed(format!("Failed to read audio file: {}", e))
            })?;
            let pcm = wav_to_pcm16_mono_16k(&bytes)?;
            encode_pcm16_mono_16k(&pcm)
        }
        // Already an accepted compressed format — send as-is.
        Some("mp3" | "mpeg" | "mpga") => {
            let bytes = std::fs::read(path).map_err(|e| {
                AsrError::ConnectionFailed(format!("Failed to read audio file: {}", e))
            })?;
            Ok(PreparedAudio {
                mime_type: "audio/mpeg",
                bytes,
            })
        }
        // Unknown container — best effort, let the server validate it.
        _ => {
            let bytes = std::fs::read(path).map_err(|e| {
                AsrError::ConnectionFailed(format!("Failed to read audio file: {}", e))
            })?;
            Ok(PreparedAudio {
                mime_type: mime_type_for_path(path),
                bytes,
            })
        }
    }
}

/// Encode 16 kHz mono signed-16-bit-LE PCM into a MiMo-accepted container.
///
/// macOS/Linux use MP3 (~0.36 MB/min) so multi-minute recordings stay well under
/// MiMo's 10 MB input cap. Windows falls back to WAV: `mp3lame-sys` builds
/// libmp3lame via autotools, which is unreliable on the GitHub Actions Windows
/// runners, so the crate is excluded there entirely. WAV is ~5× larger, so
/// Windows recordings are effectively limited to ~5 min by the 10 MB cap.
#[cfg(not(target_os = "windows"))]
fn encode_pcm16_mono_16k(pcm: &[u8]) -> Result<PreparedAudio, AsrError> {
    Ok(PreparedAudio {
        mime_type: "audio/mpeg",
        bytes: pcm16_mono_16k_to_mp3(pcm)?,
    })
}

#[cfg(target_os = "windows")]
fn encode_pcm16_mono_16k(pcm: &[u8]) -> Result<PreparedAudio, AsrError> {
    Ok(PreparedAudio {
        mime_type: "audio/wav",
        bytes: pcm16_mono_16k_to_wav(pcm),
    })
}

/// Encode 16 kHz mono signed-16-bit-LE PCM to MP3 (48 kbps CBR — ample for
/// speech, ~0.36 MB/min).
#[cfg(not(target_os = "windows"))]
fn pcm16_mono_16k_to_mp3(pcm: &[u8]) -> Result<Vec<u8>, AsrError> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality};

    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    let mut builder = Builder::new()
        .ok_or_else(|| AsrError::ProtocolError("Failed to create MP3 encoder".to_string()))?;
    let mp3_err = |e: mp3lame_encoder::BuildError| {
        AsrError::ProtocolError(format!("MP3 encoder setup failed: {}", e))
    };
    builder.set_num_channels(1).map_err(mp3_err)?;
    builder.set_sample_rate(16_000).map_err(mp3_err)?;
    builder.set_brate(Bitrate::Kbps48).map_err(mp3_err)?;
    builder.set_quality(Quality::Good).map_err(mp3_err)?;
    let mut encoder = builder.build().map_err(mp3_err)?;

    // LAME writes into the Vec's spare capacity, so it MUST be reserved up front
    // — encoding into a zero-capacity buffer overruns memory (SIGSEGV).
    let mut mp3 = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(samples.len()));
    encoder
        .encode_to_vec(MonoPcm(&samples), &mut mp3)
        .map_err(|e| AsrError::ProtocolError(format!("MP3 encode failed: {}", e)))?;
    // flush needs room for the final frame (the crate documents ≥7200 bytes).
    mp3.reserve(7200);
    encoder
        .flush_to_vec::<FlushNoGap>(&mut mp3)
        .map_err(|e| AsrError::ProtocolError(format!("MP3 flush failed: {}", e)))?;
    Ok(mp3)
}

/// Wrap 16 kHz mono signed-16-bit-LE PCM in a canonical 44-byte WAV header
/// (Windows fallback where MP3 encoding is unavailable).
#[cfg(target_os = "windows")]
fn pcm16_mono_16k_to_wav(pcm: &[u8]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8);
    let block_align = CHANNELS * (BITS / 8);
    let data_len = pcm.len() as u32;

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&BITS.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Minimal RIFF/WAVE reader for 16-bit PCM; returns 16 kHz mono PCM
/// (signed-16-bit LE), downmixing and resampling as needed.
fn wav_to_pcm16_mono_16k(bytes: &[u8]) -> Result<Vec<u8>, AsrError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(AsrError::ProtocolError(
            "MiMo WAV fallback: not a RIFF/WAVE file".to_string(),
        ));
    }

    let mut channels: u16 = 1;
    let mut sample_rate: u32 = 16_000;
    let mut bits: u16 = 16;
    let mut data: Option<&[u8]> = None;

    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(chunk_size).min(bytes.len());
        match chunk_id {
            b"fmt " if body_end - body_start >= 16 => {
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate =
                    u32::from_le_bytes(bytes[body_start + 4..body_start + 8].try_into().unwrap());
                bits = u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }
        // Chunks are word-aligned: skip a pad byte after odd-sized bodies.
        pos = body_end + (chunk_size & 1);
    }

    if bits != 16 {
        return Err(AsrError::ProtocolError(format!(
            "MiMo WAV fallback: only 16-bit PCM is supported, got {}-bit",
            bits
        )));
    }
    let data = data.ok_or_else(|| {
        AsrError::ProtocolError("MiMo WAV fallback: missing data chunk".to_string())
    })?;

    let mono = if channels > 1 {
        crate::asr::audio_utils::downmix_to_mono(data, channels)
    } else {
        data.to_vec()
    };
    let pcm16k = if sample_rate != 16_000 {
        crate::asr::audio_utils::resample_to_16k(&mono, sample_rate)
    } else {
        mono
    };
    Ok(pcm16k)
}

fn extract_text(response: MimoAsrResponse) -> Result<String, AsrError> {
    if let Some(error) = response.error {
        return Err(AsrError::ServerError(format!(
            "MiMo error: {}",
            error.message.as_deref().unwrap_or("unknown error")
        )));
    }

    let choices = response.choices.unwrap_or_default();

    // The transcript is emitted as completion tokens; if the model hit the
    // 4096-token ceiling the transcript is cut off mid-recording. Surface this
    // rather than silently returning a partial result.
    if choices
        .iter()
        .any(|choice| choice.finish_reason.as_deref() == Some("length"))
    {
        log::warn!(
            "MiMo transcript truncated: hit the {}-token output ceiling. \
             The recording is likely too long for a single request.",
            MAX_COMPLETION_TOKENS
        );
    }

    let text = choices
        .into_iter()
        .filter_map(|choice| choice.message)
        .filter_map(|message| message.content)
        .map(|content| content_to_text(&content))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        Err(AsrError::ServerError(
            "MiMo returned an empty transcript".to_string(),
        ))
    } else {
        Ok(text)
    }
}

/// Flatten `message.content` into plain text. MiMo returns a string; tolerate the
/// `[{ "type": "text", "text": "..." }]` array form as well.
fn content_to_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn normalize_language(language: &str) -> String {
    let language = language.trim();
    if language.is_empty() {
        "auto".to_string()
    } else {
        language.to_string()
    }
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn mime_type_for_path(path: &Path) -> &'static str {
    match extension(path).as_deref() {
        Some("wav") => "audio/wav",
        Some("mp3" | "mpeg" | "mpga") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("ogg" | "opus") => "audio/ogg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        content_to_text, extract_text, normalize_language, wav_to_pcm16_mono_16k, MimoAsrResponse,
    };

    fn parse(body: &str) -> MimoAsrResponse {
        serde_json::from_str(body).unwrap()
    }

    fn wav_bytes(sample_rate: u32, channels: u16, pcm: &[u8]) -> Vec<u8> {
        let bits = 16u16;
        let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
        let block_align = channels * (bits / 8);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(pcm);
        wav
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn mp3_encoding_is_smaller_than_pcm_and_nonempty() {
        // 1s of 16 kHz mono PCM (a quiet sine-ish ramp is fine for a size check).
        let pcm: Vec<u8> = (0..16_000)
            .flat_map(|i| ((i as i16).wrapping_mul(7)).to_le_bytes())
            .collect();
        let mp3 = super::pcm16_mono_16k_to_mp3(&pcm).unwrap();
        assert!(!mp3.is_empty());
        assert!(mp3.len() < pcm.len(), "MP3 should be smaller than raw PCM");
    }

    #[test]
    fn wav_parser_downsamples_48k_mono_to_16k() {
        // 0.5s of 48 kHz mono => expect ~0.5s of 16 kHz mono (a third of samples).
        let pcm: Vec<u8> = vec![0u8; 48_000 /* samples */ * 2 /* bytes */];
        let wav = wav_bytes(48_000, 1, &pcm);
        let out = wav_to_pcm16_mono_16k(&wav).unwrap();
        let out_samples = out.len() / 2;
        assert!(
            (15_500..=16_500).contains(&out_samples),
            "expected ~16000 samples, got {}",
            out_samples
        );
    }

    #[test]
    fn wav_parser_rejects_non_pcm16() {
        let wav = wav_bytes(16_000, 1, &[0u8; 4]);
        // Corrupt bits-per-sample to 8.
        let mut broken = wav.clone();
        // fmt body starts at offset 20; bits-per-sample is 14 bytes in => offset 34.
        broken[34] = 8;
        broken[35] = 0;
        assert!(wav_to_pcm16_mono_16k(&broken).is_err());
    }

    #[test]
    fn extracts_transcript_from_string_content() {
        let response = parse(
            r#"{"choices":[{"message":{"role":"assistant","content":"你好，世界。"}}]}"#,
        );
        assert_eq!(extract_text(response).unwrap(), "你好，世界。");
    }

    #[test]
    fn extracts_transcript_from_array_content() {
        let content = serde_json::json!([
            {"type": "text", "text": "你好"},
            {"type": "text", "text": "，世界。"}
        ]);
        assert_eq!(content_to_text(&content), "你好，世界。");
    }

    #[test]
    fn surfaces_server_error() {
        let response = parse(r#"{"error":{"message":"bad request"}}"#);
        let err = extract_text(response).unwrap_err();
        assert!(err.to_string().contains("bad request"));
    }

    #[test]
    fn empty_transcript_is_an_error() {
        let response = parse(r#"{"choices":[{"message":{"content":"   "}}]}"#);
        assert!(extract_text(response).is_err());
    }

    #[test]
    fn blank_language_defaults_to_auto() {
        assert_eq!(normalize_language("  "), "auto");
        assert_eq!(normalize_language("zh"), "zh");
    }
}
