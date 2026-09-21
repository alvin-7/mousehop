//! Motion never enters KCP. Reliable events checkpoint preceding displacement.
//! A motion packet depends only on preceding reliable events, not other motion.
use input_event::{Event, PointerEvent};
use mousehop_proto::{MAX_EVENT_SIZE, ProtoEvent, decode_fixed_event, transport::MOTION_TAG};

type Result<T> = std::result::Result<T, &'static str>;

fn coordinates(event: &mut ProtoEvent) -> Option<(&mut f64, &mut f64)> {
    match event {
        ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. }))
        | ProtoEvent::HandoverInput {
            event: Event::Pointer(PointerEvent::Motion { dx, dy, .. }),
            ..
        } => Some((dx, dy)),
        _ => None,
    }
}

fn encode(event: ProtoEvent) -> Vec<u8> {
    let (b, n): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    b[..n].to_vec()
}

#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub dependency: u64,
    sequence: u64,
    motion: Vec<u8>,
    pub critical: Vec<u8>,
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![MOTION_TAG, 1, u8::from(!self.critical.is_empty())];
        bytes.extend(self.dependency.to_le_bytes());
        bytes.extend(self.sequence.to_le_bytes());
        bytes.push(self.motion.len() as u8);
        bytes.extend(&self.motion);
        bytes.extend(&self.critical);
        bytes
    }
    pub fn decode(bytes: &[u8], critical: bool) -> Result<Self> {
        if bytes.len() < 20 || bytes[..3] != [MOTION_TAG, 1, u8::from(critical)] {
            return Err("invalid motion envelope");
        }
        let n = bytes[19] as usize;
        if n > MAX_EVENT_SIZE || bytes.len() < 20 + n || (critical == (bytes.len() == 20 + n)) {
            return Err("invalid motion envelope length");
        }
        let frame = Self {
            dependency: u64::from_le_bytes(bytes[3..11].try_into().unwrap()),
            sequence: u64::from_le_bytes(bytes[11..19].try_into().unwrap()),
            motion: bytes[20..20 + n].to_vec(),
            critical: bytes[20 + n..].to_vec(),
        };
        if frame.motion.is_empty() {
            if frame.sequence != 0 || !critical {
                return Err("missing motion checkpoint");
            }
        } else {
            let mut event = decode_fixed_event(&frame.motion).map_err(|_| "invalid motion")?;
            let (x, y) = coordinates(&mut event).ok_or("non-motion in UDP channel")?;
            if frame.sequence == 0 || !x.is_finite() || !y.is_finite() {
                return Err("invalid cumulative motion");
            }
        }
        Ok(frame)
    }
}

#[derive(Default)]
pub(super) struct Sender {
    reliable: u64,
    sequence: u64,
    x: f64,
    y: f64,
    last: Vec<u8>,
}
impl Sender {
    /// Returns true for an unreliable motion datagram, false for a checkpoint.
    pub fn pack(&mut self, mut event: ProtoEvent, original: Vec<u8>) -> Result<(bool, Vec<u8>)> {
        let is_motion = if let Some((x, y)) = coordinates(&mut event) {
            self.x += *x;
            self.y += *y;
            if !self.x.is_finite() || !self.y.is_finite() {
                return Err("invalid motion total");
            }
            *x = self.x;
            *y = self.y;
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("motion sequence exhausted")?;
            self.last = encode(event);
            true
        } else {
            false
        };
        let frame = Frame {
            dependency: self.reliable,
            sequence: self.sequence,
            motion: self.last.clone(),
            critical: if is_motion { vec![] } else { original },
        };
        if !is_motion {
            self.reliable = self.reliable.checked_add(1).ok_or("sequence exhausted")?;
        }
        Ok((is_motion, frame.encode()))
    }
}

