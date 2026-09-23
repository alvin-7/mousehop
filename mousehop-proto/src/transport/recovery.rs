//! Opt-in envelope inside the existing authenticated DTLS association.
//! Never advertise this capability until the driver and backend support it.
use super::{Frame, MOTION_TAG, MTU};
use crate::{MAX_EVENT_SIZE, ProtoEvent, decode_fixed_event};
use input_event::{Event, PointerEvent};

pub const CAP_INPUT_RECOVERY_V1: u32 = 1 << 6;
pub const TAG: u8 = 242;
pub const VERSION: u8 = 1;
pub const MAX_SIZE: usize = 28 + 13 + MTU;
pub type Session = [u8; 16];
pub type Ownership = [u8; 16];

pub fn negotiated(local: u32, remote: u32, kcp_selected: bool) -> bool {
    kcp_selected && local & remote & CAP_INPUT_RECOVERY_V1 != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Dialer = 0,
    Acceptor = 1,
}
impl Role {
    pub fn peer(self) -> Self {
        match self {
            Self::Dialer => Self::Acceptor,
            Self::Acceptor => Self::Dialer,
        }
    }
    fn decode(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Dialer),
            1 => Some(Self::Acceptor),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Request = 0,
    Prepare,
    Prepared,
    Commit,
    CommitAck,
    Activate,
    ActivateAck,
}
impl Kind {
    pub fn sender(self) -> Option<Role> {
        match self {
            Self::Request => None,
            Self::Prepare | Self::Commit | Self::Activate => Some(Role::Dialer),
            Self::Prepared | Self::CommitAck | Self::ActivateAck => Some(Role::Acceptor),
        }
    }
    fn decode(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Request,
            1 => Self::Prepare,
            2 => Self::Prepared,
            3 => Self::Commit,
            4 => Self::CommitAck,
            5 => Self::Activate,
            6 => Self::ActivateAck,
            _ => return None,
        })
    }
}

/// Local actual cursor and validated layout generation after release. The opaque
/// ownership digest must identify the same confirmed transaction at both peers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Baseline {
    pub x: f64,
    pub y: f64,
    pub layout: u64,
    pub ownership: Ownership,
    pub owner: Role,
}
impl Baseline {
    pub fn valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.x.abs() <= i32::MAX as f64
            && self.y.abs() <= i32::MAX as f64
            && self.layout != 0
            && self.ownership != [0; 16]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub session: Session,
    pub epoch: u64,
    pub source: Role,
    pub kind: Kind,
    pub baseline: Option<Baseline>,
}
impl Control {
    pub fn valid(&self) -> bool {
        self.session != [0; 16]
            && self.epoch != u64::MAX
            && self.kind.sender().is_none_or(|role| role == self.source)
            && match self.kind {
                Kind::Prepared | Kind::Commit => self.baseline.is_some_and(|b| b.valid()),
                _ => self.baseline.is_none(),
            }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Envelope {
    Control(Control),
    Reliable {
        session: Session,
        epoch: u64,
        source: Role,
        frame: Frame,
    },
    /// Original Motion v1 UDP frame, strictly checked before returning it.
    Motion {
        session: Session,
        epoch: u64,
        source: Role,
        bytes: Vec<u8>,
    },
}

fn valid_motion(bytes: &[u8]) -> bool {
    if bytes.len() < 20 || bytes[..3] != [MOTION_TAG, 1, 0] {
        return false;
    }
    let len = bytes[19] as usize;
    if len > MAX_EVENT_SIZE || bytes.len() != 20 + len || bytes[11..19] == [0; 8] {
        return false;
    }
    match decode_fixed_event(&bytes[20..]) {
        Ok(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. })))
        | Ok(ProtoEvent::HandoverInput {
            event: Event::Pointer(PointerEvent::Motion { dx, dy, .. }),
            ..
        }) => dx.is_finite() && dy.is_finite(),
        _ => false,
    }
}

