//! Push-to-talk recording from the default microphone.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};

/// A recording stops on its own after this long.
pub const MAX_RECORDING: Duration = Duration::from_secs(5 * 60);

/// A running recording, mixed down to mono at the device's rate. Dropping it stops the microphone.
pub struct Recorder {
    _stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
    rate: u32,
    started: Instant,
}

impl Recorder {
    pub fn start() -> Result<Recorder, String> {
        let device = cpal::default_host().default_input_device().ok_or("no microphone found")?;
        let supported = device.default_input_config().map_err(|e| format!("microphone unavailable: {e}"))?;
        let config = supported.config();
        let rate = config.sample_rate;
        let samples = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 30)));
        let error = Arc::new(Mutex::new(None));
        let input = Input {
            channels: (config.channels as usize).max(1),
            limit: rate as usize * MAX_RECORDING.as_secs() as usize,
            samples: samples.clone(),
            error: error.clone(),
        };
        let stream = match supported.sample_format() {
            SampleFormat::F32 => input.open::<f32>(&device, config),
            SampleFormat::F64 => input.open::<f64>(&device, config),
            SampleFormat::I8 => input.open::<i8>(&device, config),
            SampleFormat::I16 => input.open::<i16>(&device, config),
            SampleFormat::I32 => input.open::<i32>(&device, config),
            SampleFormat::U8 => input.open::<u8>(&device, config),
            SampleFormat::U16 => input.open::<u16>(&device, config),
            SampleFormat::U32 => input.open::<u32>(&device, config),
            other => return Err(format!("unsupported microphone format {other}")),
        }
        .map_err(|e| format!("could not open the microphone: {e}"))?;
        stream.play().map_err(|e| format!("could not start the microphone: {e}"))?;
        Ok(Recorder { _stream: stream, samples, error, rate, started: Instant::now() })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Stops the microphone and returns the mono samples and their rate. What was
    /// recorded before the microphone failed is kept.
    pub fn finish(self) -> Result<(Vec<f32>, u32), String> {
        let Recorder { _stream, samples, error, rate, .. } = self;
        drop(_stream);
        let samples = std::mem::take(&mut *samples.lock().unwrap());
        match error.lock().unwrap().take() {
            Some(err) if samples.is_empty() => Err(format!("the microphone failed: {err}")),
            _ => Ok((samples, rate)),
        }
    }
}

struct Input {
    channels: usize,
    limit: usize,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Input {
    fn open<T>(self, device: &cpal::Device, config: cpal::StreamConfig) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let Input { channels, limit, samples, error } = self;
        device.build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut samples = samples.lock().unwrap();
                if samples.len() < limit {
                    let mono = data
                        .chunks(channels)
                        .map(|frame| frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32);
                    samples.extend(mono);
                }
            },
            move |err: cpal::Error| {
                // Glitches and reroutes leave the recording usable.
                if !matches!(err.kind(), ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied) {
                    *error.lock().unwrap() = Some(err.to_string());
                }
            },
            None,
        )
    }
}
