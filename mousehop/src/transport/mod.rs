//! Per-DTLS reliable input. No socket, identity or OS backend is replaced here.
mod core;
mod driver;
pub(crate) mod lifecycle;
mod motion;
pub(crate) mod recovery;
pub(crate) use driver::{
    Receipt, attach, attach_recoverable, lifecycle, raw, ready, receipt, status,
};
use serde::{Deserialize, Serialize};

/// Validated local policy; copied into both connection managers at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub(crate) struct StallTimeout(u64);

/// Liveness is independent of reliable input delivery deadlines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub(crate) struct PeerTimeout(u64);

impl Default for PeerTimeout {
    fn default() -> Self {
        Self(3000)
    }
}

impl TryFrom<u64> for PeerTimeout {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=60000).contains(&value) {
            Ok(Self(value))
        } else {
            Err("kcp_peer_timeout_ms must be in 1..=60000 milliseconds")
        }
    }
}

impl From<PeerTimeout> for u64 {
    fn from(value: PeerTimeout) -> Self {
        value.0
    }
}

impl PeerTimeout {
    pub(crate) fn milliseconds(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Timeouts {
    pub(crate) stall: StallTimeout,
    pub(crate) peer: PeerTimeout,
}

impl From<StallTimeout> for Timeouts {
    fn from(stall: StallTimeout) -> Self {
        Self {
            stall,
            ..Self::default()
        }
    }
}

impl Default for StallTimeout {
    fn default() -> Self {
        Self(600)
    }
}

impl TryFrom<u64> for StallTimeout {
    type Error = &'static str;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=60000).contains(&value) {
            Ok(Self(value))
        } else {
            Err("kcp_stall_timeout_ms must be in 1..=60000 milliseconds")
        }
    }
}

impl From<StallTimeout> for u64 {
    fn from(value: StallTimeout) -> Self {
        value.0
    }
}

impl StallTimeout {
    pub(crate) fn milliseconds(self) -> u64 {
        self.0
    }
    pub(crate) fn duration(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputTransport {
    #[default]
    Legacy,
    KcpRequired,
}

#[cfg(test)]
mod tests;
