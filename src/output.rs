//! Delivery of transcripts to the focused window.

use std::collections::VecDeque;
use std::env;
use std::io::{self, Write};
use std::mem;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::cli::OutputMode;
use crate::hotkey::{VIRTUAL_KEYBOARD_NAME, parse_key};

/// Gives the compositor time to take the new clipboard contents before pasting them.
const CLIPBOARD_DELAY: Duration = Duration::from_millis(100);
/// The time between the key presses and releases of the paste keys.
const KEY_DELAY: Duration = Duration::from_millis(15);

/// Updates on the transcripts of the recordings, identified by a number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The transcription of a recording has started. Transcripts are delivered in this order.
    Begin(u64),
    /// A piece of a recording's transcript.
    Text(u64, String),
    /// The transcription of a recording has ended.
    End(u64),
    /// The recording was cancelled, and its transcript must not be delivered.
    Discard(u64),
}

struct Transcript {
    id: u64,
    pending: String,
    started: bool,
    ended: bool,
}

/// Orders the pieces of the transcripts, joins their lines, and separates consecutive transcripts.
pub struct Queue {
    transcripts: VecDeque<Transcript>,
    /// Deliver only complete transcripts, rather than each piece as soon as possible, as each
    /// delivery replaces the last one instead of following it.
    whole: bool,
    /// The last character delivered, which the next transcript is separated from.
    last_char: Option<char>,
}

impl Queue {
    pub fn new(whole: bool) -> Self {
        Self {
            transcripts: VecDeque::new(),
            whole,
            last_char: None,
        }
    }

    pub fn push(&mut self, update: Update) {
        match update {
            Update::Begin(id) => self.transcripts.push_back(Transcript {
                id,
                pending: String::new(),
                started: false,
                ended: false,
            }),
            Update::Text(id, text) => {
                if let Some(transcript) = self.find(id) {
                    transcript.pending.push_str(&text);
                }
            }
            Update::End(id) => {
                if let Some(transcript) = self.find(id) {
                    transcript.ended = true;
                }
            }
            Update::Discard(id) => self.transcripts.retain(|t| t.id != id),
        }
    }

    fn find(&mut self, id: u64) -> Option<&mut Transcript> {
        self.transcripts.iter_mut().find(|t| t.id == id)
    }

    /// Takes the text that can be delivered now.
    pub fn take_ready(&mut self) -> String {
        if self.whole {
            // This text does not follow the last delivery, so it is not separated from it either.
            self.last_char = None;
        }
        let mut ready = String::new();
        while let Some(transcript) = self.transcripts.front_mut() {
            if transcript.ended || !self.whole {
                let pending = mem::take(&mut transcript.pending);
                let (text, last) = if transcript.started {
                    (pending.as_str(), self.last_char)
                } else {
                    (pending.trim_start(), None)
                };
                let (text, line_break) = join_lines(text, last);
                if !transcript.ended {
                    transcript.pending = line_break.to_owned();
                }
                if !transcript.started && needs_space(self.last_char, text.chars().next()) {
                    ready.push(' ');
                }
                if let Some(last) = text.chars().next_back() {
                    transcript.started = true;
                    self.last_char = Some(last);
                    ready.push_str(&text);
                }
            }
            if !transcript.ended {
                break;
            }
            self.transcripts.pop_front();
        }
        ready
    }
}

/// Punctuation that is written without a space after it.
const OPENING: &str = "([{“‘";
/// Punctuation that is written without a space before it.
const CLOSING: &str = ".,!?:;%)]}”’";

/// Whether a space belongs between `last` and `next`, the characters on either side of a line
/// break or of the boundary between two transcripts, if there is text on both sides. Chinese and
/// Japanese are written without spaces, and so is punctuation on the side of its word.
fn needs_space(last: Option<char>, next: Option<char>) -> bool {
    let (Some(last), Some(next)) = (last, next) else {
        return false;
    };
    let spaced = |c: char| !c.is_whitespace() && !is_cjk(c);
    spaced(last) && spaced(next) && !OPENING.contains(last) && !CLOSING.contains(next)
}

