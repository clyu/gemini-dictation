//! The push-to-talk key, read from the keyboards' evdev devices.
//!
//! Wayland does not let applications see keys pressed in other windows, so they are read from
//! `/dev/input`, which requires membership of the `input` group.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use evdev::{Device, EventType, KeyCode};
use tokio::sync::mpsc::{self, UnboundedSender};

/// The name of the virtual keyboard that the paste output creates, which is not listened to.
pub const VIRTUAL_KEYBOARD_NAME: &str = "gemini-dictation virtual keyboard";

const INPUT_DIR: &str = "/dev/input";
/// How often to check for keyboards that were plugged in.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

const RELEASED: i32 = 0;
const PRESSED: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardEvent {
    /// A key was released (0), pressed (1) or repeated (2).
    Key(KeyCode, i32),
    /// A keyboard was unplugged, so the keys held on it will not be reported as released.
    Unplugged,
}

/// Parses a key name such as `KEY_RIGHTCTRL`, `rightctrl`, `F9`, `BTN_SIDE` or `ctrl`.
pub fn parse_key(name: &str) -> Result<KeyCode> {
    let upper = name.trim().to_ascii_uppercase();
    let alias = match upper.as_str() {
        "CTRL" | "CONTROL" => Some(KeyCode::KEY_LEFTCTRL),
        "SHIFT" => Some(KeyCode::KEY_LEFTSHIFT),
        "ALT" => Some(KeyCode::KEY_LEFTALT),
        "SUPER" | "META" | "LOGO" | "WIN" => Some(KeyCode::KEY_LEFTMETA),
        _ => None,
    };
    alias
        .or_else(|| KeyCode::from_str(&upper).ok())
        .or_else(|| KeyCode::from_str(&format!("KEY_{upper}")).ok())
        .with_context(|| format!("unknown key {name:?}; `gemini-dictation keys` shows key names"))
}

fn is_modifier(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KEY_LEFTCTRL
            | KeyCode::KEY_RIGHTCTRL
            | KeyCode::KEY_LEFTSHIFT
            | KeyCode::KEY_RIGHTSHIFT
            | KeyCode::KEY_LEFTALT
            | KeyCode::KEY_RIGHTALT
            | KeyCode::KEY_LEFTMETA
            | KeyCode::KEY_RIGHTMETA
    )
}

/// Keyboard keys, as opposed to mouse, joystick and other buttons, which start at `BTN_0`.
fn is_keyboard_key(code: KeyCode) -> bool {
    code.code() < KeyCode::BTN_0.code()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushToTalk {
    Press,
    Release,
    /// The key was used in a combination with another key, so it was not meant as push-to-talk.
    Cancel,
}

/// Turns keyboard events into push-to-talk events, and tracks whether typing has to wait.
pub struct Tracker {
    key: KeyCode,
    down: bool,
    chorded: bool,
    modifiers: HashSet<KeyCode>,
}

impl Tracker {
    pub fn new(key: KeyCode) -> Self {
        Self {
            key,
            down: false,
            chorded: false,
            modifiers: HashSet::new(),
        }
    }

    pub fn handle(&mut self, event: KeyboardEvent) -> Option<PushToTalk> {
        let (code, value) = match event {
            KeyboardEvent::Key(code, value) => (code, value),
            KeyboardEvent::Unplugged => {
                self.modifiers.clear();
                return self.handle(KeyboardEvent::Key(self.key, RELEASED));
            }
        };
        if code == self.key {
            return match value {
                PRESSED if !self.down => {
                    self.down = true;
                    self.chorded = false;
                    Some(PushToTalk::Press)
                }
                RELEASED if self.down => {
                    self.down = false;
                    (!self.chorded).then_some(PushToTalk::Release)
                }
                _ => None,
            };
        }
        if is_modifier(code) {
            match value {
                PRESSED => {
                    self.modifiers.insert(code);
                }
                RELEASED => {
                    self.modifiers.remove(&code);
                }
                _ => {}
            }
        }
        if value == PRESSED && self.down && !self.chorded && is_keyboard_key(code) {
            self.chorded = true;
            return Some(PushToTalk::Cancel);
        }
        None
    }

    /// Whether keys are held that would turn typed text into shortcuts.
    pub fn keys_held(&self) -> bool {
        self.down || !self.modifiers.is_empty()
    }
}

/// Forwards the key events of the devices that have `key` (any keys if `None`) to `events`,
/// including devices that are plugged in later.
pub fn listen(key: Option<KeyCode>, events: UnboundedSender<KeyboardEvent>) -> Result<()> {
    check_access()?;
    let listening: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let mut modified = input_dir_modified();
    let mut scan = Scan::run(key, &events, &listening);
    if scan.started == 0 {
        match key {
            Some(key) => tracing::warn!("no input device has {key:?} yet"),
            None => tracing::warn!("no input device has keys yet"),
        }
    }
    thread::Builder::new()
        .name("input-scan".into())
        .spawn(move || {
            while !events.is_closed() {
                thread::sleep(RESCAN_INTERVAL);
                // Devices are only opened again when some were added, as opening them can wake
                // them up, but new devices may take a moment to get their permissions.
                let now = input_dir_modified();
                if now != modified || scan.denied {
                    modified = now;
                    scan = Scan::run(key, &events, &listening);
                }
            }
        })
        .context("cannot start the input device scanner")?;
    Ok(())
}

fn input_dir_modified() -> Option<SystemTime> {
    fs::metadata(INPUT_DIR).and_then(|dir| dir.modified()).ok()
}

fn event_nodes() -> io::Result<Vec<PathBuf>> {
    let mut nodes: Vec<PathBuf> = fs::read_dir(INPUT_DIR)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_event_node(path))
        .collect();
    nodes.sort();
    Ok(nodes)
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("event"))
}

