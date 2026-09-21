//! Versioned frames inside an authenticated DTLS connection.
pub const TAG: u8 = 240;
pub const VERSION: u8 = 2;
pub const CAP_KCP_INPUT_V1: u32 = 1 << 2;
/// Controller selection, distinct from receiver support; fixed for this DTLS session.
pub const CAP_KCP_REQUEST: u32 = 1 << 3;
/// Cumulative UDP motion with reliable position checkpoints (envelope v1).
pub const CAP_UDP_MOTION_V1: u32 = 1 << 5;
pub const MOTION_TAG: u8 = 241;
pub const MTU: usize = 1100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Offer {
        id: u64,
        conv: u32,
    },
    Ready {
        id: u64,
        conv: u32,
    },
    Data {
        id: u64,
        payload: Vec<u8>,
    },
    /// Highest sequence sent, and highest received sequence this sender consumed.
    Progress {
        id: u64,
        sent: u64,
        processed: u64,
    },
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, id) = match self {
            Self::Offer { id, .. } => (0, id),
            Self::Ready { id, .. } => (1, id),
            Self::Data { id, .. } => (2, id),
            Self::Progress { id, .. } => (3, id),
        };
        let mut bytes = vec![TAG, VERSION, kind];
        bytes.extend(id.to_le_bytes());
        match self {
            Self::Offer { conv, .. } | Self::Ready { conv, .. } => bytes.extend(conv.to_le_bytes()),
            Self::Data { payload, .. } => {
                bytes.extend((payload.len() as u16).to_le_bytes());
                bytes.extend(payload);
            }
            Self::Progress {
                sent, processed, ..
            } => {
                bytes.extend(sent.to_le_bytes());
                bytes.extend(processed.to_le_bytes());
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 11 || bytes[0] != TAG || bytes[1] != VERSION {
            return None;
        }
        let id = u64::from_le_bytes(bytes[3..11].try_into().ok()?);
        if id == 0 {
            return None;
        }
        match bytes[2] {
            kind @ (0 | 1) if bytes.len() == 15 => {
                let conv = u32::from_le_bytes(bytes[11..15].try_into().ok()?);
                if conv == 0 {
                    return None;
                }
                Some(if kind == 0 {
                    Self::Offer { id, conv }
                } else {
                    Self::Ready { id, conv }
                })
            }
            2 if bytes.len() >= 13 => {
                let len = u16::from_le_bytes(bytes[11..13].try_into().ok()?) as usize;
                if !(24..=MTU).contains(&len) || bytes.len() != 13 + len {
                    return None;
                }
                Some(Self::Data {
                    id,
                    payload: bytes[13..].to_vec(),
                })
            }
            3 if bytes.len() == 27 => Some(Self::Progress {
                id,
                sent: u64::from_le_bytes(bytes[11..19].try_into().ok()?),
                processed: u64::from_le_bytes(bytes[19..27].try_into().ok()?),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_reject_truncation() {
        for frame in [
            Frame::Offer { id: 19, conv: 7 },
            Frame::Ready { id: 19, conv: 7 },
            Frame::Progress {
                id: 19,
                sent: 8,
                processed: 6,
            },
            Frame::Data {
                id: 19,
                payload: vec![1; 24],
            },
        ] {
            let bytes = frame.encode();
            assert_eq!(Frame::decode(&bytes), Some(frame));
            for n in 0..bytes.len() {
                assert!(Frame::decode(&bytes[..n]).is_none());
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(Frame::decode(&extra).is_none());
            let mut version = bytes;
            version[1] += 1;
            assert!(Frame::decode(&version).is_none());
        }
    }
}
