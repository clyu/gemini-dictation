//! A minimal Gemini Live API client that transcribes one push-to-talk recording per session.
//!
//! Automatic activity detection is disabled: the recording is framed by `activityStart` and
//! `activityEnd`, and the transcript comes from the input audio transcription of a transcription
//! model such as `gemini-3.5-transcribe-live`.
//!
//! See <https://ai.google.dev/api/live> for the protocol and
//! <https://ai.google.dev/gemini-api/docs/live-api/live-transcribe> for transcription.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

const ENDPOINT: &str = concat!(
    "wss://generativelanguage.googleapis.com/ws/",
    "google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent",
);

/// The sample rate of the audio sent to the Live API.
pub const INPUT_SAMPLE_RATE: u32 = 16_000;
const AUDIO_MIME_TYPE: &str = "audio/pcm;rate=16000";
/// Audio is sent in chunks of 100 ms.
const CHUNK_SAMPLES: usize = INPUT_SAMPLE_RATE as usize / 10;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Once the recording has ended, the session is closed when the server has been silent for this
/// long, in case it does not complete the turn.
const QUIET_TIMEOUT: Duration = Duration::from_secs(3);
/// Once the recording has ended, the session is closed after this long in any case.
const FINISH_TIMEOUT: Duration = Duration::from_secs(20);

pub struct SessionConfig {
    pub api_key: String,
    pub model: String,
    pub languages: Vec<String>,
    pub vocabulary: Vec<String>,
    /// `SMART` or `VERBATIM`.
    pub mode: &'static str,
}

pub enum Event {
    /// A piece of the final transcript, to be appended to the earlier pieces.
    Transcript(String),
    /// A low-latency preview of the transcript, which later messages supersede.
    Interim(String),
}

