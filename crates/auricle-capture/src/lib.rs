//! Windows audio capture for Auricle.
//!
//! Empirical results from the Phase 1 capture probe (see
//! `docs/PHASE1_CAPTURE_REPORT.md`) drive two decisions baked in here:
//!
//! 1. Loopback uses cpal's WASAPI host directly: building an *input* stream on
//!    an *output* device transparently enables `AUDCLNT_STREAMFLAGS_LOOPBACK`.
//!    The `wasapi` crate fallback was not needed.
//! 2. On output devices, `default_input_config()` fails; the loopback stream
//!    must be configured from `default_output_config()`.
//!
//! Real-time capture callbacks are allocation-free and lock-free: they only
//! downmix to mono and push into an `rtrb` ring buffer, counting drops in an
//! atomic when the buffer is full.

mod capture;
mod devices;
mod resample;
mod ring;

pub use capture::{start_loopback, start_mic, CaptureConsumer, CaptureHandle};
pub use devices::{enumerate, DeviceInfo, DeviceKind};
pub use resample::MonoResampler;
pub use ring::drain_into;