impl Envelope {
    pub fn identity(&self) -> (Session, u64, Role) {
        match self {
            Self::Control(c) => (c.session, c.epoch, c.source),
            Self::Reliable {
                session,
                epoch,
                source,
                ..
            }
            | Self::Motion {
                session,
                epoch,
                source,
                ..
            } => (*session, *epoch, *source),
        }
    }
    pub fn matches(&self, session: Session, epoch: u64, source: Role) -> bool {
        self.identity() == (session, epoch, source)
    }
    pub fn encode(&self) -> Option<Vec<u8>> {
        let (session, epoch, source) = self.identity();
        if session == [0; 16] {
            return None;
        }
        let kind = match self {
            Self::Control(_) => 0,
            Self::Reliable { .. } => 1,
            Self::Motion { .. } => 2,
        };
        let mut bytes = vec![TAG, VERSION, kind, source as u8];
        bytes.extend(session);
        bytes.extend(epoch.to_le_bytes());
        match self {
            Self::Control(c) => {
                if !c.valid() {
                    return None;
                }
                bytes.push(c.kind as u8);
                if let Some(b) = c.baseline {
                    bytes.extend(b.x.to_le_bytes());
                    bytes.extend(b.y.to_le_bytes());
                    bytes.extend(b.layout.to_le_bytes());
                    bytes.extend(b.ownership);
                    bytes.push(b.owner as u8);
                }
            }
            Self::Reliable { frame, .. } => {
                if let Frame::Data { payload, .. } = frame {
                    if !(24..=MTU).contains(&payload.len()) {
                        return None;
                    }
                }
                let encoded = frame.encode();
                Frame::decode(&encoded)?;
                bytes.extend(encoded);
            }
            Self::Motion { bytes: motion, .. } => {
                if !valid_motion(motion) {
                    return None;
                }
                bytes.extend(motion);
            }
        }
        Some(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !(29..=MAX_SIZE).contains(&bytes.len()) || bytes[..2] != [TAG, VERSION] {
            return None;
        }
        let source = Role::decode(bytes[3])?;
        let session: Session = bytes[4..20].try_into().ok()?;
        if session == [0; 16] {
            return None;
        }
        let epoch = u64::from_le_bytes(bytes[20..28].try_into().ok()?);
        Some(match bytes[2] {
            0 => {
                let kind = Kind::decode(bytes[28])?;
                let baseline = if matches!(kind, Kind::Prepared | Kind::Commit) {
                    if bytes.len() != 70 {
                        return None;
                    }
                    Some(Baseline {
                        x: f64::from_le_bytes(bytes[29..37].try_into().ok()?),
                        y: f64::from_le_bytes(bytes[37..45].try_into().ok()?),
                        layout: u64::from_le_bytes(bytes[45..53].try_into().ok()?),
                        ownership: bytes[53..69].try_into().ok()?,
                        owner: Role::decode(bytes[69])?,
                    })
                } else {
                    if bytes.len() != 29 {
                        return None;
                    }
                    None
                };
                let control = Control {
                    session,
                    epoch,
                    source,
                    kind,
                    baseline,
                };
                if !control.valid() {
                    return None;
                }
                Self::Control(control)
            }
            1 => Self::Reliable {
                session,
                epoch,
                source,
                frame: Frame::decode(&bytes[28..])?,
            },
            2 => {
                if !valid_motion(&bytes[28..]) {
                    return None;
                }
                Self::Motion {
                    session,
                    epoch,
                    source,
                    bytes: bytes[28..].to_vec(),
                }
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Baseline {
        Baseline {
            x: 1.0,
            y: -2.0,
            layout: 1,
            ownership: [9; 16],
            owner: Role::Dialer,
        }
    }

    #[test]
    fn r12_envelopes_roundtrip_and_reject_invalid_wire() {
        for kind in [
            Kind::Request,
            Kind::Prepare,
            Kind::Prepared,
            Kind::Commit,
            Kind::CommitAck,
            Kind::Activate,
            Kind::ActivateAck,
        ] {
            let control = Control {
                session: [1; 16],
                epoch: 4,
                source: kind.sender().unwrap_or(Role::Dialer),
                kind,
                baseline: matches!(kind, Kind::Prepared | Kind::Commit).then(baseline),
            };
            let envelope = Envelope::Control(control);
            let bytes = envelope.encode().unwrap();
            assert_eq!(Envelope::decode(&bytes), Some(envelope));
            for n in 0..bytes.len() {
                assert!(Envelope::decode(&bytes[..n]).is_none());
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(Envelope::decode(&extra).is_none());
            let mut wrong_version = bytes;
            wrong_version[1] = 2;
            assert!(Envelope::decode(&wrong_version).is_none());
        }
        let frame = super::super::Frame::Progress {
            id: 7,
            sent: 4,
            processed: 2,
        };
        let envelope = Envelope::Reliable {
            session: [2; 16],
            epoch: 8,
            source: Role::Acceptor,
            frame,
        };
        assert_eq!(
            Envelope::decode(&envelope.encode().unwrap()),
            Some(envelope)
        );
    }

    #[test]
    fn r12_rejects_invalid_baseline_identity_role_and_epoch() {
        let mut control = Control {
            session: [1; 16],
            epoch: 1,
            source: Role::Acceptor,
            kind: Kind::Prepared,
            baseline: Some(baseline()),
        };
        control.baseline.as_mut().unwrap().x = f64::NAN;
        assert!(Envelope::Control(control.clone()).encode().is_none());
        control.baseline = Some(baseline());
        control.source = Role::Dialer;
        assert!(Envelope::Control(control.clone()).encode().is_none());
        control.source = Role::Acceptor;
        control.session = [0; 16];
        assert!(Envelope::Control(control.clone()).encode().is_none());
        control.session = [1; 16];
        control.epoch = u64::MAX;
        assert!(Envelope::Control(control).encode().is_none());
    }

    #[test]
    fn r15_recovery_requires_both_capabilities_and_selected_kcp() {
        let cap = CAP_INPUT_RECOVERY_V1;
        assert!(negotiated(cap, cap, true));
        assert!(!negotiated(cap, cap, false));
        assert!(!negotiated(cap, 0, true));
        assert!(!negotiated(0, cap, true));
        let hello = ProtoEvent::Hello {
            magic: crate::PROTOCOL_MAGIC,
            commit: [0; 8],
            capabilities: crate::PROTOCOL_CAPABILITIES | cap,
        };
        let (bytes, len): ([u8; MAX_EVENT_SIZE], usize) = hello.into();
        assert!(matches!(decode_fixed_event(&bytes[..len]).unwrap(),
            ProtoEvent::Hello { capabilities, .. } if capabilities & cap != 0));
        assert_eq!(super::super::VERSION, 2);
        assert_eq!(
            crate::PROTOCOL_CAPABILITIES & cap,
            0,
            "not advertised before driver integration"
        );
    }

    #[test]
    fn r12_all_data_types_preserve_full_identity_and_reject_malformed_payloads() {
        let event = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: -2.0,
        }));
        let (raw, n): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let mut motion = vec![MOTION_TAG, 1, 0];
        motion.extend(0_u64.to_le_bytes());
        motion.extend(1_u64.to_le_bytes());
        motion.push(n as u8);
        motion.extend(&raw[..n]);
        let mut envelopes = vec![Envelope::Motion {
            session: [1; 16],
            epoch: 9,
            source: Role::Dialer,
            bytes: motion,
        }];
        for frame in [
            Frame::Offer { id: 2, conv: 7 },
            Frame::Ready { id: 2, conv: 7 },
            Frame::Data {
                id: 2,
                payload: vec![0; MTU],
            },
            Frame::Progress {
                id: 2,
                sent: 1,
                processed: 1,
            },
        ] {
            envelopes.push(Envelope::Reliable {
                session: [1; 16],
                epoch: 9,
                source: Role::Dialer,
                frame,
            });
        }
        for envelope in envelopes {
            assert!(envelope.matches([1; 16], 9, Role::Dialer));
            assert!(!envelope.matches([2; 16], 9, Role::Dialer));
            assert!(!envelope.matches([1; 16], 10, Role::Dialer));
            assert!(!envelope.matches([1; 16], 9, Role::Acceptor));
            let bytes = envelope.encode().unwrap();
            assert_eq!(Envelope::decode(&bytes), Some(envelope));
            for len in 0..bytes.len() {
                assert!(Envelope::decode(&bytes[..len]).is_none());
            }
            let mut invalid = bytes.clone();
            invalid.push(0);
            assert!(Envelope::decode(&invalid).is_none());
            invalid = bytes.clone();
            invalid[3] = 2;
            assert!(Envelope::decode(&invalid).is_none());
            invalid = bytes;
            invalid[2] = 3;
            assert!(Envelope::decode(&invalid).is_none());
        }
    }

    #[test]
    fn r12_nonfinite_motion_and_forged_control_bytes_are_rejected() {
        let c = Control {
            session: [1; 16],
            epoch: 1,
            source: Role::Acceptor,
            kind: Kind::Prepared,
            baseline: Some(baseline()),
        };
        let encoded = Envelope::Control(c).encode().unwrap();
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            i32::MAX as f64 + 1.0,
        ] {
            let mut bytes = encoded.clone();
            bytes[29..37].copy_from_slice(&value.to_le_bytes());
            assert!(Envelope::decode(&bytes).is_none());
        }
        let mut bytes = encoded.clone();
        bytes[69] = 2;
        assert!(Envelope::decode(&bytes).is_none());
        let mut bytes = encoded;
        bytes[45..53].fill(0);
        assert!(Envelope::decode(&bytes).is_none());
        let event = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 0,
            dx: f64::NAN,
            dy: 0.0,
        }));
        let (raw, n): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let mut motion = vec![MOTION_TAG, 1, 0];
        motion.extend(0_u64.to_le_bytes());
        motion.extend(1_u64.to_le_bytes());
        motion.push(n as u8);
        motion.extend(&raw[..n]);
        assert!(
            Envelope::Motion {
                session: [1; 16],
                epoch: 0,
                source: Role::Dialer,
                bytes: motion
            }
            .encode()
            .is_none()
        );
    }
}
