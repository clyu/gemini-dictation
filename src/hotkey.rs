//! The push-to-talk key, read from the keyboards' evdev devices.
//!
//! Wayland does not let applications see keys pressed in other windows, so they are read from
//! `/dev/input`, which requires membership of the `input` group.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use evdev::{AttributeSetRef, Device, EventType, KeyCode};
use tokio::sync::mpsc::{self, UnboundedSender};

/// The name of the virtual keyboard that the paste output creates, which is not listened to.
pub const VIRTUAL_KEYBOARD_NAME: &str = "gemini-dictation virtual keyboard";

const INPUT_DIR: &str = "/dev/input";
/// How often to check for keyboards that were plugged in.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

const RELEASED: i32 = 0;
const PRESSED: i32 = 1;

/// Identifies a device among those listened to since the program started.
pub type DeviceId = u32;

static NEXT_DEVICE_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardEvent {
    /// A key of a device was released (0), pressed (1) or repeated (2).
    Key(DeviceId, KeyCode, i32),
    /// A device was unplugged, so the keys held on it will not be reported as released.
    Unplugged(DeviceId),
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

/// The keys that turn typed text into shortcuts while they are held.
const MODIFIERS: [KeyCode; 8] = [
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_RIGHTMETA,
];

fn is_modifier(code: KeyCode) -> bool {
    MODIFIERS.contains(&code)
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
    /// The device on which the key is held.
    down: Option<DeviceId>,
    chorded: bool,
    /// The modifiers that are held, each with the device it is held on.
    modifiers: HashSet<(DeviceId, KeyCode)>,
}

impl Tracker {
    pub fn new(key: KeyCode) -> Self {
        Self {
            key,
            down: None,
            chorded: false,
            modifiers: HashSet::new(),
        }
    }

    pub fn handle(&mut self, event: KeyboardEvent) -> Option<PushToTalk> {
        let (device, code, value) = match event {
            KeyboardEvent::Key(device, code, value) => (device, code, value),
            // Only the keys held on that device are released.
            KeyboardEvent::Unplugged(device) => {
                self.modifiers.retain(|&(holder, _)| holder != device);
                (device, self.key, RELEASED)
            }
        };
        if code == self.key {
            return match value {
                PRESSED if self.down.is_none() => {
                    self.down = Some(device);
                    self.chorded = false;
                    Some(PushToTalk::Press)
                }
                RELEASED if self.down == Some(device) => {
                    self.down = None;
                    (!self.chorded).then_some(PushToTalk::Release)
                }
                _ => None,
            };
        }
        if is_modifier(code) {
            match value {
                PRESSED => {
                    self.modifiers.insert((device, code));
                }
                RELEASED => {
                    self.modifiers.remove(&(device, code));
                }
                _ => {}
            }
        }
        if value == PRESSED && self.down.is_some() && !self.chorded && is_keyboard_key(code) {
            self.chorded = true;
            return Some(PushToTalk::Cancel);
        }
        None
    }

    /// Whether keys are held that would turn typed text into shortcuts.
    pub fn keys_held(&self) -> bool {
        self.down.is_some() || !self.modifiers.is_empty()
    }
}

/// Forwards the key events of the devices that have `key` (any keys if `None`) or modifier keys to
/// `events`, including devices that are plugged in later.
pub fn listen(key: Option<KeyCode>, events: UnboundedSender<KeyboardEvent>) -> Result<()> {
    check_access()?;
    let listening: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let mut modified = input_dir_modified();
    let mut scan = Scan::run(key, &events, &listening);
    if !scan.found_key {
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

/// Whether a device with `keys` has `key`, or any keys if `None`.
fn has_key(keys: &AttributeSetRef<KeyCode>, key: Option<KeyCode>) -> bool {
    match key {
        Some(key) => keys.contains(key),
        None => keys.iter().next().is_some(),
    }
}

/// Whether a device with `keys` is worth listening to: one that has `key`, or modifier keys, as
/// typing waits for the modifiers of every keyboard, even if `key` is elsewhere, such as on a
/// mouse.
fn has_wanted_keys(keys: &AttributeSetRef<KeyCode>, key: Option<KeyCode>) -> bool {
    has_key(keys, key) || MODIFIERS.iter().any(|&modifier| keys.contains(modifier))
}

fn wanted(device: &Device, key: Option<KeyCode>) -> bool {
    device.name() != Some(VIRTUAL_KEYBOARD_NAME)
        && device
            .supported_keys()
            .is_some_and(|keys| has_wanted_keys(keys, key))
}

/// The outcome of looking for devices to listen to.
#[derive(Default)]
struct Scan {
    /// Whether a device that has the key is listened to now.
    found_key: bool,
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
                    let with_key = device
                        .supported_keys()
                        .is_some_and(|keys| has_key(keys, key));
                    if listen_to(device, path, events, listening) {
                        scan.found_key |= with_key;
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
    let id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
    listening.lock().unwrap().insert(path.clone());
    let events = events.clone();
    let listening = listening.clone();
    let spawned = thread::Builder::new().name("input".into()).spawn(move || {
        let result = forward(id, device, &events);
        listening.lock().unwrap().remove(&path);
        let _ = events.send(KeyboardEvent::Unplugged(id));
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

fn forward(
    id: DeviceId,
    mut device: Device,
    events: &UnboundedSender<KeyboardEvent>,
) -> io::Result<()> {
    loop {
        for event in device.fetch_events()? {
            if event.event_type() != EventType::KEY {
                continue;
            }
            let key = KeyboardEvent::Key(id, KeyCode::new(event.code()), event.value());
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
        if let KeyboardEvent::Key(_, code, PRESSED) = event {
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
    use evdev::AttributeSet;

    const PTT: KeyCode = KeyCode::KEY_F9;
    const KEYBOARD: DeviceId = 0;
    const OTHER: DeviceId = 1;

    fn press_on(tracker: &mut Tracker, device: DeviceId, code: KeyCode) -> Option<PushToTalk> {
        tracker.handle(KeyboardEvent::Key(device, code, PRESSED))
    }

    fn release_on(tracker: &mut Tracker, device: DeviceId, code: KeyCode) -> Option<PushToTalk> {
        tracker.handle(KeyboardEvent::Key(device, code, RELEASED))
    }

    fn press(tracker: &mut Tracker, code: KeyCode) -> Option<PushToTalk> {
        press_on(tracker, KEYBOARD, code)
    }

    fn release(tracker: &mut Tracker, code: KeyCode) -> Option<PushToTalk> {
        release_on(tracker, KEYBOARD, code)
    }

    fn unplug(tracker: &mut Tracker, device: DeviceId) -> Option<PushToTalk> {
        tracker.handle(KeyboardEvent::Unplugged(device))
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
        assert_eq!(tracker.handle(KeyboardEvent::Key(KEYBOARD, PTT, 2)), None);
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
        unplug(&mut tracker, KEYBOARD);
        assert!(!tracker.keys_held());
    }

    #[test]
    fn tracks_the_modifiers_of_each_device() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, KeyCode::KEY_LEFTCTRL);
        press_on(&mut tracker, OTHER, KeyCode::KEY_LEFTCTRL);
        release(&mut tracker, KeyCode::KEY_LEFTCTRL);
        assert!(tracker.keys_held());
        release_on(&mut tracker, OTHER, KeyCode::KEY_LEFTCTRL);
        assert!(!tracker.keys_held());
    }

    #[test]
    fn listens_to_keyboards_without_the_key() {
        let keyboard = AttributeSet::from_iter([KeyCode::KEY_A, KeyCode::KEY_LEFTSHIFT]);
        let mouse = AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_SIDE]);
        let side = Some(KeyCode::BTN_SIDE);
        assert!(has_key(&mouse, side));
        assert!(!has_key(&keyboard, side));
        assert!(has_wanted_keys(&mouse, side));
        assert!(has_wanted_keys(&keyboard, side));
        assert!(!has_wanted_keys(&mouse, Some(PTT)));
        assert!(has_wanted_keys(&mouse, None));
        assert!(!has_wanted_keys(&AttributeSet::<KeyCode>::new(), None));
    }

    #[test]
    fn unplugging_releases_the_key() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, PTT);
        assert_eq!(unplug(&mut tracker, KEYBOARD), Some(Release));
        assert_eq!(unplug(&mut tracker, KEYBOARD), None);
    }

    #[test]
    fn unplugging_another_device_releases_nothing() {
        let mut tracker = Tracker::new(PTT);
        press(&mut tracker, KeyCode::KEY_LEFTSHIFT);
        press_on(&mut tracker, OTHER, KeyCode::KEY_LEFTALT);
        assert_eq!(press(&mut tracker, PTT), Some(Press));
        assert_eq!(unplug(&mut tracker, OTHER), None);
        assert_eq!(release(&mut tracker, PTT), Some(Release));
        // Shift is still held on the keyboard, unlike Alt on the unplugged device.
        assert!(tracker.keys_held());
        release(&mut tracker, KeyCode::KEY_LEFTSHIFT);
        assert!(!tracker.keys_held());
    }

    #[test]
    fn the_key_is_released_on_the_device_it_was_pressed_on() {
        let mut tracker = Tracker::new(PTT);
        assert_eq!(press(&mut tracker, PTT), Some(Press));
        assert_eq!(press_on(&mut tracker, OTHER, PTT), None);
        assert_eq!(release_on(&mut tracker, OTHER, PTT), None);
        assert!(tracker.keys_held());
        assert_eq!(release(&mut tracker, PTT), Some(Release));
    }
}
