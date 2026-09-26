//! Microphone capture, converted to the 16 kHz mono 16-bit PCM that the Live API expects.

use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::gemini::INPUT_SAMPLE_RATE;

pub type AudioSender = UnboundedSender<Vec<i16>>;

enum Command {
    Start {
        sink: AudioSender,
        reply: oneshot::Sender<Result<()>>,
    },
    Stop,
}

/// Records from the microphone on request. The microphone is only opened while recording.
///
/// cpal streams cannot move between threads, so they live on a thread of their own.
pub struct Recorder {
    commands: mpsc::Sender<Command>,
}

impl Recorder {
    pub fn spawn(mic: Option<String>) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || run(mic.as_deref(), receiver))
            .context("cannot start the audio thread")?;
        Ok(Self { commands })
    }

    /// Starts recording into `sink`. Recording continues until [`Recorder::stop`], after which
    /// `sink` is dropped, so that the receiving end sees the end of the audio.
    pub async fn start(&self, sink: AudioSender) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Start { sink, reply })
            .map_err(|_| anyhow!("the audio thread has stopped"))?;
        result
            .await
            .map_err(|_| anyhow!("the audio thread has stopped"))?
    }

    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

/// Where a stream's callback delivers audio. Emptied when recording stops, which closes the
/// channel even if the audio backend holds on to the callback for a while.
type Slot = Arc<Mutex<Option<AudioSender>>>;

fn run(mic: Option<&str>, commands: mpsc::Receiver<Command>) {
    let mut active: Option<(cpal::Stream, Slot)> = None;
    for command in commands {
        if let Some((stream, slot)) = active.take() {
            slot.lock().unwrap().take();
            drop(stream);
        }
        if let Command::Start { sink, reply } = command {
            let slot = Arc::new(Mutex::new(Some(sink)));
            match open_stream(mic, slot.clone()) {
                Ok(stream) => {
                    active = Some((stream, slot));
                    let _ = reply.send(Ok(()));
                }
                Err(err) => {
                    let _ = reply.send(Err(err));
                }
            }
        }
    }
}

fn find_device(host: &cpal::Host, mic: Option<&str>) -> Result<cpal::Device> {
    let Some(name) = mic else {
        return host
            .default_input_device()
            .context("there is no default microphone");
    };
    host.input_devices()
        .context("cannot list microphones")?
        .find(|device| device_name(device) == name)
        .with_context(|| format!("there is no microphone named {name:?}"))
}

pub fn device_name(device: &cpal::Device) -> String {
    device.to_string()
}

fn open_stream(mic: Option<&str>, slot: Slot) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = find_device(&host, mic)?;
    let supported = device
        .default_input_config()
        .with_context(|| format!("cannot configure microphone {device}"))?;
    let config = supported.config();
    let converter = Converter::new(config.channels, config.sample_rate);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, config, converter, slot),
        SampleFormat::F64 => build_stream::<f64>(&device, config, converter, slot),
        SampleFormat::I8 => build_stream::<i8>(&device, config, converter, slot),
        SampleFormat::I16 => build_stream::<i16>(&device, config, converter, slot),
        SampleFormat::I32 => build_stream::<i32>(&device, config, converter, slot),
        SampleFormat::U8 => build_stream::<u8>(&device, config, converter, slot),
        SampleFormat::U16 => build_stream::<u16>(&device, config, converter, slot),
        SampleFormat::U32 => build_stream::<u32>(&device, config, converter, slot),
        format => bail!("microphone {device} uses unsupported sample format {format}"),
    }
    .with_context(|| format!("cannot open microphone {device}"))?;
    stream
        .play()
        .with_context(|| format!("cannot start recording from microphone {device}"))?;
    tracing::debug!(
        "recording from {device} ({} channels at {} Hz)",
        config.channels,
        config.sample_rate
    );
    Ok(stream)
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut converter: Converter,
    slot: Slot,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let samples = data.iter().map(|&s| s.to_sample::<f32>());
            let samples = converter.process(samples);
            if samples.is_empty() {
                return;
            }
            if let Some(sink) = slot.lock().unwrap().as_ref() {
                let _ = sink.send(samples);
            }
        },
        |err| tracing::warn!("microphone error: {err}"),
        None,
    )
}

