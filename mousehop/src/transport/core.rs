use kcp::Kcp;
use mousehop_proto::transport::{Frame, MTU};
use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

// Together with the fixed receive window (128), bounded output queues and
// 16-slot I/O queues this leaves headroom under a 1 MiB transport payload budget.
// DTLS's own buffers and allocator/task overhead are owned by webrtc, not KCP.
const MAX_PENDING: usize = 64;
// One flush: <=64 send-window segments, one carried ACK buffer and two probes.
// Reserve two further packets for input/flush_ack (one MTU of <=45 ACKs).
const FLUSH_RESERVE: usize = MAX_PENDING + 3;
const OUTPUT_CAPACITY: usize = FLUSH_RESERVE + 2;
const MAX_MESSAGE: usize = mousehop_proto::MAX_DISPLAY_LAYOUT_SIZE + 8 + 64;
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Default)]
struct Output {
    queue: Arc<Mutex<VecDeque<(u64, Vec<u8>)>>>,
    now: Arc<AtomicU64>,
}
impl Write for Output {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let mut q = self.queue.lock().unwrap();
        if q.len() >= OUTPUT_CAPACITY || b.len() > MTU {
            return Err(io::Error::other("KCP output budget"));
        }
        q.push_back((self.now.load(Ordering::Relaxed), b.to_vec()));
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Session {
    dialer: bool,
    hello: bool,
    ready: bool,
    closed: bool,
    id: u64,
    conv: u32,
    started: u64,
    last_offer: u64,
    last_feedback: u64,
    last_peer: u64,
    kcp: Option<Kcp<Output>>,
    output: Output,
    wire: VecDeque<(u64, Vec<u8>)>,
    incoming: VecDeque<(u64, Vec<u8>)>,
    sent: u64,
    received: u64,
    consumed: u64,
    peer_sent: u64,
    outstanding: VecDeque<(u64, u64)>,
    // Time since contiguous application consumption last advanced.
    acknowledgement_since: Option<u64>,
    consuming: VecDeque<(u64, u64)>,
    gap_since: Option<u64>,
    stall_ms: u64,
}

impl Session {
    #[cfg(test)]
    pub(super) fn new(dialer: bool, id: u64, conv: u32, now: u64) -> Self {
        Self::with_timeout(dialer, id, conv, now, super::StallTimeout::default())
    }
    pub(super) fn with_timeout(
        dialer: bool,
        id: u64,
        conv: u32,
        now: u64,
        timeout: super::StallTimeout,
    ) -> Self {
        Self {
            dialer,
            hello: false,
            ready: false,
            closed: false,
            id,
            conv,
            started: now,
            last_offer: now,
            last_feedback: now,
            last_peer: now,
            kcp: None,
            output: Output::default(),
            wire: VecDeque::new(),
            incoming: VecDeque::new(),
            sent: 0,
            received: 0,
            consumed: 0,
            peer_sent: 0,
            outstanding: VecDeque::new(),
            acknowledgement_since: None,
            consuming: VecDeque::new(),
            gap_since: None,
            stall_ms: timeout.milliseconds(),
        }
    }
    #[cfg(test)]
    pub(super) fn set_stall_ms(&mut self, milliseconds: u64) {
        self.stall_ms = milliseconds;
    }
    fn fail<T>(&mut self, reason: &'static str) -> Result<T> {
        self.closed = true;
        self.ready = false;
        self.kcp = None;
        self.wire.clear();
        self.incoming.clear();
        self.outstanding.clear();
        self.acknowledgement_since = None;
        self.consuming.clear();
        self.output.queue.lock().unwrap().clear();
        Err(reason)
    }
    pub(super) fn ready(&self) -> bool {
        self.ready && !self.closed
    }
    pub(super) fn can_send(&self) -> bool {
        self.ready()
            && self.outstanding.len() < MAX_PENDING
            && self
                .kcp
                .as_ref()
                .is_some_and(|k| k.wait_snd() < MAX_PENDING)
    }
    pub(super) fn has_received(&self) -> bool {
        !self.incoming.is_empty()
    }
    pub(super) fn can_input(&self) -> bool {
        self.wire.len() < 64 && self.output.queue.lock().unwrap().len() <= OUTPUT_CAPACITY - 2
    }
    fn can_flush(&self) -> bool {
        self.output.queue.lock().unwrap().len() <= OUTPUT_CAPACITY - FLUSH_RESERVE
    }
    pub(super) fn fresh_motion(&mut self, dependency: u64, now: u64) {
        if self.ready() {
            self.last_peer = now;
            // A future sample also proves a reliable gap even if Progress was lost.
            // Start its deadline once; further motion must never renew it.
            self.peer_sent = self.peer_sent.max(dependency);
            if self.peer_sent > self.received && self.gap_since.is_none() {
                self.gap_since = Some(now);
            }
        }
    }
    pub(super) fn has_output(&self) -> bool {
        !self.wire.is_empty() || !self.output.queue.lock().unwrap().is_empty()
    }
    pub(super) fn output_expired(&self, now: u64) -> bool {
        self.wire
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > self.stall_ms)
            || self
                .output
                .queue
                .lock()
                .unwrap()
                .front()
                .is_some_and(|(t, _)| now.saturating_sub(*t) > self.stall_ms)
    }
    // Called only after the writer has reserved capacity. Never discard a batch.
    pub(super) fn pop_output(&mut self) -> Option<(u64, Vec<u8>)> {
        let mut output = self.output.queue.lock().unwrap();
        if self
            .wire
            .front()
            .is_some_and(|(t, _)| output.front().is_none_or(|(u, _)| t <= u))
        {
            self.wire.pop_front()
        } else {
            output.pop_front().map(|(t, payload)| {
                (
                    t,
                    Frame::Data {
                        id: self.id,
                        payload,
                    }
                    .encode(),
                )
            })
        }
    }
    pub(super) fn hello(&mut self, supported: bool, now: u64) -> Result<()> {
        self.output.now.store(now, Ordering::Relaxed);
        if self.closed {
            return Err("closed");
        }
        if !supported {
            return self.fail("peer does not support required KCP");
        }
        if !self.hello {
            self.hello = true;
            if self.dialer {
                self.init(now)?;
                self.offer()?;
            }
        }
        Ok(())
    }
    fn init(&mut self, now: u64) -> Result<()> {
        let mut k = Kcp::new(self.conv, self.output.clone());
        k.set_mtu(MTU).map_err(|_| "MTU")?;
        k.set_wndsize(64, 128);
        // Interactive LAN input must not wait for a congestion window to
        // recover after Wi-Fi loss. Application windows/queues remain bounded.
        k.set_nodelay(true, 10, 2, true);
        k.update(now as u32).map_err(|_| "KCP update")?;
        self.kcp = Some(k);
        Ok(())
    }
    fn emit(&mut self, f: Frame) -> Result<()> {
        if self.wire.len() >= 64 {
            return self.fail("wire budget");
        }
        self.wire
            .push_back((self.output.now.load(Ordering::Relaxed), f.encode()));
        Ok(())
    }
    fn offer(&mut self) -> Result<()> {
        self.emit(Frame::Offer {
            id: self.id,
            conv: self.conv,
        })
    }
    fn feedback(&mut self) -> Result<()> {
        self.emit(Frame::Progress {
            id: self.id,
            sent: self.sent,
            processed: self.consumed,
        })
    }
    #[cfg(test)]
    pub(super) fn drain(&mut self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.pop_output().map(|(_, bytes)| bytes)).collect()
    }
    pub(super) fn send(&mut self, bytes: &[u8], now: u64) -> Result<()> {
        self.output.now.store(now, Ordering::Relaxed);
        if !self.ready() {
            return Err("KCP not ready");
        }
        if bytes.len() + 8 > MAX_MESSAGE
            || self.outstanding.len() >= MAX_PENDING
            || self.kcp.as_ref().unwrap().wait_snd() >= MAX_PENDING
        {
            return self.fail("reliable input budget");
        }
        self.sent = self.sent.checked_add(1).ok_or("sequence exhausted")?;
        let mut message = self.sent.to_le_bytes().to_vec();
        message.extend(bytes);
        let flush = self.can_flush();
        let k = self.kcp.as_mut().unwrap();
        if k.send(&message).is_err() || (flush && k.flush().is_err()) {
            return self.fail("KCP send");
        }
        if self.outstanding.is_empty() {
            self.acknowledgement_since = Some(now);
        }
        self.outstanding.push_back((self.sent, now));
        Ok(())
    }
    pub(super) fn input(&mut self, bytes: &[u8], now: u64) -> Result<()> {
        self.output.now.store(now, Ordering::Relaxed);
        if self.closed {
            return Err("closed");
        }
        if !self.hello {
            return self.fail("transport before Hello");
        }
        let Some(frame) = Frame::decode(bytes) else {
            return self.fail("invalid transport frame");
        };
        match frame {
            Frame::Offer { id, conv } if !self.dialer => {
                if self.kcp.is_none() {
                    self.id = id;
                    self.conv = conv;
                    self.init(now)?;
                }
                if id != self.id || conv != self.conv {
                    return self.fail("renegotiation forbidden");
                }
                self.emit(Frame::Ready { id, conv })?;
            }
            Frame::Ready { id, conv } if self.dialer && id == self.id && conv == self.conv => {
                if !self.ready {
                    self.ready = true;
                    self.last_peer = now;
                    // Bootstrap the acceptor's reverse gate without an application event.
                    let k = self.kcp.as_mut().unwrap();
                    k.send(&[0; 8]).map_err(|_| "bootstrap")?;
                    k.flush().map_err(|_| "bootstrap flush")?;
                }
            }
            Frame::Data { id, payload } if id == self.id && self.kcp.is_some() => {
                if self.dialer && !self.ready {
                    return self.fail("data before Ready");
                }
                if !valid_segments(&payload, self.conv) {
                    return self.fail("invalid KCP segment");
                }
                let k = self.kcp.as_mut().unwrap();
                if k.input(&payload).is_err() || k.flush_ack().is_err() {
                    return self.fail("KCP input");
                }
                self.ready = true;
                self.last_peer = now;
                let previous_received = self.received;
                loop {
                    let k = self.kcp.as_mut().unwrap();
                    let Ok(size) = k.peeksize() else {
                        break;
                    };
                    if !(8..=MAX_MESSAGE).contains(&size) || self.consuming.len() >= MAX_PENDING {
                        return self.fail("receive budget");
                    }
                    let mut message = vec![0; size];
                    k.recv(&mut message).map_err(|_| "KCP receive")?;
                    let seq = u64::from_le_bytes(message[..8].try_into().unwrap());
                    if seq == 0 && size == 8 && !self.dialer && self.received == 0 {
                        continue;
                    }
                    if seq != self.received + 1 {
                        return self.fail("invalid input sequence");
                    }
                    self.received = seq;
                    self.peer_sent = self.peer_sent.max(seq);
                    self.consuming.push_back((seq, now));
                    self.incoming.push_back((seq, message[8..].to_vec()));
                }
                if self.peer_sent <= self.received {
                    self.gap_since = None;
                } else if self.received > previous_received {
                    // Only contiguous application progress renews the remaining gap.
                    // Bootstrap, ACK-only and buffered out-of-order segments do not.
                    self.gap_since = Some(now);
                }
            }
            Frame::Progress {
                id,
                sent,
                processed,
            } if id == self.id && self.kcp.is_some() => {
                if processed > self.sent {
                    return self.fail("future processing acknowledgement");
                }
                // Reordered feedback cannot roll back either cumulative watermark.
                self.peer_sent = self.peer_sent.max(sent);
                let previous_pending = self.outstanding.len();
                while self
                    .outstanding
                    .front()
                    .is_some_and(|(s, _)| *s <= processed)
                {
                    self.outstanding.pop_front();
                }
                if self.outstanding.is_empty() {
                    self.acknowledgement_since = None;
                } else if self.outstanding.len() < previous_pending {
                    // A heartbeat, duplicate feedback or transport ACK cannot
                    // renew this deadline. Only newly consumed input can.
                    self.acknowledgement_since = Some(now);
                }
                if self.peer_sent > self.received && self.gap_since.is_none() {
                    self.gap_since = Some(now);
                }
                self.last_peer = now;
            }
            Frame::Data { id, .. } | Frame::Progress { id, .. } if id != self.id => {}
            _ => return self.fail("unexpected session/negotiation frame"),
        }
        Ok(())
    }
    pub(super) fn receive(&mut self) -> Option<(u64, Vec<u8>)> {
        self.incoming.pop_front()
    }
    pub(super) fn processed(&mut self, sequence: u64, _now: u64) -> Result<()> {
        if self.closed {
            return Err("closed");
        }
        if sequence != self.consumed + 1 || sequence > self.received {
            return self.fail("out of order consumption");
        }
        self.consumed = sequence;
        self.consuming.pop_front();
        Ok(())
    }
    pub(super) fn tick(&mut self, now: u64) -> Result<()> {
        self.output.now.store(now, Ordering::Relaxed);
        if self.closed {
            return Err("closed");
        }
        if !self.ready && now.saturating_sub(self.started) >= 6000 {
            return self.fail("KCP negotiation timeout");
        }
        if self.ready
            && (now.saturating_sub(self.last_peer) > self.stall_ms
                || self
                    .acknowledgement_since
                    .is_some_and(|t| now.saturating_sub(t) > self.stall_ms)
                || self
                    .consuming
                    .front()
                    .is_some_and(|(_, t)| now.saturating_sub(*t) > self.stall_ms)
                || self
                    .gap_since
                    .is_some_and(|t| now.saturating_sub(t) > self.stall_ms))
        {
            log::warn!(
                "KCP stall: limit={}ms peer_age={}ms ack_progress_age={:?}ms oldest_sent={:?}ms oldest_consuming={:?}ms gap_age={:?}ms pending={} consuming={} sent={} received={} consumed={}",
                self.stall_ms,
                now.saturating_sub(self.last_peer),
                self.acknowledgement_since.map(|t| now.saturating_sub(t)),
                self.outstanding
                    .front()
                    .map(|(_, t)| now.saturating_sub(*t)),
                self.consuming.front().map(|(_, t)| now.saturating_sub(*t)),
                self.gap_since.map(|t| now.saturating_sub(t)),
                self.outstanding.len(),
                self.consuming.len(),
                self.sent,
                self.received,
                self.consumed,
            );
            return self.fail("reliable input stalled");
        }
        if self.output_expired(now) {
            return self.fail("DTLS output stalled");
        }
        if self.wire.len() < 64
            && self.hello
            && self.dialer
            && !self.ready
            && now.saturating_sub(self.last_offer) >= 100
        {
            self.offer()?;
            self.last_offer = now;
        }
        if self.wire.len() < 64 && self.ready && now.saturating_sub(self.last_feedback) >= 50 {
            self.feedback()?;
            self.last_feedback = now;
        }
        if self.can_flush() {
            if let Some(k) = self.kcp.as_mut() {
                if k.update(now as u32).is_err() {
                    return self.fail("KCP output budget");
                }
            }
        }
        Ok(())
    }
}