/// Replaces each line break in `text`, together with the whitespace around it, with a space, or
/// with nothing where no space belongs, so that a transcript never presses Enter. `last` is the
/// character before `text`.
///
/// Returns the joined text, and the line break at the end of `text` if there is one, which can
/// only be replaced once the text that follows it is known.
fn join_lines(text: &str, mut last: Option<char>) -> (String, &str) {
    let mut joined = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(is_line_break) {
        let before = rest[..index].trim_end();
        let after = rest[index..].trim_start();
        joined.push_str(before);
        last = before.chars().next_back().or(last);
        let Some(next) = after.chars().next() else {
            return (joined, &rest[before.len()..]);
        };
        if needs_space(last, Some(next)) {
            joined.push(' ');
        }
        rest = after;
    }
    joined.push_str(rest);
    (joined, "")
}

fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Chinese and Japanese characters and punctuation, which are written without spaces.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{2e80}'..='\u{31ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ff00}'..='\u{ffef}'
            | '\u{20000}'..='\u{3ffff}'
    )
}

pub enum Emitter {
    Type,
    Paste {
        keyboard: VirtualDevice,
        keys: Vec<KeyCode>,
    },
    Clipboard,
    Stdout,
}

impl Emitter {
    /// `paste_keys` is the key combination that the paste output presses.
    pub fn new(mode: OutputMode, paste_keys: Vec<KeyCode>) -> Result<Self> {
        match mode {
            OutputMode::Auto => Ok(Self::detect(paste_keys)),
            OutputMode::Type => {
                require("wtype")?;
                Ok(Self::Type)
            }
            OutputMode::Paste => {
                require("wl-copy")?;
                Ok(Self::Paste {
                    keys: paste_keys,
                    keyboard: virtual_keyboard()?,
                })
            }
            OutputMode::Clipboard => {
                require("wl-copy")?;
                Ok(Self::Clipboard)
            }
            OutputMode::Stdout => Ok(Self::Stdout),
        }
    }

    fn detect(paste_keys: Vec<KeyCode>) -> Self {
        // GNOME and KDE do not offer the virtual keyboard protocol that wtype relies on.
        let desktop = env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let without_protocol = desktop.contains("gnome") || desktop.contains("kde");
        if !without_protocol && in_path("wtype") {
            return Self::Type;
        }
        if !in_path("wl-copy") {
            let reason = if without_protocol {
                "wl-copy is not installed, and wtype does not work on this desktop"
            } else {
                "wtype and wl-copy are not installed"
            };
            tracing::warn!("{reason}; printing transcripts instead");
            return Self::Stdout;
        }
        match virtual_keyboard() {
            Ok(keyboard) => Self::Paste {
                keys: paste_keys,
                keyboard,
            },
            Err(err) => {
                tracing::warn!("{err:#}; only copying transcripts to the clipboard");
                Self::Clipboard
            }
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Type => "typing with wtype",
            Self::Paste { .. } => "pasting with wl-copy and a virtual keyboard",
            Self::Clipboard => "copying to the clipboard with wl-copy",
            Self::Stdout => "printing on standard output",
        }
    }

    /// Whether only complete transcripts should be delivered, as each delivery replaces the last.
    pub fn wants_whole_transcripts(&self) -> bool {
        matches!(self, Self::Clipboard)
    }

    pub async fn emit(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Type => run("wtype", &["--", text], None).await,
            Self::Paste { keyboard, keys } => {
                copy(text, &[]).await?;
                // Terminals such as GNOME Terminal paste the primary selection, which is that of
                // the text selected last, on Shift+Insert.
                if let Err(err) = copy(text, &["--primary"]).await {
                    tracing::debug!("cannot set the primary selection: {err:#}");
                }
                sleep(CLIPBOARD_DELAY).await;
                press(keyboard, keys).await
            }
            Self::Clipboard => copy(text, &[]).await,
            Self::Stdout => print(text).context("cannot write to standard output"),
        }
    }
}

fn in_path(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn require(program: &str) -> Result<()> {
    if !in_path(program) {
        bail!("{program} is not installed");
    }
    Ok(())
}

/// Parses a key combination such as `ctrl+shift+v`.
pub fn parse_keys(combination: &str) -> Result<Vec<KeyCode>> {
    combination
        .split('+')
        .map(parse_key)
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("invalid key combination {combination:?}"))
}