/// Downmixes interleaved samples to mono and resamples them to [`INPUT_SAMPLE_RATE`].
///
/// Downsampling averages the input samples that fall in each output period, which also serves as
/// a crude low-pass filter; upsampling repeats samples. Both are good enough for speech.
pub struct Converter {
    channels: usize,
    /// Input samples per output sample.
    ratio: f64,
    frame_sum: f32,
    frame_len: usize,
    sum: f32,
    len: u32,
    phase: f64,
    last: f32,
}

impl Converter {
    pub fn new(channels: u16, sample_rate: u32) -> Self {
        Self {
            channels: usize::from(channels.max(1)),
            ratio: f64::from(sample_rate) / f64::from(INPUT_SAMPLE_RATE),
            frame_sum: 0.0,
            frame_len: 0,
            sum: 0.0,
            len: 0,
            phase: 0.0,
            last: 0.0,
        }
    }

    pub fn process(&mut self, samples: impl IntoIterator<Item = f32>) -> Vec<i16> {
        let mut out = Vec::new();
        for sample in samples {
            self.frame_sum += sample;
            self.frame_len += 1;
            if self.frame_len < self.channels {
                continue;
            }
            let mono = self.frame_sum / self.channels as f32;
            self.frame_sum = 0.0;
            self.frame_len = 0;

            self.sum += mono;
            self.len += 1;
            self.phase += 1.0;
            while self.phase >= self.ratio {
                if self.len > 0 {
                    self.last = self.sum / self.len as f32;
                }
                out.push(to_i16(self.last));
                self.sum = 0.0;
                self.len = 0;
                self.phase -= self.ratio;
            }
        }
        out
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

pub fn print_devices() -> Result<()> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .map(|device| device_name(&device));
    println!("Microphones (for --mic):");
    for device in host.input_devices().context("cannot list microphones")? {
        let name = device_name(&device);
        let marker = if Some(&name) == default.as_ref() {
            " (default)"
        } else {
            ""
        };
        println!("  {name}{marker}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_16k_mono_through() {
        let mut converter = Converter::new(1, 16_000);
        let samples = converter.process([0.0, 0.5, -0.5, 1.0]);
        assert_eq!(samples, [0, 16384, -16384, 32767]);
    }

    #[test]
    fn downmixes_stereo() {
        let mut converter = Converter::new(2, 16_000);
        let samples = converter.process([1.0, 0.0, -0.25, -0.75]);
        assert_eq!(samples, [16384, -16384]);
    }

    #[test]
    fn keeps_partial_frames_for_the_next_call() {
        let mut converter = Converter::new(2, 16_000);
        assert!(converter.process([1.0]).is_empty());
        assert_eq!(converter.process([0.0]), [16384]);
    }

    #[test]
    fn downsamples_48k_by_averaging() {
        let mut converter = Converter::new(1, 48_000);
        let samples = converter.process([0.0, 0.25, 0.5, 1.0, 1.0, 1.0]);
        assert_eq!(samples, [8192, 32767]);
    }

    #[test]
    fn downsamples_44k1_at_the_right_rate() {
        let mut converter = Converter::new(2, 44_100);
        let mut produced = 0;
        // One second of audio, delivered in chunks of 441 frames.
        for _ in 0..100 {
            produced += converter.process([0.1; 882]).len();
        }
        assert!((15_999..=16_000).contains(&produced), "{produced}");
    }

    #[test]
    fn upsamples_8k_by_repeating() {
        let mut converter = Converter::new(1, 8_000);
        let samples = converter.process([0.5, -0.5]);
        assert_eq!(samples, [16384, 16384, -16384, -16384]);
    }

    #[test]
    fn clamps_out_of_range_samples() {
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), -i16::MAX);
    }
}