/// Transcribes the audio received from `audio`, which ends when the sender is dropped.
///
/// The connection is opened immediately; audio that arrives in the meantime is buffered.
pub async fn transcribe(
    config: &SessionConfig,
    mut audio: UnboundedReceiver<Vec<i16>>,
    mut on_event: impl FnMut(Event),
) -> Result<()> {
    let url = format!("{ENDPOINT}?key={}", config.api_key);
    let (socket, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .context("timed out connecting to the Gemini Live API")?
        .map_err(describe_connect_error)?;
    let (mut sink, mut stream) = socket.split();

    let setup = ClientMessage::Setup(Setup::new(config));
    send(&mut sink, setup).await?;
    loop {
        let message = next_message(&mut stream)
            .await?
            .context("the Gemini Live API closed the connection during setup")?;
        if message.setup_complete.is_some() {
            break;
        }
    }
    tracing::debug!("session set up");
    send(&mut sink, ClientMessage::activity_start()).await?;

    let mut pending: Vec<i16> = Vec::with_capacity(2 * CHUNK_SAMPLES);
    let mut recording = true;
    let mut quiet_deadline = Instant::now();
    let mut final_deadline = Instant::now();
    loop {
        tokio::select! {
            samples = audio.recv(), if recording => {
                if let Some(samples) = samples {
                    pending.extend_from_slice(&samples);
                    if pending.len() >= CHUNK_SAMPLES {
                        send(&mut sink, ClientMessage::audio(&pending)).await?;
                        pending.clear();
                    }
                    continue;
                }
                if !pending.is_empty() {
                    send(&mut sink, ClientMessage::audio(&pending)).await?;
                }
                send(&mut sink, ClientMessage::activity_end()).await?;
                recording = false;
                let now = Instant::now();
                quiet_deadline = now + QUIET_TIMEOUT;
                final_deadline = now + FINISH_TIMEOUT;
            }
            message = next_message(&mut stream) => {
                let Some(message) = message? else {
                    break;
                };
                if let Some(content) = message.server_content {
                    if let Some(transcription) = content.input_transcription {
                        tracing::debug!("transcript: {:?}", transcription.text);
                        on_event(Event::Transcript(transcription.text));
                    }
                    if let Some(transcription) = content.interim_input_transcription {
                        on_event(Event::Interim(transcription.text));
                    }
                    if !recording && content.turn_complete {
                        break;
                    }
                }
                if let Some(go_away) = message.go_away {
                    tracing::debug!("the server will disconnect in {:?}", go_away.time_left);
                }
                quiet_deadline = Instant::now() + QUIET_TIMEOUT;
            }
            () = sleep_until(quiet_deadline.min(final_deadline)), if !recording => break,
        }
    }
    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}

fn describe_connect_error(err: tungstenite::Error) -> anyhow::Error {
    if let tungstenite::Error::Http(response) = &err {
        let body = response.body().as_deref().unwrap_or_default();
        return anyhow!(
            "the Gemini Live API refused the connection with {}: {}",
            response.status(),
            String::from_utf8_lossy(body).trim()
        );
    }
    anyhow::Error::new(err).context("cannot connect to the Gemini Live API")
}

/// Returns the next server message, or `None` once the connection has been closed normally.
async fn next_message<S>(stream: &mut S) -> Result<Option<ServerMessage>>
where
    S: Stream<Item = Result<Message, tungstenite::Error>> + Unpin,
{
    while let Some(frame) = stream.next().await {
        let frame = frame.context("lost the connection to the Gemini Live API")?;
        let message: serde_json::Result<ServerMessage> = match frame {
            Message::Text(text) => serde_json::from_str(text.as_str()),
            Message::Binary(data) => serde_json::from_slice(&data),
            Message::Close(Some(close)) if close.code != CloseCode::Normal => {
                return Err(anyhow!(
                    "the Gemini Live API closed the connection: {} ({})",
                    close.reason.as_str(),
                    close.code
                ));
            }
            Message::Close(_) => return Ok(None),
            _ => continue,
        };
        return message
            .map(Some)
            .context("cannot parse a message from the Gemini Live API");
    }
    Ok(None)
}

async fn send<S>(sink: &mut S, message: ClientMessage) -> Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(&message)?;
    sink.send(Message::text(text)).await?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum ClientMessage {
    Setup(Setup),
    RealtimeInput(RealtimeInput),
}

impl ClientMessage {
    fn activity_start() -> Self {
        Self::RealtimeInput(RealtimeInput::ActivityStart {})
    }

    fn activity_end() -> Self {
        Self::RealtimeInput(RealtimeInput::ActivityEnd {})
    }

    fn audio(samples: &[i16]) -> Self {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        Self::RealtimeInput(RealtimeInput::Audio {
            data: BASE64.encode(bytes),
            mime_type: AUDIO_MIME_TYPE,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Setup {
    model: String,
    generation_config: GenerationConfig,
    realtime_input_config: RealtimeInputConfig,
    input_audio_transcription: AudioTranscriptionConfig,
}

impl Setup {
    fn new(config: &SessionConfig) -> Self {
        let model = if config.model.starts_with("models/") {
            config.model.clone()
        } else {
            format!("models/{}", config.model)
        };
        Self {
            model,
            generation_config: GenerationConfig {
                response_modalities: vec!["TEXT"],
            },
            realtime_input_config: RealtimeInputConfig {
                automatic_activity_detection: AutomaticActivityDetection { disabled: true },
            },
            input_audio_transcription: AudioTranscriptionConfig {
                language_codes: config.languages.clone(),
                custom_vocabulary: config.vocabulary.clone(),
                mode: config.mode,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    response_modalities: Vec<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RealtimeInputConfig {
    automatic_activity_detection: AutomaticActivityDetection,
}

#[derive(Serialize)]
struct AutomaticActivityDetection {
    disabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioTranscriptionConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    language_codes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    custom_vocabulary: Vec<String>,
    mode: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum RealtimeInput {
    ActivityStart {},
    ActivityEnd {},
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: &'static str,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ServerMessage {
    setup_complete: Option<IgnoredAny>,
    server_content: Option<ServerContent>,
    go_away: Option<GoAway>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ServerContent {
    input_transcription: Option<Transcription>,
    interim_input_transcription: Option<Transcription>,
    turn_complete: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Transcription {
    text: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GoAway {
    time_left: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn config() -> SessionConfig {
        SessionConfig {
            api_key: "key".into(),
            model: "gemini-3.5-transcribe-live".into(),
            languages: vec!["zh-TW".into()],
            vocabulary: vec![],
            mode: "SMART",
        }
    }

    fn to_json(message: ClientMessage) -> Value {
        serde_json::to_value(message).unwrap()
    }

    #[test]
    fn setup_message() {
        let setup = to_json(ClientMessage::Setup(Setup::new(&config())));
        assert_eq!(
            setup,
            json!({
                "setup": {
                    "model": "models/gemini-3.5-transcribe-live",
                    "generationConfig": { "responseModalities": ["TEXT"] },
                    "realtimeInputConfig": { "automaticActivityDetection": { "disabled": true } },
                    "inputAudioTranscription": { "languageCodes": ["zh-TW"], "mode": "SMART" }
                }
            })
        );
    }

    #[test]
    fn setup_message_keeps_the_model_prefix() {
        let config = SessionConfig {
            model: "models/other".into(),
            ..config()
        };
        let setup = to_json(ClientMessage::Setup(Setup::new(&config)));
        assert_eq!(setup["setup"]["model"], "models/other");
    }

    #[test]
    fn activity_messages() {
        assert_eq!(
            to_json(ClientMessage::activity_start()),
            json!({ "realtimeInput": { "activityStart": {} } })
        );
        assert_eq!(
            to_json(ClientMessage::activity_end()),
            json!({ "realtimeInput": { "activityEnd": {} } })
        );
    }

    #[test]
    fn audio_is_little_endian_base64() {
        let data = BASE64.encode([1u8, 0, 0xfe, 0xff]);
        assert_eq!(
            to_json(ClientMessage::audio(&[1, -2])),
            json!({
                "realtimeInput": {
                    "audio": { "data": data, "mimeType": "audio/pcm;rate=16000" }
                }
            })
        );
    }

    #[test]
    fn parses_server_messages() {
        let message: ServerMessage = serde_json::from_str(r#"{"setupComplete": {}}"#).unwrap();
        assert!(message.setup_complete.is_some());

        let message: ServerMessage = serde_json::from_str(
            r#"{
                "serverContent": {
                    "inputTranscription": { "text": "你好", "languageCode": "zh-TW" }
                },
                "usageMetadata": { "totalTokenCount": 3 }
            }"#,
        )
        .unwrap();
        assert!(message.setup_complete.is_none());
        let content = message.server_content.unwrap();
        assert_eq!(content.input_transcription.unwrap().text, "你好");
        assert!(!content.turn_complete);

        let message: ServerMessage =
            serde_json::from_str(r#"{"serverContent": {"turnComplete": true}}"#).unwrap();
        assert!(message.server_content.unwrap().turn_complete);
    }
}