// kcp accepts trailing short data and large fragments; validate the complete datagram first.
fn valid_segments(mut bytes: &[u8], conv: u32) -> bool {
    while !bytes.is_empty() {
        if bytes.len() < 24 || u32::from_le_bytes(bytes[..4].try_into().unwrap()) != conv {
            return false;
        }
        let len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        if len > MTU - 24 || bytes.len() < 24 + len || bytes[5] > 1 {
            return false;
        }
        if !matches!(bytes[4], 81..=84) {
            return false;
        }
        bytes = &bytes[24 + len..];
    }
    true
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn disconnect_r7_d1_full_callback_and_control_queues_drain_without_loss() {
        let mut session = Session::new(true, 31, 7, 0);
        for n in 0..OUTPUT_CAPACITY {
            session.output.write_all(&vec![n as u8; MTU]).unwrap();
        }
        assert!(session.output.write_all(&[0]).is_err());
        for _ in 0..64 {
            session.offer().unwrap();
        }
        let packets = session.drain();
        assert_eq!(packets.len(), OUTPUT_CAPACITY + 64);
        let data: Vec<_> = packets
            .iter()
            .filter_map(|bytes| match Frame::decode(bytes) {
                Some(Frame::Data { payload, .. }) => Some(payload),
                _ => None,
            })
            .collect();
        assert_eq!(data.len(), OUTPUT_CAPACITY);
        for (n, payload) in data.iter().enumerate() {
            assert_eq!(payload, &vec![n as u8; MTU]);
        }
        assert!(!session.has_output());
    }
}
