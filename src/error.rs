//! Error type for `pipewire-vircam`.

/// Construction / negotiation failure.
///
/// Only the failure modes that actually occur during [`crate::Camera::new`]
/// are carried here; runtime stream errors are reported via
/// [`crate::State::Disconnected { error: Some(..) }`], not as an `Error`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The config is invalid (no modes, empty mode, zero fps, ...).
    InvalidConfig(String),
    /// Failed to connect to PipeWire (main loop, context, or stream creation).
    Connect(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            Error::Connect(msg) => write!(f, "connect failed: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::Error;

    /// The `Display` strings are part of the operator-facing contract (the
    /// harness greps for them), so pin them.
    #[test]
    fn display_strings() {
        assert_eq!(
            Error::InvalidConfig("x".into()).to_string(),
            "invalid config: x"
        );
        assert_eq!(Error::Connect("y".into()).to_string(), "connect failed: y");
    }

    #[test]
    fn error_is_std_error() {
        let e: &dyn std::error::Error = &Error::Connect("c".into());
        assert!(e.to_string().contains("connect failed"));
    }
}
