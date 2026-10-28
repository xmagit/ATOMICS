//! Voice input: recordings as WAV, and transcription through an OpenAI-compatible
//! `/audio/transcriptions` endpoint or an audio-capable chat model on OpenRouter.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::config::{VoiceConfig, VoiceProvider};

/// Recordings are sent at this rate when the microphone records faster; plenty for speech.
pub const SPEECH_RATE: u32 = 16_000;

const TIMEOUT: Duration = Duration::from_secs(180);

const INSTRUCTION: &str = "Transcribe this audio. Reply with only the verbatim transcript: no quotes, labels, \
translation or commentary. If nothing is said, reply with nothing.";

/// Lowers the sample rate to `to` by averaging the input under each output sample.
/// Audio at or below `to` keeps its rate.
pub fn downsample(samples: &[f32], from: u32, to: u32) -> (Vec<f32>, u32) {
    if from <= to || to == 0 {
        return (samples.to_vec(), from);
    }
    let (from_rate, to_rate) = (from as u64, to as u64);
    let len = (samples.len() as u64 * to_rate / from_rate) as usize;
    let out = (0..len as u64)
        .map(|i| {
            let start = (i * from_rate / to_rate) as usize;
            let end = (((i + 1) * from_rate / to_rate) as usize).clamp(start + 1, samples.len());
            samples[start..end].iter().sum::<f32>() / (end - start) as f32
        })
        .collect();
    (out, to)
}

/// True when the loudest sample is below -80 dBFS: what a muted or blocked microphone records.
pub fn is_silent(samples: &[f32]) -> bool {
    samples.iter().all(|s| s.abs() < 1e-4)
}

/// Mono 16-bit PCM WAV.
pub fn wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes());
    }
    out
}

/// The `multipart/form-data` body of a transcription request.
pub fn multipart(boundary: &str, model: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// The chat completion that asks an OpenRouter model for the transcript of `wav`.
pub fn openrouter_request(model: &str, wav: &[u8]) -> Value {
    let audio = base64::engine::general_purpose::STANDARD.encode(wav);
    json!({
        "model": model,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": INSTRUCTION },
                { "type": "input_audio", "input_audio": { "data": audio, "format": "wav" } },
            ],
        }],
    })
}

/// The transcript in an `/audio/transcriptions` response.
pub fn parse_transcription(status: u16, body: &str) -> Result<String> {
    let value = parse_response(status, body)?;
    match value["text"].as_str() {
        Some(text) => Ok(text.trim().to_string()),
        None => bail!("unexpected response: {}", excerpt(body)),
    }
}

/// The transcript in an OpenRouter chat completion. Content comes as a string or as parts.
pub fn parse_completion(status: u16, body: &str) -> Result<String> {
    let value = parse_response(status, body)?;
    let content = &value["choices"][0]["message"]["content"];
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        Value::Null if value["choices"][0].is_object() => String::new(),
        _ => bail!("unexpected response: {}", excerpt(body)),
    };
    Ok(text.trim().to_string())
}

/// The JSON of a response, or the API's reason when it failed.
fn parse_response(status: u16, body: &str) -> Result<Value> {
    let value: Option<Value> = serde_json::from_str(body).ok();
    let reason = value.as_ref().and_then(|v| match &v["error"] {
        Value::String(message) => Some(message.clone()),
        Value::Object(error) => error.get("message").and_then(Value::as_str).map(str::to_string),
        _ => None,
    });
    match (reason, value) {
        (Some(reason), _) if (200..300).contains(&status) => bail!("{reason}"),
        (Some(reason), _) => bail!("HTTP {status}: {reason}"),
        (None, _) if !(200..300).contains(&status) => bail!("HTTP {status}: {}", excerpt(body)),
        (None, Some(value)) => Ok(value),
        (None, None) => bail!("unexpected response: {}", excerpt(body)),
    }
}

fn excerpt(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &body[..at]),
        None => body.to_string(),
    }
}

