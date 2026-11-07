use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use auricle_core::{ChannelId, Error, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::devices::{resolve_input_device, resolve_output_device};
use crate::ring::push_downmixed;

/// Seconds of mono audio each ring buffer can hold before dropping samples.
/// The drain side runs every few milliseconds, so 4 s is a wide safety margin.
const RING_SECONDS: u32 = 4;

/// Owns the live cpal stream. Keep this alive for as long as capture should
/// run; drop (or `pause`) it to stop the callbacks.
pub struct CaptureHandle {
    stream: cpal::Stream,
    pub channel: ChannelId,
    pub device_name: String,
    pub native_rate_hz: u32,
    pub native_channels: u16,
    pub sample_format: String,
    error: Arc<Mutex<Option<String>>>,
}

impl CaptureHandle {
    /// Stop the data callbacks. Pending ring-buffer contents stay drainable.
    pub fn pause(&self) -> Result<()> {
        self.stream
            .pause()
            .map_err(|e| Error::Audio(format!("pausing stream: {e}")))
    }

    /// First fatal stream error reported by the backend (e.g. device
    /// unplugged), if any. The capture session keeps running on the surviving
    /// channel. Notices from a stream that is still running never appear.
    pub fn stream_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }
}

/// The drain side of a capture channel: consume mono f32 samples at the
/// device's native rate, plus the drop counter fed by the callback.
pub struct CaptureConsumer {
    pub channel: ChannelId,
    pub consumer: Consumer<f32>,
    pub dropped: Arc<AtomicU64>,
    pub native_rate_hz: u32,
}

/// Start microphone capture on the selected input device
/// ("default" or an exact device name).
pub fn start_mic(selector: &str) -> Result<(CaptureHandle, CaptureConsumer)> {
    let host = cpal::default_host();
    let device = resolve_input_device(&host, selector)?;
    let cfg = device
        .default_input_config()
        .map_err(|e| Error::Audio(format!("querying input config of \"{device}\": {e}")))?;
    start_stream(device, cfg, ChannelId::Mic)
}

/// Start WASAPI loopback capture on the selected output device
/// ("default" or an exact device name).
pub fn start_loopback(selector: &str) -> Result<(CaptureHandle, CaptureConsumer)> {
    let host = cpal::default_host();
    let device = resolve_output_device(&host, selector)?;
    // Probe-verified: on WASAPI render devices the input-config APIs fail;
    // the loopback stream must be configured from the output (mix) format.
    // cpal enables AUDCLNT_STREAMFLAGS_LOOPBACK transparently when an input
    // stream is built on a render device.
    let cfg = device
        .default_output_config()
        .map_err(|e| Error::Audio(format!("querying output config of \"{device}\": {e}")))?;
    start_stream(device, cfg, ChannelId::Loopback)
}

