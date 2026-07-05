use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by the Auricle engine.
#[derive(Debug)]
pub enum Error {
    /// A device named in the config was not found on the system.
    DeviceNotFound {
        name: String,
        available: Vec<String>,
    },
    /// No default device exists for the given direction ("input" / "output").
    NoDefaultDevice(&'static str),
    /// An audio backend (cpal/WASAPI) operation failed.
    Audio(String),
    /// Resampler construction or processing failed.
    Resample(String),
    /// Configuration file could not be read or parsed.
    Config(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DeviceNotFound { name, available } => {
                write!(f, "audio device not found: \"{name}\". available: ")?;
                if available.is_empty() {
                    write!(f, "(none)")
                } else {
                    write!(f, "{}", available.join(", "))
                }
            }
            Error::NoDefaultDevice(dir) => write!(f, "no default {dir} device on this system"),
            Error::Audio(msg) => write!(f, "audio backend error: {msg}"),
            Error::Resample(msg) => write!(f, "resampler error: {msg}"),
            Error::Config(msg) => write!(f, "config error: {msg}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