fn check_access() -> Result<()> {
    let nodes = event_nodes().with_context(|| format!("cannot list {INPUT_DIR}"))?;
    let denied = nodes.iter().all(|node| {
        fs::File::open(node).is_err_and(|err| err.kind() == io::ErrorKind::PermissionDenied)
    });
    if !nodes.is_empty() && denied {
        bail!(
            "no permission to read keyboards in {INPUT_DIR}; add yourself to the input group \
             with `sudo usermod -aG input $USER` and log in again"
        );
    }
    Ok(())
}

fn wanted(device: &Device, key: Option<KeyCode>) -> bool {
    if device.name() == Some(VIRTUAL_KEYBOARD_NAME) {
        return false;
    }
    device.supported_keys().is_some_and(|keys| match key {
        Some(key) => keys.contains(key),
        None => keys.iter().next().is_some(),
    })
}

/// The outcome of looking for devices to listen to.
#[derive(Default)]
struct Scan {
    /// The number of devices that are listened to now.
    started: usize,
    /// Whether some devices could not be opened for lack of permission.
    denied: bool,
}

impl Scan {
    /// Starts listening to the wanted devices that are not listened to yet.
    fn run(
        key: Option<KeyCode>,
        events: &UnboundedSender<KeyboardEvent>,
        listening: &Arc<Mutex<HashSet<PathBuf>>>,
    ) -> Self {
        let mut scan = Self::default();
        for path in event_nodes().unwrap_or_default() {
            if listening.lock().unwrap().contains(&path) {
                continue;
            }
            match Device::open(&path) {
                Ok(device) if wanted(&device, key) => {
                    if listen_to(device, path, events, listening) {
                        scan.started += 1;
                    }
                }
                Ok(_) => {}
                Err(err) => scan.denied |= err.kind() == io::ErrorKind::PermissionDenied,
            }
        }
        scan
    }
}

/// Starts forwarding the events of `device` on a thread of its own, and returns whether that
/// succeeded.
fn listen_to(
    device: Device,
    path: PathBuf,
    events: &UnboundedSender<KeyboardEvent>,
    listening: &Arc<Mutex<HashSet<PathBuf>>>,
) -> bool {
    let name = device.name().unwrap_or("unnamed device").to_owned();
    listening.lock().unwrap().insert(path.clone());
    let events = events.clone();
    let listening = listening.clone();
    let spawned = thread::Builder::new().name("input".into()).spawn(move || {
        let result = forward(device, &events);
        listening.lock().unwrap().remove(&path);
        let _ = events.send(KeyboardEvent::Unplugged);
        if let Err(err) = result {
            tracing::debug!("stopped listening to {}: {err}", path.display());
        }
    });
    match spawned {
        Ok(_) => {
            tracing::debug!("listening to {name}");
            true
        }
        Err(err) => {
            tracing::warn!("cannot listen to {name}: {err}");
            false
        }
    }
}

fn forward(mut device: Device, events: &UnboundedSender<KeyboardEvent>) -> io::Result<()> {
    loop {
        for event in device.fetch_events()? {
            if event.event_type() != EventType::KEY {
                continue;
            }
            let key = KeyboardEvent::Key(KeyCode::new(event.code()), event.value());
            if events.send(key).is_err() {
                return Ok(());
            }
        }
    }
}

