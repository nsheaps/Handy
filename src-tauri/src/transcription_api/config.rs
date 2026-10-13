//! Validation of the user-supplied listener configuration.

use crate::settings::TranscriptionApiSettings;
use std::net::{IpAddr, SocketAddr};

/// Ports below this need elevated privileges on Unix-like systems.
pub const MIN_UNPRIVILEGED_PORT: u16 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    InvalidAddress,
    InvalidPort,
    NonLoopbackNotConfirmed,
    CredentialRequired,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidAddress => "The bind address is not a valid IP address",
            Self::InvalidPort => "The port must be between 1024 and 65535",
            Self::NonLoopbackNotConfirmed => {
                "Listening beyond this computer must be confirmed explicitly"
            }
            Self::CredentialRequired => {
                "A credential is required before listening beyond this computer"
            }
        })
    }
}

impl std::error::Error for ConfigError {}

/// A validated listener address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenConfig {
    pub addr: SocketAddr,
}

impl ListenConfig {
    /// Validate the persisted settings. `has_credential` is whether a bearer
    /// token exists in the secure store; the server never starts without one
    /// (see [`super::TranscriptionApiManager`]), but non-loopback additionally
    /// reports a distinct error so the UI can explain it.
    pub fn from_settings(
        settings: &TranscriptionApiSettings,
        has_credential: bool,
    ) -> Result<Self, ConfigError> {
        let ip = parse_ip(&settings.bind_address)?;
        if settings.port < MIN_UNPRIVILEGED_PORT {
            return Err(ConfigError::InvalidPort);
        }
        if !ip.is_loopback() {
            if !settings.allow_non_loopback {
                return Err(ConfigError::NonLoopbackNotConfirmed);
            }
            if !has_credential {
                return Err(ConfigError::CredentialRequired);
            }
        }
        Ok(Self {
            addr: SocketAddr::new(ip, settings.port),
        })
    }

    pub fn is_loopback(&self) -> bool {
        self.addr.ip().is_loopback()
    }

    /// The URL clients should use for the transcription endpoint. A wildcard
    /// bind has no single reachable address, so the loopback form is shown and
    /// the user substitutes the machine's address.
    pub fn endpoint_url(&self) -> String {
        endpoint_url(self.addr.ip(), self.addr.port())
    }
}

pub fn parse_ip(value: &str) -> Result<IpAddr, ConfigError> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .map_err(|_| ConfigError::InvalidAddress)
}

pub fn endpoint_url(ip: IpAddr, port: u16) -> String {
    let host = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => "127.0.0.1".to_string(),
        IpAddr::V6(v6) if v6.is_unspecified() => "[::1]".to_string(),
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{port}/v1/audio/transcriptions")
}

/// Whether a bind address in settings exposes the API beyond this computer.
/// Unparseable values count as exposed so the UI never under-warns.
pub fn is_exposed(bind_address: &str) -> bool {
    parse_ip(bind_address).map_or(true, |ip| !ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(addr: &str, port: u16, allow: bool) -> TranscriptionApiSettings {
        TranscriptionApiSettings {
            enabled: true,
            bind_address: addr.to_string(),
            port,
            allow_non_loopback: allow,
            ..Default::default()
        }
    }

    #[test]
    fn loopback_default_is_accepted() {
        let cfg = ListenConfig::from_settings(&TranscriptionApiSettings::default(), true).unwrap();
        assert!(cfg.is_loopback());
        assert_eq!(
            cfg.addr.port(),
            crate::settings::DEFAULT_TRANSCRIPTION_API_PORT
        );
    }

    #[test]
    fn ipv6_loopback_with_brackets_is_accepted() {
        let cfg = ListenConfig::from_settings(&settings("[::1]", 24639, false), true).unwrap();
        assert!(cfg.is_loopback());
        assert_eq!(
            cfg.endpoint_url(),
            "http://[::1]:24639/v1/audio/transcriptions"
        );
    }

    #[test]
    fn non_loopback_requires_confirmation() {
        assert_eq!(
            ListenConfig::from_settings(&settings("0.0.0.0", 24639, false), true),
            Err(ConfigError::NonLoopbackNotConfirmed)
        );
        assert_eq!(
            ListenConfig::from_settings(&settings("192.168.1.5", 24639, false), true),
            Err(ConfigError::NonLoopbackNotConfirmed)
        );
    }

    #[test]
    fn non_loopback_requires_credential() {
        assert_eq!(
            ListenConfig::from_settings(&settings("0.0.0.0", 24639, true), false),
            Err(ConfigError::CredentialRequired)
        );
        assert!(ListenConfig::from_settings(&settings("0.0.0.0", 24639, true), true).is_ok());
    }

    #[test]
    fn rejects_hostnames_and_privileged_ports() {
        assert_eq!(
            ListenConfig::from_settings(&settings("localhost", 24639, false), true),
            Err(ConfigError::InvalidAddress)
        );
        assert_eq!(
            ListenConfig::from_settings(&settings("127.0.0.1", 80, false), true),
            Err(ConfigError::InvalidPort)
        );
        assert_eq!(
            ListenConfig::from_settings(&settings("127.0.0.1", 0, false), true),
            Err(ConfigError::InvalidPort)
        );
    }

    #[test]
    fn wildcard_endpoint_shows_loopback() {
        assert_eq!(
            endpoint_url("0.0.0.0".parse().unwrap(), 24639),
            "http://127.0.0.1:24639/v1/audio/transcriptions"
        );
    }

    #[test]
    fn exposure_detection_is_conservative() {
        assert!(!is_exposed("127.0.0.1"));
        assert!(!is_exposed("::1"));
        assert!(is_exposed("0.0.0.0"));
        assert!(is_exposed("10.0.0.2"));
        assert!(is_exposed("not an ip"));
    }
}
