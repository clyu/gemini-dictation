# gemini-dictation

Push-to-talk dictation for Linux Wayland desktops. Hold a key, speak, and release it: the speech is transcribed by the [Gemini Live API](https://ai.google.dev/gemini-api/docs/live-api) and typed into the focused window.

## Features

- Push-to-talk on any key or mouse button (Right Ctrl by default), read from the evdev devices so that it works under every Wayland compositor
- Transcription by the Live API's input audio transcription (`inputAudioTranscription`), with the model defaulting to `gemini-3.8-live`
  - Smart mode (default) removes filler words and false starts and tidies up punctuation; verbatim mode transcribes exactly what was said
  - Language hints (Traditional Chinese and English by default) and custom vocabulary
- One Live API session per recording, framed with `activityStart` / `activityEnd` as automatic voice activity detection is disabled. The connection is only opened once the key has been held for 250 ms, so that taps of the key (and shortcuts such as Ctrl+C when the key is Right Ctrl) never reach Gemini; audio recorded in the meantime is buffered
- The transcript is typed as it arrives, but only once the push-to-talk key and all modifier keys are released, so that it cannot trigger shortcuts
- Output methods:
  - `type`: types the text with [wtype](https://github.com/atx/wtype) (compositors with the virtual keyboard protocol, such as Sway, Hyprland and river)
  - `paste`: copies the text with `wl-copy` and presses Shift+Insert on a virtual uinput keyboard, which pastes in applications and terminals alike (any compositor, including GNOME and KDE Plasma)
  - `clipboard`: only copies the text
  - `stdout`: prints the text
  - `auto` (default): `type` if wtype is installed and the desktop is neither GNOME nor KDE, `paste` otherwise
- `gemini-dictation ctl start|stop|toggle|cancel` controls the running instance, for compositor key bindings

## Installation

Download the `gemini-dictation-<version>-<build>-x86_64-linux.tar.gz` archive of the latest build from the [Latest Build](../../releases) pre-release (or from the artifacts of a workflow run), and put the `gemini-dictation` binary somewhere on your `PATH`.

Runtime requirements:

- ALSA's `libasound.so.2` (on PipeWire and PulseAudio systems, the ALSA plugin of the sound server provides the default microphone)
- `wtype` for the `type` output, `wl-clipboard` for the `paste` and `clipboard` outputs

### Permissions

Reading the push-to-talk key needs read access to `/dev/input/event*`, which members of the `input` group have:

```
sudo usermod -aG input $USER
```

The `paste` output creates a virtual keyboard, which needs write access to `/dev/uinput`. For example, to give it to the `input` group:

```
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/99-gemini-dictation-uinput.rules
sudo modprobe uinput
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Log in again for the group membership to take effect. Note that any program running as a member of the `input` group can read every key typed, and with `/dev/uinput` access, type keys too.

## Usage

```
export GEMINI_API_KEY=...
gemini-dictation
```

Hold Right Ctrl, speak, and release it. Pressing another key while holding it cancels the recording.

| Option | Default | Description |
| --- | --- | --- |
| `--api-key` | `$GEMINI_API_KEY` | Gemini API key |
| `--model` | `gemini-3.8-live` | Live API model (also `$GEMINI_DICTATION_MODEL`) |
| `--key` | `KEY_RIGHTCTRL` | Push-to-talk key; `gemini-dictation keys` prints the names of the keys pressed |
| `--language` | `zh-Hant,en` | Language hints, comma-separated or repeated; `auto` for automatic detection. Without a hint, Mandarin is transcribed in Simplified Chinese |
| `--vocabulary` | | Phrase to favor, such as a name or a product; can be repeated |
| `--transcription-mode` | `smart` | `smart` or `verbatim` |
| `--output` | `auto` | `auto`, `type`, `paste`, `clipboard` or `stdout` |
| `--paste-keys` | `shift+insert` | Keys pressed by the `paste` output, such as `ctrl+v` |
| `--mic` | default input | Microphone, as listed by `gemini-dictation devices` |
| `--min-hold-ms` | `250` | Shorter presses are ignored |
| `--max-record-secs` | `300` | Recordings are stopped after this long |
| `--no-hotkey` | | Do not read keyboards; only `gemini-dictation ctl` works |
| `-v`, `--verbose` | | Log debugging details, such as the interim transcripts (`RUST_LOG` also works) |

### Compositor key bindings

Without access to `/dev/input`, or to use a key combination, bind `gemini-dictation ctl` in the compositor instead. For example, push-to-talk on Super+Space in Sway:

```
bindsym --no-repeat Mod4+space exec gemini-dictation ctl start
bindsym --release Mod4+space exec gemini-dictation ctl stop
```

or in Hyprland:

```
bind = SUPER, space, exec, gemini-dictation ctl start
bindr = SUPER, space, exec, gemini-dictation ctl stop
```

`gemini-dictation ctl toggle` suits desktops whose shortcuts cannot react to key releases, such as GNOME.

To start gemini-dictation with the session, run it from the compositor's autostart (such as `exec gemini-dictation` in Sway), or as a systemd user service with the API key in an environment file.

## Notes

- The model's own spoken reply is not used; the system instruction asks it to stay silent.
- Transcripts are logged on standard error.
- The `type` output types line breaks as Shift+Enter, which starts a new line rather than sending the message in chat applications.
- The `paste` output replaces the contents of both the clipboard and the primary selection. Shift+Insert pastes the clipboard in most applications, but the primary selection in terminals such as GNOME Terminal, in which Ctrl+V does not paste at all.
- Consecutive transcripts are separated by a space when both sides are Latin letters, digits or punctuation, as they are dictated without knowing the text around the cursor.

## Building

GitHub Actions (`.github/workflows/ci.yml`) runs these jobs on every push:

- `test`: `cargo test --locked`
- `lint`: `cargo fmt --check` and `cargo clippy`
- `build`: `cargo build --release --locked`, and uploads `gemini-dictation-<version>-<run number>-x86_64-linux.tar.gz` as a workflow artifact. The run number is also part of the version that `gemini-dictation --version` prints, such as `0.1.0+42 (1a2b3c4)`.
- `publish-latest`: on the `master` branch, once `build` and `test` have passed, publishes the archive as the "Latest Build" pre-release under the tag `ci-<run number>`, and deletes the Latest Build releases of earlier runs

To build locally, install Rust 1.88 or later and the ALSA development files (`libasound2-dev` on Debian and Ubuntu, `alsa-lib-devel` on Fedora), then run `cargo build --release --locked`.