/// Sends a WAV recording to the configured provider and returns the transcript. Blocks;
/// errors never contain `key`.
pub fn transcribe(config: &VoiceConfig, key: &str, wav: &[u8]) -> Result<String> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).http_status_as_error(false).build().into();
    let auth = format!("Bearer {key}");
    let result = match config.provider {
        VoiceProvider::Openai => {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            let boundary = format!("cox-voice-{nanos:x}");
            agent
                .post(format!("{}/audio/transcriptions", config.base_url()))
                .header("Authorization", &auth)
                .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
                .send(multipart(&boundary, config.model(), wav))
        }
        VoiceProvider::Openrouter => agent
            .post(format!("{}/chat/completions", config.base_url()))
            .header("Authorization", &auth)
            .header("Content-Type", "application/json")
            .send(openrouter_request(config.model(), wav).to_string()),
    };
    let redact = |err: anyhow::Error| match key.len() {
        0..8 => err,
        _ => anyhow!(format!("{err:#}").replace(key, "***")),
    };
    let mut response = result.map_err(|err| redact(anyhow!("request to {} failed: {err}", config.base_url())))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().map_err(|err| redact(anyhow!("reading the response: {err}")))?;
    match config.provider {
        VoiceProvider::Openai => parse_transcription(status, &body),
        VoiceProvider::Openrouter => parse_completion(status, &body),
    }
    .map_err(redact)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn wav_header_and_samples() {
        let bytes = wav(&[0.0, 1.0, -1.0, 2.0], 16_000);
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        let samples: Vec<i16> = bytes[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(samples, [0, i16::MAX, -i16::MAX, i16::MAX]);
    }

    #[test]
    fn downsampling_averages_and_keeps_low_rates() {
        let (out, rate) = downsample(&[0.0, 0.3, 0.6, 1.0, 1.0, 1.0, 0.5], 48_000, 16_000);
        assert_eq!(rate, 16_000);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6 && (out[1] - 1.0).abs() < 1e-6);
        let (out, rate) = downsample(&vec![0.5; 44_100], 44_100, 16_000);
        assert_eq!((out.len(), rate), (16_000, 16_000));
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
        assert_eq!(downsample(&[0.1, 0.2], 8_000, 16_000), (vec![0.1, 0.2], 8_000));
        assert!(is_silent(&[0.0, 5e-5]) && !is_silent(&[0.0, 0.01]));
    }

    #[test]
    fn multipart_body() {
        let body = multipart("XYZ", "whisper-1", b"RIFFdata");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.starts_with("--XYZ\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--XYZ\r\n")
        );
        assert!(text.contains("name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFFdata\r\n"));
        assert!(text.ends_with("\r\n--XYZ--\r\n"));
    }

    #[test]
    fn openrouter_request_json() {
        let request = openrouter_request("google/gemini-2.5-flash", b"RIFF");
        assert_eq!(request["model"], "google/gemini-2.5-flash");
        let content = &request["messages"][0]["content"];
        assert_eq!(request["messages"][0]["role"], "user");
        assert!(content[0]["text"].as_str().unwrap().contains("only the verbatim transcript"));
        assert_eq!(content[1]["type"], "input_audio");
        assert_eq!(content[1]["input_audio"]["format"], "wav");
        assert_eq!(content[1]["input_audio"]["data"], "UklGRg==");
    }

    #[test]
    fn response_parsing() {
        assert_eq!(parse_transcription(200, r#"{"text":" Add a test. "}"#).unwrap(), "Add a test.");
        let err = parse_transcription(401, r#"{"error":{"message":"Incorrect API key","type":"auth"}}"#);
        assert_eq!(err.unwrap_err().to_string(), "HTTP 401: Incorrect API key");
        assert_eq!(parse_transcription(502, "Bad gateway").unwrap_err().to_string(), "HTTP 502: Bad gateway");
        assert!(parse_transcription(200, "{}").unwrap_err().to_string().starts_with("unexpected response"));

        let ok = r#"{"choices":[{"message":{"role":"assistant","content":"Ship it.\n"}}]}"#;
        assert_eq!(parse_completion(200, ok).unwrap(), "Ship it.");
        let parts =
            r#"{"choices":[{"message":{"content":[{"type":"text","text":"Ship "},{"type":"text","text":"it"}]}}]}"#;
        assert_eq!(parse_completion(200, parts).unwrap(), "Ship it");
        assert_eq!(parse_completion(200, r#"{"choices":[{"message":{"content":null}}]}"#).unwrap(), "");
        let upstream = r#"{"error":{"code":502,"message":"Provider returned error"}}"#;
        assert_eq!(parse_completion(200, upstream).unwrap_err().to_string(), "Provider returned error");
        assert_eq!(
            parse_completion(402, r#"{"error":"Insufficient credits"}"#).unwrap_err().to_string(),
            "HTTP 402: Insufficient credits"
        );
    }

    /// Serves one request on a local port: returns the base URL and the raw request (head, body).
    fn mock_server(status: &str, response: &str) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let (status, response) = (status.to_string(), response.to_string());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                head.push_str(&line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let length = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
            let _ = tx.send((head, body));
        });
        (url, rx)
    }

    #[test]
    fn transcribes_through_an_openai_compatible_server() {
        let (url, rx) = mock_server("200 OK", r#"{"text":"Split the task in two."}"#);
        let config = VoiceConfig { base_url: Some(format!("{url}/v1/")), ..VoiceConfig::default() };
        let audio = wav(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(transcribe(&config, "sk-test-123456", &audio).unwrap(), "Split the task in two.");
        let (head, body) = rx.recv().unwrap();
        assert!(head.starts_with("POST /v1/audio/transcriptions HTTP/1.1\r\n"), "{head}");
        assert!(head.to_ascii_lowercase().contains("authorization: bearer sk-test-123456"));
        let boundary = head
            .lines()
            .find_map(|l| l.split_once("multipart/form-data; boundary=").map(|(_, b)| b.trim().to_string()))
            .expect("multipart content type");
        assert_eq!(body, multipart(&boundary, "gpt-4o-mini-transcribe", &audio));
    }

    #[test]
    fn transcribes_through_openrouter() {
        let (url, rx) = mock_server("200 OK", r#"{"choices":[{"message":{"content":"Merge it."}}]}"#);
        let config = VoiceConfig {
            provider: VoiceProvider::Openrouter,
            base_url: Some(format!("{url}/api/v1")),
            ..VoiceConfig::default()
        };
        let audio = wav(&[0.25; 8], 16_000);
        assert_eq!(transcribe(&config, "sk-or-test-123456", &audio).unwrap(), "Merge it.");
        let (head, body) = rx.recv().unwrap();
        assert!(head.starts_with("POST /api/v1/chat/completions HTTP/1.1\r\n"), "{head}");
        assert!(head.to_ascii_lowercase().contains("content-type: application/json"));
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request, openrouter_request("google/gemini-2.5-flash", &audio));
    }

    #[test]
    fn errors_carry_the_reason_but_never_the_key() {
        let key = "sk-secret-abcdef123456";
        let (url, _rx) = mock_server(
            "401 Unauthorized",
            &format!(r#"{{"error":{{"message":"Incorrect API key provided: {key}"}}}}"#),
        );
        let config = VoiceConfig { base_url: Some(url), ..VoiceConfig::default() };
        let err = format!("{:#}", transcribe(&config, key, &wav(&[0.1], 16_000)).unwrap_err());
        assert_eq!(err, "HTTP 401: Incorrect API key provided: ***");

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let config = VoiceConfig { base_url: Some(url.clone()), ..VoiceConfig::default() };
        let err = format!("{:#}", transcribe(&config, key, &wav(&[0.1], 16_000)).unwrap_err());
        assert!(err.starts_with(&format!("request to {url} failed")) && !err.contains(key), "{err}");
    }
}
