//! Error type for `pipewire-vircam`.

/// Construction / negotiation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The config is invalid (no modes, empty mode, zero fps, ...).
    InvalidConfig(String),
    /// Failed to connect to PipeWire.
    Connect(String),
    /// The stream reported an error state.
    Stream(String),
    /// The negotiated format is not supported by the camera.
    UnsupportedFormat(String),
    /// A PipeWire call failed with a raw C error code (< 0).
    PipeWire(i32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            Error::Connect(msg) => write!(f, "connect failed: {msg}"),
            Error::Stream(msg) => write!(f, "stream error: {msg}"),
            Error::UnsupportedFormat(msg) => write!(f, "unsupported format: {msg}"),
            Error::PipeWire(code) => write!(f, "pipewire error: {code}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;

    /// The `Display` strings are part of the operator-facing contract (the
    /// harness greps for `stream state: "error" <msg>`), so pin them.
    #[test]
    fn display_strings() {
        assert_eq!(
            Error::InvalidConfig("x".into()).to_string(),
            "invalid config: x"
        );
        assert_eq!(Error::Connect("y".into()).to_string(), "connect failed: y");
        assert_eq!(Error::Stream("z".into()).to_string(), "stream error: z");
        assert_eq!(
            Error::UnsupportedFormat("i420".into()).to_string(),
            "unsupported format: i420"
        );
        assert_eq!(Error::PipeWire(-12).to_string(), "pipewire error: -12");
    }

    #[test]
    fn error_is_std_error() {
        let e: &dyn std::error::Error = &Error::Stream("s".into());
        assert!(e.to_string().contains("stream error"));
    }
}