#[derive(Default)]
pub(super) struct Receiver {
    reliable: u64,
    sequence: u64,
    freshest: u64,
    freshest_dependency: u64,
    x: f64,
    y: f64,
    pending: Option<Frame>,
}
impl Receiver {
    fn apply(&mut self, frame: &Frame) -> Result<Option<Vec<u8>>> {
        if frame.sequence <= self.sequence {
            return Ok(None);
        }
        let mut event = decode_fixed_event(&frame.motion).map_err(|_| "invalid checkpoint")?;
        let (x, y) = coordinates(&mut event).ok_or("invalid checkpoint motion")?;
        let total = (*x, *y);
        *x -= self.x;
        *y -= self.y;
        if !x.is_finite() || !y.is_finite() {
            return Err("invalid motion delta");
        }
        self.x = total.0;
        self.y = total.1;
        self.sequence = frame.sequence;
        Ok(Some(encode(event)))
    }
    pub fn datagram(&mut self, frame: Frame) -> bool {
        // Only one future sample is retained. Checkpoints recover skipped epochs.
        if frame.sequence > self.freshest.max(self.sequence)
            && frame.dependency >= self.reliable
            && frame.dependency >= self.freshest_dependency
        {
            self.freshest = frame.sequence;
            self.freshest_dependency = frame.dependency;
            self.pending = Some(frame);
            return true;
        }
        false
    }
    pub fn ready(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.dependency <= self.reliable)
    }
    pub fn take(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.ready() {
            return Ok(None);
        }
        let Some(frame) = self.pending.take() else {
            return Ok(None);
        };
        if frame.dependency < self.reliable {
            return Ok(None);
        }
        self.apply(&frame)
    }
    pub fn checkpoint(&mut self, frame: &Frame, sequence: u64) -> Result<Option<Vec<u8>>> {
        if frame.dependency != self.reliable || sequence != self.reliable + 1 {
            return Err("invalid checkpoint dependency");
        }
        let motion = self.apply(frame)?;
        self.freshest = self.freshest.max(frame.sequence);
        self.freshest_dependency = self.freshest_dependency.max(frame.dependency);
        self.reliable = sequence;
        Ok(motion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn moving(time: u32, dx: f64) -> ProtoEvent {
        ProtoEvent::HandoverInput {
            serial: 7,
            event: Event::Pointer(PointerEvent::Motion { time, dx, dy: -dx }),
        }
    }
    fn packet(tx: &mut Sender, event: ProtoEvent) -> Frame {
        let bytes = encode(event.clone());
        let (udp, wire) = tx.pack(event, bytes).unwrap();
        Frame::decode(&wire, !udp).unwrap()
    }
    fn delta(bytes: Vec<u8>) -> (f64, f64) {
        let mut event = decode_fixed_event(&bytes).unwrap();
        assert!(matches!(event, ProtoEvent::HandoverInput { serial: 7, .. }));
        let (x, y) = coordinates(&mut event).unwrap();
        (*x, *y)
    }
    #[test]
    fn disconnect_r7_d2_freshness_tracks_pending_and_checkpoint_without_delivery() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let enter = packet(&mut tx, ProtoEvent::Ack(1));
        let first = packet(&mut tx, moving(1, 2.0));
        let second = packet(&mut tx, moving(2, 3.0));
        assert!(rx.datagram(second.clone()));
        assert!(!rx.datagram(second.clone()));
        assert!(!rx.datagram(first));
        assert!(!rx.ready());
        assert!(rx.take().unwrap().is_none()); // future dependency stays buffered
        assert!(rx.checkpoint(&enter, 1).unwrap().is_none());
        assert_eq!(delta(rx.take().unwrap().unwrap()), (5.0, -5.0));
        assert!(!rx.datagram(second));
        let third = packet(&mut tx, moving(3, 1.0));
        let checkpoint = packet(&mut tx, ProtoEvent::Ack(2));
        rx.checkpoint(&checkpoint, 2).unwrap();
        assert!(!rx.datagram(third));
        let mut invalid_dependency = packet(&mut tx, moving(4, 1.0));
        invalid_dependency.dependency = 0;
        assert!(!rx.datagram(invalid_dependency));
    }

    #[test]
    fn lost_reordered_duplicate_motion_recovers_distance_without_retransmission() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let first = packet(&mut tx, moving(1, 2.0));
        let second = packet(&mut tx, moving(2, 3.0));
        let third = packet(&mut tx, moving(3, 4.0));
        rx.datagram(third.clone()); // first two lost
        assert_eq!(delta(rx.take().unwrap().unwrap()), (9.0, -9.0));
        for old in [first, second, third] {
            rx.datagram(old);
        }
        assert!(!rx.ready());
        assert!(rx.take().unwrap().is_none());
        assert_eq!(tx.reliable, 0);
    }
    #[test]
    fn click_checkpoint_repairs_last_loss_and_gates_following_drag() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        packet(&mut tx, moving(1, 5.0)); // final pre-button motion lost
        let button = packet(&mut tx, ProtoEvent::Ack(12));
        let drag = packet(&mut tx, moving(2, 8.0));
        rx.datagram(drag); // UDP overtakes button
        assert!(!rx.ready());
        assert_eq!(
            delta(rx.checkpoint(&button, 1).unwrap().unwrap()),
            (5.0, -5.0)
        );
        assert_eq!(button.critical, encode(ProtoEvent::Ack(12)));
        assert!(rx.ready());
        assert_eq!(delta(rx.take().unwrap().unwrap()), (8.0, -8.0));
        assert!(rx.checkpoint(&button, 1).is_err());
    }
    #[test]
    fn later_checkpoint_recovers_overwritten_future_epochs() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let enter = packet(&mut tx, ProtoEvent::Ack(1));
        let earlier = packet(&mut tx, moving(1, 4.0));
        let release = packet(&mut tx, ProtoEvent::Ack(2));
        let later = packet(&mut tx, moving(2, 3.0));
        rx.datagram(earlier);
        rx.datagram(later);
        assert!(rx.checkpoint(&enter, 1).unwrap().is_none());
        assert!(!rx.ready());
        assert_eq!(
            delta(rx.checkpoint(&release, 2).unwrap().unwrap()),
            (4.0, -4.0)
        );
        assert_eq!(delta(rx.take().unwrap().unwrap()), (3.0, -3.0));
    }
    #[test]
    fn leave_checkpoint_prevents_old_motion_leaking_into_next_handover() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let old = packet(&mut tx, moving(1, 6.0));
        let leave = packet(&mut tx, ProtoEvent::HandoverLeave { serial: 7, mode: 0 });
        assert_eq!(
            delta(rx.checkpoint(&leave, 1).unwrap().unwrap()),
            (6.0, -6.0)
        );
        let enter = packet(&mut tx, ProtoEvent::Ack(8));
        assert!(rx.checkpoint(&enter, 2).unwrap().is_none());
        rx.datagram(old);
        assert!(!rx.ready());
        let event = ProtoEvent::HandoverInput {
            serial: 8,
            event: Event::Pointer(PointerEvent::Motion {
                time: 2,
                dx: 2.0,
                dy: 0.0,
            }),
        };
        rx.datagram(packet(&mut tx, event.clone()));
        assert_eq!(rx.take().unwrap().unwrap(), encode(event));
        // A new DTLS session starts with fresh cumulative totals and dependencies.
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        rx.datagram(packet(&mut tx, moving(1, 1.0)));
        assert_eq!(delta(rx.take().unwrap().unwrap()), (1.0, -1.0));
    }

    #[test]
    fn envelope_rejects_wrong_channel_version_truncation_and_nonfinite_motion() {
        let mut tx = Sender::default();
        let frame = packet(&mut tx, moving(1, 1.0));
        let bytes = frame.encode();
        for n in 0..bytes.len() {
            assert!(Frame::decode(&bytes[..n], false).is_err());
        }
        assert!(Frame::decode(&bytes, true).is_err());
        let mut wrong = bytes.clone();
        wrong[1] = 2;
        assert!(Frame::decode(&wrong, false).is_err());
        let mut wrong = frame;
        wrong.motion = encode(moving(2, f64::NAN));
        assert!(Frame::decode(&wrong.encode(), false).is_err());
        assert!(tx.pack(moving(2, f64::INFINITY), vec![]).is_err());
    }
}
