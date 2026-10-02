use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::gemini::MAX_RECORDING_SECS;

pub const DEFAULT_MODEL: &str = "gemini-3.5-transcribe-live";

/// Push-to-talk dictation for Wayland, transcribed by the Gemini Live API.
///
/// Hold the push-to-talk key, speak, and release it: the transcript is typed into the focused
/// window.
#[derive(Debug, Parser)]
#[command(name = "gemini-dictation", version = env!("GEMINI_DICTATION_VERSION"))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub run: RunArgs,

    /// Log debugging details.
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Control a running instance, e.g. from a compositor key binding.
    Ctl {
        #[arg(value_enum)]
        action: CtlAction,
    },
    /// List the microphones and keyboards that can be used.
    Devices,
    /// Print the name of every key pressed, to choose a push-to-talk key.
    Keys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CtlAction {
    /// Start recording.
    Start,
    /// Stop recording and transcribe.
    Stop,
    /// Start recording, or stop it if already recording.
    Toggle,
    /// Stop recording and discard it.
    Cancel,
    /// Exit the running instance.
    Quit,
}

impl CtlAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CtlAction::Start => "start",
            CtlAction::Stop => "stop",
            CtlAction::Toggle => "toggle",
            CtlAction::Cancel => "cancel",
            CtlAction::Quit => "quit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        <Self as ValueEnum>::from_str(s, true).ok()
    }
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Gemini API key; read from ~/.config/gemini-dictation/api-key if not given.
    #[arg(long, env = "GEMINI_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    /// Gemini Live API transcription model.
    #[arg(long, env = "GEMINI_DICTATION_MODEL", default_value = DEFAULT_MODEL)]
    pub model: String,

    /// Push-to-talk key, as an evdev key name such as KEY_RIGHTCTRL or F9; `gemini-dictation keys`
    /// prints the names of the keys pressed.
    #[arg(long, default_value = "KEY_RIGHTCTRL")]
    pub key: String,

    /// Do not read keyboards; only react to `gemini-dictation ctl`.
    #[arg(long)]
    pub no_hotkey: bool,

    /// BCP-47 codes of the languages spoken, or `auto` for automatic detection. Without a hint,
    /// Mandarin is transcribed in Simplified Chinese, as speech says nothing about the script.
    #[arg(
        long = "language",
        value_name = "CODES",
        value_delimiter = ',',
        default_values = ["zh-Hant", "en"]
    )]
    pub languages: Vec<String>,

    /// Phrase that recognition should favor, such as a product name, in addition to those in
    /// ~/.config/gemini-dictation/vocabulary; can be repeated.
    #[arg(long = "vocabulary", value_name = "PHRASE")]
    pub vocabulary: Vec<String>,

    /// How literally speech is transcribed.
    #[arg(long, value_enum, default_value_t = TranscriptionMode::Smart)]
    pub transcription_mode: TranscriptionMode,

    /// How the transcript reaches the focused window.
    #[arg(long, value_enum, default_value_t = OutputMode::Auto)]
    pub output: OutputMode,

    /// Key combination that the paste output presses, such as shift+insert, ctrl+v or
    /// ctrl+shift+v.
    #[arg(long, default_value = "shift+insert")]
    pub paste_keys: String,

    /// Microphone to record from (see `gemini-dictation devices`); the default input if omitted.
    #[arg(long)]
    pub mic: Option<String>,

    /// Recordings shorter than this many milliseconds are discarded without contacting Gemini.
    #[arg(long, default_value_t = 250)]
    pub min_hold_ms: u64,

    /// Recordings are stopped after this many seconds, at most 580: the Live API ends a session
    /// after 10 minutes, and needs some of that to finish the transcript.
    #[arg(
        long,
        default_value_t = 300,
        value_parser = clap::value_parser!(u64).range(1..=MAX_RECORDING_SECS)
    )]
    pub max_record_secs: u64,
}

impl RunArgs {
    /// The language hints for transcription; empty for automatic detection.
    pub fn language_codes(&self) -> Vec<String> {
        self.languages
            .iter()
            .map(|code| code.trim())
            .filter(|code| !code.is_empty() && !code.eq_ignore_ascii_case("auto"))
            .map(str::to_owned)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TranscriptionMode {
    /// Remove filler words and false starts, and tidy up punctuation and formatting.
    Smart,
    /// Transcribe exactly what was said.
    Verbatim,
}

impl TranscriptionMode {
    pub fn api_name(self) -> &'static str {
        match self {
            TranscriptionMode::Smart => "SMART",
            TranscriptionMode::Verbatim => "VERBATIM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// `type` if the compositor supports it, `paste` otherwise.
    Auto,
    /// Type the text with wtype (wlroots compositors such as Sway and Hyprland).
    Type,
    /// Copy the text with wl-copy and press the paste keys on a virtual keyboard.
    Paste,
    /// Only copy the text to the clipboard.
    Clipboard,
    /// Print the text on standard output.
    Stdout,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults() {
        let cli = Cli::try_parse_from(["gemini-dictation"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.key, "KEY_RIGHTCTRL");
        assert_eq!(cli.run.language_codes(), ["zh-Hant", "en"]);
        assert_eq!(cli.run.transcription_mode, TranscriptionMode::Smart);
        assert_eq!(cli.run.output, OutputMode::Auto);
    }

    #[test]
    fn languages() {
        let parse = |args: &[&str]| {
            let cli = Cli::try_parse_from(["gemini-dictation"].iter().chain(args)).unwrap();
            cli.run.language_codes()
        };
        assert_eq!(parse(&["--language", "ja,en"]), ["ja", "en"]);
        assert_eq!(parse(&["--language=ja", "--language=ko"]), ["ja", "ko"]);
        assert!(parse(&["--language", "auto"]).is_empty());
    }

    #[test]
    fn repeated_vocabulary() {
        let args = [
            "gemini-dictation",
            "--vocabulary=Gemini",
            "--vocabulary=Wayland",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.run.vocabulary, ["Gemini", "Wayland"]);
    }

    #[test]
    fn max_record_secs_is_limited() {
        let parse = |secs: &str| {
            let args = ["gemini-dictation", "--max-record-secs", secs];
            Cli::try_parse_from(args)
        };
        assert_eq!(parse("580").unwrap().run.max_record_secs, 580);
        assert!(parse("581").is_err());
        assert!(parse("0").is_err());
    }

    #[test]
    fn ctl_actions_round_trip() {
        for action in CtlAction::value_variants() {
            assert_eq!(CtlAction::parse(action.as_str()), Some(*action));
        }
        assert_eq!(CtlAction::parse("bogus"), None);
    }
}
