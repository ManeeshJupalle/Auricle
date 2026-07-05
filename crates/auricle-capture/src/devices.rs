use auricle_core::{Error, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// How a device can be captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Regular input device (microphone).
    Input,
    /// Output device captured via WASAPI loopback.
    Loopback,
}

/// A capture-capable device and its default (shared-mode mix) format.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: String,
}

/// List input devices and loopback-capable output devices with their
/// default shared-mode formats.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let mut out = Vec::new();

    let default_input = host.default_input_device().map(|d| d.to_string());
    let default_output = host.default_output_device().map(|d| d.to_string());

    let inputs = host
        .input_devices()
        .map_err(|e| Error::Audio(format!("enumerating input devices: {e}")))?;
    for dev in inputs {
        let name = dev.to_string();
        if let Ok(cfg) = dev.default_input_config() {
            out.push(DeviceInfo {
                is_default: default_input.as_deref() == Some(name.as_str()),
                name,
                kind: DeviceKind::Input,
                sample_rate_hz: cfg.sample_rate(),
                channels: cfg.channels(),
                sample_format: format!("{:?}", cfg.sample_format()),
            });
        }
    }

    let outputs = host
        .output_devices()
        .map_err(|e| Error::Audio(format!("enumerating output devices: {e}")))?;
    for dev in outputs {
        let name = dev.to_string();
        // Loopback streams are configured from the device's OUTPUT config;
        // default_input_config() fails on render devices (probe-verified).
        if let Ok(cfg) = dev.default_output_config() {
            out.push(DeviceInfo {
                is_default: default_output.as_deref() == Some(name.as_str()),
                name,
                kind: DeviceKind::Loopback,
                sample_rate_hz: cfg.sample_rate(),
                channels: cfg.channels(),
                sample_format: format!("{:?}", cfg.sample_format()),
            });
        }
    }

    Ok(out)
}

/// Resolve a selector ("default" or an exact device name) to an input device.
pub(crate) fn resolve_input_device(host: &cpal::Host, selector: &str) -> Result<cpal::Device> {
    if selector == "default" {
        return host
            .default_input_device()
            .ok_or(Error::NoDefaultDevice("input"));
    }
    let mut available = Vec::new();
    let devices = host
        .input_devices()
        .map_err(|e| Error::Audio(format!("enumerating input devices: {e}")))?;
    for dev in devices {
        let name = dev.to_string();
        if name == selector {
            return Ok(dev);
        }
        available.push(name);
    }
    Err(Error::DeviceNotFound {
        name: selector.to_string(),
        available,
    })
}

/// Resolve a selector ("default" or an exact device name) to an output device
/// (for loopback capture).
pub(crate) fn resolve_output_device(host: &cpal::Host, selector: &str) -> Result<cpal::Device> {
    if selector == "default" {
        return host
            .default_output_device()
            .ok_or(Error::NoDefaultDevice("output"));
    }
    let mut available = Vec::new();
    let devices = host
        .output_devices()
        .map_err(|e| Error::Audio(format!("enumerating output devices: {e}")))?;
    for dev in devices {
        let name = dev.to_string();
        if name == selector {
            return Ok(dev);
        }
        available.push(name);
    }
    Err(Error::DeviceNotFound {
        name: selector.to_string(),
        available,
    })
}