fn virtual_keyboard() -> Result<VirtualDevice> {
    // All keyboard keys, so that it is recognised as a keyboard.
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=KeyCode::KEY_MICMUTE.code() {
        keys.insert(KeyCode::new(code));
    }
    let build = || {
        VirtualDevice::builder()?
            .name(VIRTUAL_KEYBOARD_NAME)
            .with_keys(&keys)?
            .build()
    };
    build().context(
        "cannot create a virtual keyboard: /dev/uinput needs to be writable (see the README), \
         or choose another --output",
    )
}

/// Runs `program` until it exits, with `input` on its standard input if there is any.
async fn run(program: &str, args: &[&str], input: Option<&str>) -> Result<()> {
    let stdin = match input {
        Some(_) => Stdio::piped(),
        None => Stdio::inherit(),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(stdin)
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    if let Some(input) = input {
        // The pipe is closed at the end of this block, which ends the input.
        let mut pipe = child
            .stdin
            .take()
            .with_context(|| format!("cannot write to {program}"))?;
        pipe.write_all(input.as_bytes()).await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        bail!("{program} failed ({status})");
    }
    Ok(())
}

/// Copies `text` with wl-copy and its `options`: to the clipboard, unless they say otherwise.
/// wl-copy only sets the clipboard, and then keeps serving it in the background, once its input
/// has ended.
async fn copy(text: &str, options: &[&str]) -> Result<()> {
    let mut args = vec!["--type", "text/plain;charset=utf-8"];
    args.extend(options);
    run("wl-copy", &args, Some(text)).await
}

async fn press(keyboard: &mut VirtualDevice, keys: &[KeyCode]) -> Result<()> {
    let presses = keys.iter().map(|&key| (key, 1));
    let releases = keys.iter().rev().map(|&key| (key, 0));
    for (key, value) in presses.chain(releases) {
        keyboard.emit(&[InputEvent::new(EventType::KEY.0, key.code(), value)])?;
        sleep(KEY_DELAY).await;
    }
    Ok(())
}

fn print(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Delivers the transcripts from `updates`, but only while no keys are held that would turn the
/// text into shortcuts.
pub async fn deliver(
    mut emitter: Emitter,
    mut updates: mpsc::UnboundedReceiver<Update>,
    mut keys_held: watch::Receiver<bool>,
) {
    let mut queue = Queue::new(emitter.wants_whole_transcripts());
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Some(update) => queue.push(update),
                None => break,
            },
            changed = keys_held.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
        if *keys_held.borrow_and_update() {
            continue;
        }
        let text = queue.take_ready();
        if text.is_empty() {
            continue;
        }
        tracing::info!("transcript: {text}");
        if let Err(err) = emitter.emit(&text).await {
            tracing::error!("cannot deliver the transcript: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Update::{Begin, Discard, End};
    use super::*;

    fn text(id: u64, text: &str) -> Update {
        Update::Text(id, text.to_owned())
    }

    fn feed<const N: usize>(queue: &mut Queue, updates: [Update; N]) -> String {
        for update in updates {
            queue.push(update);
        }
        queue.take_ready()
    }

    #[test]
    fn delivers_pieces_as_they_arrive() {
        let mut queue = Queue::new(false);
        assert_eq!(feed(&mut queue, [Begin(1), text(1, " Hello")]), "Hello");
        assert_eq!(feed(&mut queue, [text(1, " world.")]), " world.");
        assert_eq!(feed(&mut queue, [End(1)]), "");
        assert_eq!(feed(&mut queue, [Begin(2), text(2, "Bye")]), " Bye");
    }

    #[test]
    fn delivers_transcripts_in_order() {
        let mut queue = Queue::new(false);
        let updates = [Begin(1), Begin(2), text(2, "二"), text(1, "一")];
        assert_eq!(feed(&mut queue, updates), "一");
        assert_eq!(feed(&mut queue, [End(1)]), "二");
        assert_eq!(feed(&mut queue, [text(2, "三"), End(2)]), "三");
    }

    #[test]
    fn whole_transcripts_wait_for_the_end() {
        let mut queue = Queue::new(true);
        assert_eq!(feed(&mut queue, [Begin(1), text(1, "a")]), "");
        assert_eq!(feed(&mut queue, [text(1, "b"), End(1)]), "ab");
    }

    #[test]
    fn whole_transcripts_stand_alone() {
        let mut queue = Queue::new(true);
        assert_eq!(feed(&mut queue, [Begin(1), text(1, "Hi."), End(1)]), "Hi.");
        assert_eq!(feed(&mut queue, [Begin(2), text(2, "OK"), End(2)]), "OK");
        // Those delivered together are still separated from each other.
        assert_eq!(feed(&mut queue, [Begin(3), text(3, "One."), Begin(4)]), "");
        let updates = [End(3), text(4, "Two"), End(4)];
        assert_eq!(feed(&mut queue, updates), "One. Two");
    }

    #[test]
    fn discarded_transcripts_are_skipped() {
        let mut queue = Queue::new(false);
        let updates = [Begin(1), Begin(2), text(1, "no"), text(2, "yes")];
        assert_eq!(feed(&mut queue, updates), "no");
        let updates = [Discard(1), text(1, "late"), End(2)];
        assert_eq!(feed(&mut queue, updates), " yes");
        assert!(queue.transcripts.is_empty());
    }

    #[test]
    fn blank_transcripts_leave_no_trace() {
        let mut queue = Queue::new(false);
        assert_eq!(feed(&mut queue, [Begin(1), text(1, "Hi."), End(1)]), "Hi.");
        assert_eq!(feed(&mut queue, [Begin(2), text(2, " "), End(2)]), "");
        assert_eq!(feed(&mut queue, [Begin(3), text(3, " OK")]), " OK");
    }

    #[test]
    fn line_breaks_become_spaces() {
        let mut queue = Queue::new(false);
        let updates = [Begin(1), text(1, "One.\nTwo \r\n\n three"), End(1)];
        assert_eq!(feed(&mut queue, updates), "One. Two three");
    }

    #[test]
    fn line_breaks_next_to_chinese_disappear() {
        let mut queue = Queue::new(false);
        let updates = [Begin(1), text(1, "第一。\n\n第二\nA"), End(1)];
        assert_eq!(feed(&mut queue, updates), "第一。第二A");
    }

    #[test]
    fn line_breaks_wait_for_the_next_piece() {
        let mut queue = Queue::new(false);
        assert_eq!(feed(&mut queue, [Begin(1), text(1, "\nOne.\n")]), "One.");
        assert_eq!(feed(&mut queue, [text(1, " Two\n")]), " Two");
        assert_eq!(feed(&mut queue, [End(1)]), "");
        assert_eq!(feed(&mut queue, [Begin(2), text(2, "Three")]), " Three");
    }

    #[test]
    fn spaces_between_words() {
        let space = |last, next| needs_space(Some(last), Some(next));
        assert!(space('.', 'N'));
        assert!(space('a', '1'));
        assert!(space('%', 'N'));
        assert!(space('"', 'T'));
        assert!(space('a', '('));
        assert!(space('é', 'É'));
        assert!(space('요', '안'));
        assert!(!space(' ', 'a'));
        assert!(!space('(', 'a'));
        assert!(!space('a', ','));
        assert!(!space('a', ')'));
        assert!(!space('。', 'a'));
        assert!(!space('a', '中'));
        assert!(!needs_space(None, Some('a')));
        assert!(!needs_space(Some('a'), None));
    }

    #[test]
    fn line_breaks_and_transcripts_are_separated_alike() {
        let pieces = ["100%", "(ok)", ", \"hi\"", "Then", "中文", "end"];
        let expected = "100% (ok), \"hi\" Then中文end";

        let mut queue = Queue::new(false);
        let updates = [Begin(1), text(1, &pieces.join("\n")), End(1)];
        assert_eq!(feed(&mut queue, updates), expected);

        let mut queue = Queue::new(false);
        let mut delivered = String::new();
        for (id, piece) in (1..).zip(pieces) {
            delivered += &feed(&mut queue, [Begin(id), text(id, piece), End(id)]);
        }
        assert_eq!(delivered, expected);
    }

    #[test]
    fn parses_key_combinations() {
        assert_eq!(
            parse_keys("ctrl+shift+v").unwrap(),
            [
                KeyCode::KEY_LEFTCTRL,
                KeyCode::KEY_LEFTSHIFT,
                KeyCode::KEY_V
            ]
        );
        assert_eq!(
            parse_keys("shift+insert").unwrap(),
            [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_INSERT]
        );
        assert!(parse_keys("ctrl+").is_err());
    }
}