/// Prints the names of the keys that are pressed, until interrupted.
pub async fn print_keys() -> Result<()> {
    let (sender, mut events) = mpsc::unbounded_channel();
    listen(None, sender)?;
    println!("Press keys to see their names; press Ctrl+C to quit.");
    while let Some(event) = events.recv().await {
        if let KeyboardEvent::Key(code, PRESSED) = event {
            println!("{code:?}");
        }
    }
    Ok(())
}

pub fn print_keyboards() -> Result<()> {
    check_access()?;
    println!("Input devices with keys:");
    for path in event_nodes()? {
        let Ok(device) = Device::open(&path) else {
            continue;
        };
        if wanted(&device, None) {
            let name = device.name().unwrap_or("unnamed device");
            println!("  {}: {name}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PushToTalk::{Cancel, Press, Release};
    use super::*;

    const PTT: KeyCode = KeyCode::KEY_F9;

    fn press(tracker: &mut Tracker, code: KeyCode) -> Option<PushToTalk> {
        tracker.handle(KeyboardEvent::Key(code, PRESSED))
    }

    fn release(tracker: &mut Tracker, code: KeyCode) -> Option<PushToTalk> {
        tracker.handle(KeyboardEvent::Key(code, RELEASED))
    }

    #[test]
    fn parses_key_names() {
        assert_eq!(parse_key("KEY_RIGHTCTRL").unwrap(), KeyCode::KEY_RIGHTCTRL);
        assert_eq!(parse_key("rightctrl").unwrap(), KeyCode::KEY_RIGHTCTRL);
        assert_eq!(parse_key(" f9 ").unwrap(), KeyCode::KEY_F9);
        assert_eq!(parse_key("BTN_SIDE").unwrap(), KeyCode::BTN_SIDE);
        assert_eq!(parse_key("Ctrl").unwrap(), KeyCode::KEY_LEFTCTRL);
        assert_eq!(parse_key("super").unwrap(), KeyCode::KEY_LEFTMETA);
        assert!(parse_key("KEY_NOPE").is_err());
    }

    #[test]
    fn press_and_release() {
        let mut tracker = Tracker::new(PTT);
        assert_eq!(press(&mut tracker, PTT), Some(Press));
        assert!(tracker.keys_held());
        assert_eq!(tracker.handle(KeyboardEvent::Key(PTT, 2)), None);
        assert_eq!(release(&mut tracker, PTT), Some(Release));
        assert!(!tracker.keys_held());
        assert_eq!(release(&mut tracker, PTT), None);
    }

    #[test]
    fn combination_cancels() {
        let mut tracker = Tracker::new(KeyCode::KEY_RIGHTCTRL);
        press(&mut tracker, KeyCode::KEY_RIGHTCTRL);
        assert_eq!(press(&mut tracker, KeyCode::KEY_C), Some(Cancel));
        assert_eq!(press(&mut tracker, KeyCode::KEY_V), None);
        assert_eq!(release(&mut tracker, KeyCode::KEY_RIGHTCTRL), None);
        assert_eq!(press(&mut tracker, KeyCode::KEY_RIGHTCTRL), Some(Press));
    }

    #[test]
    fn mouse_buttons_do_not_cancel() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, PTT);
        assert_eq!(press(&mut tracker, KeyCode::BTN_LEFT), None);
        assert_eq!(release(&mut tracker, PTT), Some(Release));
    }

    #[test]
    fn keys_held_before_the_press_do_not_cancel() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, KeyCode::KEY_A);
        assert_eq!(press(&mut tracker, PTT), Some(Press));
        assert_eq!(release(&mut tracker, KeyCode::KEY_A), None);
        assert_eq!(release(&mut tracker, PTT), Some(Release));
    }

    #[test]
    fn tracks_modifiers() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, KeyCode::KEY_LEFTMETA);
        assert!(tracker.keys_held());
        release(&mut tracker, KeyCode::KEY_LEFTMETA);
        assert!(!tracker.keys_held());
        press(&mut tracker, KeyCode::KEY_RIGHTSHIFT);
        tracker.handle(KeyboardEvent::Unplugged);
        assert!(!tracker.keys_held());
    }

    #[test]
    fn unplugging_releases_the_key() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, PTT);
        assert_eq!(tracker.handle(KeyboardEvent::Unplugged), Some(Release));
        assert_eq!(tracker.handle(KeyboardEvent::Unplugged), None);
    }
}
