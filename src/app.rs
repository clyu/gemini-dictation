//! Ties the push-to-talk key, the microphone, the transcription sessions and the output together.

use std::env;
use std::fs;
use std::future;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use crate::audio::Recorder;
use crate::cli::{CtlAction, RunArgs};
use crate::gemini::{self, SessionConfig};
use crate::hotkey::{self, PushToTalk, Tracker};
use crate::ipc;
use crate::output::{self, Emitter, Update};

pub async fn run(args: RunArgs) -> Result<()> {
    let languages = args.language_codes();
    let api_key = api_key(args.api_key)?;
    let vocabulary = vocabulary(args.vocabulary)?;
    let key = hotkey::parse_key(&args.key)?;
    // Parsed here rather than by the paste output, so that a mistake is reported whatever the
    // output turns out to be.
    let paste_keys = output::parse_keys(&args.paste_keys)?;
    let config = SessionConfig {
        api_key,
        languages,
        model: args.model,
        vocabulary,
        mode: args.transcription_mode.api_name(),
    };
    tracing::info!("transcribing with {}", config.model);
    if !config.vocabulary.is_empty() {
        tracing::info!("favoring {} vocabulary phrases", config.vocabulary.len());
        tracing::debug!("vocabulary: {:?}", config.vocabulary);
    }
    let (actions, mut ctl) = mpsc::unbounded_channel();
    let _server = ipc::serve(actions).await?;

    let emitter = Emitter::new(args.output, paste_keys)?;
    tracing::info!("delivering transcripts by {}", emitter.description());
    let (updates, pending_updates) = mpsc::unbounded_channel();
    let (keys_held, keys_held_updates) = watch::channel(false);
    tokio::spawn(output::deliver(emitter, pending_updates, keys_held_updates));

    let (keyboard_events, mut keyboard) = mpsc::unbounded_channel();
    if args.no_hotkey {
        drop(keyboard_events);
    } else {
        match hotkey::listen(Some(key), keyboard_events) {
            Ok(()) => tracing::info!("hold {key:?} to talk"),
            Err(err) => tracing::warn!("{err:#}; only `gemini-dictation ctl` will work"),
        }
    }
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    let mut tracker = Tracker::new(key);
    let mut app = App {
        config: Arc::new(config),
        recorder: Recorder::spawn(args.mic)?,
        updates,
        recording: None,
        next_id: 0,
        min_hold: Duration::from_millis(args.min_hold_ms),
        max_length: Duration::from_secs(args.max_record_secs),
    };
    loop {
        tokio::select! {
            Some(event) = keyboard.recv() => {
                let action = tracker.handle(event);
                let held = tracker.keys_held();
                keys_held.send_if_modified(|value| mem::replace(value, held) != held);
                match action {
                    Some(PushToTalk::Press) => app.start(false).await,
                    Some(PushToTalk::Release) => app.stop(),
                    Some(PushToTalk::Cancel) => app.cancel(),
                    None => {}
                }
            }
            Some(action) = ctl.recv() => match action {
                CtlAction::Start => app.start(true).await,
                CtlAction::Stop => app.stop(),
                CtlAction::Toggle if app.recording.is_some() => app.stop(),
                CtlAction::Toggle => app.start(true).await,
                CtlAction::Cancel => app.cancel(),
                CtlAction::Quit => break,
            },
            () = wait_until(app.connect_at()) => app.connect(),
            () = wait_until(app.stop_at()) => {
                tracing::warn!("stopping the recording after {:?}", app.max_length);
                app.stop();
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    Ok(())
}

/// Returns the API key given on the command line or in the environment, or else the one saved in
/// the configuration directory, as desktop launchers do not see the variables set by shells.
fn api_key(given: Option<String>) -> Result<String> {
    if let Some(key) = given.filter(|key| !key.is_empty()) {
        return Ok(key);
    }
    let path = config_path("api-key");
    let key = read_config(&path)?.trim().to_owned();
    if key.is_empty() {
        bail!(
            "no Gemini API key: set GEMINI_API_KEY, pass --api-key, or save it in {}",
            path.display()
        );
    }
    Ok(key)
}

/// Returns the phrases saved in the vocabulary file of the configuration directory, followed by
/// those given on the command line.
fn vocabulary(given: Vec<String>) -> Result<Vec<String>> {
    let saved = read_config(&config_path("vocabulary"))?;
    Ok(parse_vocabulary(&saved, given))
}

/// Returns the phrases of a vocabulary file, one per line, followed by the phrases `given`, without
/// blank lines, `#` comments and duplicates.
fn parse_vocabulary(saved: &str, given: Vec<String>) -> Vec<String> {
    let lines = saved
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(str::to_owned);
    let mut phrases: Vec<String> = Vec::new();
    for phrase in lines.chain(given) {
        let phrase = phrase.trim().to_owned();
        if !phrase.is_empty() && !phrases.contains(&phrase) {
            phrases.push(phrase);
        }
    }
    phrases
}

/// Returns the contents of a file in the configuration directory, or nothing if it does not exist.
fn read_config(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Returns the path of a file in the configuration directory, ~/.config/gemini-dictation.
fn config_path(name: &str) -> PathBuf {
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default();
    config.join("gemini-dictation").join(name)
}

/// Waits until `deadline`, or forever if there is none.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => future::pending().await,
    }
}

/// What is done with the audio of a recording.
enum Stage {
    /// The audio is kept, until the recording has lasted long enough to be transcribed.
    Buffering(mpsc::UnboundedReceiver<Vec<i16>>),
    /// A session transcribes the audio.
    Transcribing(JoinHandle<()>),
}

struct Recording {
    id: u64,
    started: Instant,
    stage: Stage,
}

impl Recording {
    /// Whether its transcription has yet to start.
    fn buffering(&self) -> bool {
        matches!(self.stage, Stage::Buffering(_))
    }
}

struct App {
    config: Arc<SessionConfig>,
    recorder: Recorder,
    updates: mpsc::UnboundedSender<Update>,
    recording: Option<Recording>,
    next_id: u64,
    /// Key presses shorter than this are ignored, as they are taps rather than push-to-talk.
    min_hold: Duration,
    max_length: Duration,
}

impl App {
    /// When the pending recording is to be transcribed, if it is still recording by then.
    fn connect_at(&self) -> Option<Instant> {
        let recording = self.recording.as_ref().filter(|r| r.buffering())?;
        Some(recording.started + self.min_hold)
    }

    fn stop_at(&self) -> Option<Instant> {
        let recording = self.recording.as_ref()?;
        Some(recording.started + self.max_length)
    }

    /// Starts recording. The recording is transcribed after the minimum hold time, or at once if
    /// `immediately`, so that taps of the push-to-talk key do not reach Gemini.
    async fn start(&mut self, immediately: bool) {
        if self.recording.is_some() {
            return;
        }
        let (sink, audio) = mpsc::unbounded_channel();
        if let Err(err) = self.recorder.start(sink).await {
            tracing::error!("{err:#}");
            return;
        }
        tracing::info!("recording");
        self.next_id += 1;
        self.recording = Some(Recording {
            id: self.next_id,
            started: Instant::now(),
            stage: Stage::Buffering(audio),
        });
        if immediately {
            self.connect();
        }
    }

    /// Starts transcribing the recording, if that has not started yet.
    fn connect(&mut self) {
        let Some(mut recording) = self.recording.take() else {
            return;
        };
        recording.stage = match recording.stage {
            Stage::Buffering(audio) => Stage::Transcribing(self.transcribe(recording.id, audio)),
            transcribing => transcribing,
        };
        self.recording = Some(recording);
    }

    /// Starts a session that transcribes the `audio` of the recording `id`.
    fn transcribe(&self, id: u64, audio: mpsc::UnboundedReceiver<Vec<i16>>) -> JoinHandle<()> {
        let _ = self.updates.send(Update::Begin(id));
        let config = self.config.clone();
        let updates = self.updates.clone();
        tokio::spawn(async move {
            let on_transcript = |text| {
                let _ = updates.send(Update::Text(id, text));
            };
            if let Err(err) = gemini::transcribe(&config, audio, on_transcript).await {
                tracing::error!("transcription failed: {err:#}");
            }
            let _ = updates.send(Update::End(id));
        })
    }

    /// Stops recording, and lets the transcription finish.
    fn stop(&mut self) {
        let Some(recording) = &self.recording else {
            return;
        };
        let length = recording.started.elapsed();
        if recording.buffering() && length < self.min_hold {
            tracing::info!("ignoring a tap of {} ms", length.as_millis());
            self.cancel();
            return;
        }
        self.connect();
        self.recorder.stop();
        self.recording = None;
        tracing::info!("transcribing {:.1} s of audio", length.as_secs_f32());
    }

    /// Stops recording, and discards the recording and its transcript.
    fn cancel(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        self.recorder.stop();
        if let Stage::Transcribing(session) = recording.stage {
            session.abort();
            let _ = self.updates.send(Update::Discard(recording.id));
            tracing::info!("recording cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_file() {
        let saved = "Gemini\n\n  # Desktops\n  Wayland  \r\nGemini\n";
        let given = vec!["Sway".to_owned(), "Wayland".to_owned(), " ".to_owned()];
        assert_eq!(
            parse_vocabulary(saved, given),
            ["Gemini", "Wayland", "Sway"]
        );
        assert!(parse_vocabulary("", vec![]).is_empty());
    }
}