fn start_stream(
    device: cpal::Device,
    cfg: cpal::SupportedStreamConfig,
    channel: ChannelId,
) -> Result<(CaptureHandle, CaptureConsumer)> {
    let device_name = device.to_string();
    let native_rate_hz = cfg.sample_rate();
    let native_channels = cfg.channels();
    let sample_format = cfg.sample_format();

    let ring_capacity = (native_rate_hz * RING_SECONDS) as usize;
    let (producer, consumer) = RingBuffer::<f32>::new(ring_capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    let error = Arc::new(Mutex::new(None));

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(
            &device,
            cfg.config(),
            native_channels,
            producer,
            dropped.clone(),
            error.clone(),
        ),
        cpal::SampleFormat::I16 => build_stream::<i16>(
            &device,
            cfg.config(),
            native_channels,
            producer,
            dropped.clone(),
            error.clone(),
        ),
        cpal::SampleFormat::I32 => build_stream::<i32>(
            &device,
            cfg.config(),
            native_channels,
            producer,
            dropped.clone(),
            error.clone(),
        ),
        cpal::SampleFormat::U8 => build_stream::<u8>(
            &device,
            cfg.config(),
            native_channels,
            producer,
            dropped.clone(),
            error.clone(),
        ),
        other => Err(Error::Audio(format!(
            "unsupported sample format {other:?} on \"{device_name}\""
        ))),
    }?;

    stream
        .play()
        .map_err(|e| Error::Audio(format!("starting stream on \"{device_name}\": {e}")))?;

    Ok((
        CaptureHandle {
            stream,
            channel,
            device_name,
            native_rate_hz,
            native_channels,
            sample_format: format!("{sample_format:?}"),
            error,
        },
        CaptureConsumer {
            channel,
            consumer,
            dropped,
            native_rate_hz,
        },
    ))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: u16,
    mut producer: Producer<f32>,
    dropped: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let ch = channels as usize;
    device
        .build_input_stream::<T, _, _>(
            config,
            // Real-time data callback: allocation-free, lock-free. Downmix to
            // mono and push into the ring buffer; count drops in an atomic.
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                push_downmixed(data, ch, &mut producer, &dropped);
            },
            // Error callback: runs on a non-real-time backend thread, so a
            // mutex is fine here (this is not the capture data path).
            move |e| record_stream_error(&error, &e),
            None,
        )
        .map_err(|e| Error::Audio(format!("building input stream: {e}")))
}

/// Whether the stream is gone. cpal reports through one callback both real
/// failures and notices from a stream that carries on: a default-device
/// stream reroutes itself (`DeviceChanged`), a glitch costs a few samples
/// (`Xrun`), and a refused priority boost changes nothing (`RealtimeDenied`).
/// Kinds cpal adds later count as fatal until shown otherwise.
fn is_fatal(kind: cpal::ErrorKind) -> bool {
    use cpal::ErrorKind::{DeviceChanged, RealtimeDenied, Xrun};
    !matches!(kind, DeviceChanged | Xrun | RealtimeDenied)
}

/// Keep the first fatal error, ignoring notices from a stream that is still
/// running: they would read as a lost device and mask a later real error.
fn record_stream_error(slot: &Mutex<Option<String>>, e: &cpal::Error) {
    if !is_fatal(e.kind()) {
        return;
    }
    if let Ok(mut guard) = slot.lock() {
        guard.get_or_insert_with(|| e.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::ErrorKind;

    #[test]
    fn notices_from_a_stream_that_keeps_running_are_not_fatal() {
        for kind in [
            ErrorKind::DeviceChanged,
            ErrorKind::Xrun,
            ErrorKind::RealtimeDenied,
        ] {
            assert!(!is_fatal(kind), "{kind:?}");
        }
    }

    #[test]
    fn errors_that_end_the_stream_are_fatal() {
        for kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::BackendError,
            ErrorKind::Other,
        ] {
            assert!(is_fatal(kind), "{kind:?}");
        }
    }

    #[test]
    fn a_reroute_alone_records_nothing() {
        let slot = Mutex::new(None);
        record_stream_error(&slot, &cpal::Error::new(ErrorKind::DeviceChanged));
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn a_reroute_does_not_mask_a_later_real_error() {
        let slot = Mutex::new(None);
        record_stream_error(&slot, &cpal::Error::new(ErrorKind::DeviceChanged));
        record_stream_error(
            &slot,
            &cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "unplugged"),
        );
        assert_eq!(slot.lock().unwrap().as_deref(), Some("unplugged"));
    }

    #[test]
    fn the_first_fatal_error_is_kept() {
        let slot = Mutex::new(None);
        record_stream_error(
            &slot,
            &cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "unplugged"),
        );
        record_stream_error(
            &slot,
            &cpal::Error::with_message(ErrorKind::BackendError, "aftermath"),
        );
        record_stream_error(&slot, &cpal::Error::new(ErrorKind::DeviceChanged));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("unplugged"));
    }
}
