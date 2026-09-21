#[cfg(test)]
use super::StallTimeout;
use super::{InputTransport, Timeouts, core::Session, motion};
use super::{
    lifecycle::{Generation, Link, Runtime},
    recovery::{Input as RecoveryInput, Outcome},
};
use async_trait::async_trait;
use mousehop_proto::transport::recovery::{self as protocol, Envelope, Role};
use mousehop_proto::{
    MAX_CLIPBOARD_SIZE, MAX_EVENT_SIZE, PROTOCOL_MAGIC, ProtoEvent, decode_clipboard_event,
    decode_display_layout_event, decode_fixed_event,
    transport::{CAP_KCP_INPUT_V1, CAP_KCP_REQUEST, CAP_UDP_MOTION_V1, MOTION_TAG, TAG},
};
use std::{
    collections::VecDeque,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, mpsc},
    task::spawn_local,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use webrtc_util::{Conn, Result};

type ArcConn = Arc<dyn Conn + Send + Sync>;
const QUEUE: usize = 16;
// Resource cleanup has its own bound, independent of input tolerance.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(300);

struct WirePacket {
    bytes: Vec<u8>,
    deadline: Instant,
    generation: Option<Generation>,
}

struct Command {
    bytes: Vec<u8>,
    generation: Option<Generation>,
}

#[derive(Default)]
struct MotionSlot {
    packet: Mutex<Option<WirePacket>>,
    changed: Notify,
}
impl MotionSlot {
    fn replace(&self, mut packet: WirePacket) {
        let mut slot = self.packet.lock().unwrap();
        if let Some(old) = slot.as_ref() {
            packet.deadline = old.deadline;
        }
        *slot = Some(packet);
        self.changed.notify_one();
    }
    fn take(&self) -> Option<WirePacket> {
        self.packet.lock().unwrap().take()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Receipt {
    sequence: u64,
    completed: mpsc::Sender<(u64, Option<Generation>)>,
    cancel: CancellationToken,
    deadline: Instant,
    generation: Option<Generation>,
    recovery: Arc<Link>,
}
impl Receipt {
    pub(crate) fn abort(&self) {
        if self.generation.as_ref().is_some_and(|g| !g.valid()) {
            return;
        }
        self.cancel.cancel();
    }
    pub(crate) fn generation(&self) -> Option<&Generation> {
        self.generation.as_ref()
    }
    pub(crate) fn expire(&self) {
        if self.generation.as_ref().is_some_and(|g| !g.valid()) {
            return;
        }
        if self.recovery.enabled() {
            self.recovery.stall();
        } else {
            self.cancel.cancel();
        }
    }
    pub(crate) fn valid(&self) -> bool {
        !self.cancel.is_cancelled()
            && Instant::now() < self.deadline
            && self.generation.as_ref().is_none_or(Generation::valid)
    }
    pub(crate) fn complete(self) {
        if self.generation.as_ref().is_some_and(|g| !g.valid()) {
            return;
        }
        if self.sequence == 0 {
            return;
        }
        if !self.valid()
            || self
                .completed
                .try_send((self.sequence, self.generation.clone()))
                .is_err()
        {
            log::warn!(
                "KCP receipt {} expired or completion queue is full",
                self.sequence
            );
            self.expire();
        }
    }
    pub(crate) async fn after(self, done: tokio::sync::oneshot::Receiver<()>) {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {},
            _ = tokio::time::sleep_until(self.deadline) => self.expire(),
            result = done => {
                if result.is_ok() { self.complete(); } else { self.abort(); }
            }
        }
    }
}

struct Delivery {
    bytes: Vec<u8>,
    receipt: Option<Receipt>,
}
struct Peer {
    raw: ArcConn,
    send: mpsc::Sender<Command>,
    receive: AsyncMutex<mpsc::Receiver<Delivery>>,
    receipt: Mutex<Option<Receipt>>,
    ready: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    cancel: CancellationToken,
    timeout: Duration,
    recovery: Arc<Link>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn closed() -> webrtc_util::Error {
    io::Error::other("KCP session closed/not ready").into()
}

#[cfg(test)]
async fn send_before_deadline(conn: &ArcConn, bytes: &[u8], timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, conn.send(bytes)).await, Ok(Ok(n)) if n == bytes.len())
}

pub(crate) fn raw(conn: &ArcConn) -> &ArcConn {
    conn.as_any()
        .downcast_ref::<Peer>()
        .map_or(conn, |p| &p.raw)
}
pub(crate) fn ready(conn: &ArcConn) -> bool {
    conn.as_any()
        .downcast_ref::<Peer>()
        .is_none_or(|p| p.ready.load(Ordering::Acquire) && !p.cancel.is_cancelled())
}
pub(crate) fn receipt(conn: &ArcConn) -> Option<Receipt> {
    conn.as_any()
        .downcast_ref::<Peer>()
        .and_then(|p| p.receipt.lock().unwrap().take())
}

pub(crate) fn status(conn: &ArcConn) -> String {
    conn.as_any()
        .downcast_ref::<Peer>()
        .map_or_else(|| "Legacy".to_owned(), |p| p.status.lock().unwrap().clone())
}

pub(crate) fn lifecycle(conn: &ArcConn) -> Option<Arc<Link>> {
    conn.as_any()
        .downcast_ref::<Peer>()
        .map(|p| p.recovery.clone())
}

pub(crate) fn attach_recoverable(
    conn: ArcConn,
    mode: Option<InputTransport>,
    timeout: impl Into<Timeouts>,
) -> ArcConn {
    if mode == Some(InputTransport::Legacy) {
        return conn;
    }
    attach_policy(conn, mode.is_some(), timeout.into(), true)
}

/// None means incoming: the authenticated controller's Hello selects the mode.
pub(crate) fn attach(
    conn: ArcConn,
    mode: Option<InputTransport>,
    timeout: impl Into<Timeouts>,
) -> ArcConn {
    if mode == Some(InputTransport::Legacy) {
        return conn;
    }
    attach_with_timeout(conn, mode.is_some(), timeout)
}

#[cfg(test)]
pub(super) fn attach_required(conn: ArcConn, dialer: bool) -> ArcConn {
    attach_with_timeout(
        conn,
        dialer,
        Timeouts {
            stall: StallTimeout::try_from(300).unwrap(),
            peer: super::PeerTimeout::try_from(1500).unwrap(),
        },
    )
}

fn attach_with_timeout(conn: ArcConn, dialer: bool, timeout: impl Into<Timeouts>) -> ArcConn {
    attach_policy(conn, dialer, timeout.into(), false)
}
fn attach_policy(conn: ArcConn, dialer: bool, timeouts: Timeouts, recoverable: bool) -> ArcConn {
    let timeout = timeouts.stall;
    let (send, commands) = mpsc::channel(QUEUE);
    let (deliver, receive) = mpsc::channel(QUEUE);
    let cancel = CancellationToken::new();
    let ready = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new("Negotiating".to_owned()));
    let recovery = Arc::new(Link::default());
    let peer = Arc::new(Peer {
        raw: conn.clone(),
        send,
        receive: AsyncMutex::new(receive),
        receipt: Mutex::new(None),
        ready: ready.clone(),
        status: status.clone(),
        cancel: cancel.clone(),
        timeout: timeout.duration(),
        recovery: recovery.clone(),
    });
    spawn_local(run(
        conn,
        dialer,
        commands,
        deliver,
        ready,
        status,
        cancel,
        timeouts,
        recoverable,
        recovery,
    ));
    peer
}

fn decode(b: &[u8]) -> Option<ProtoEvent> {
    decode_fixed_event(b)
        .ok()
        .or_else(|| decode_clipboard_event(b).ok())
        .or_else(|| decode_display_layout_event(b).ok())
}
fn direct(event: &ProtoEvent) -> bool {
    matches!(
        event,
        ProtoEvent::Hello { .. }
            | ProtoEvent::Ping
            | ProtoEvent::Pong(_)
            | ProtoEvent::Clipboard { .. }
    )
}

fn control_output(outcome: &Outcome, tx: &mpsc::Sender<WirePacket>) {
    if let Some(control) = outcome.control() {
        // The state machine retries every 50ms; a full control lane never
        // allocates more memory or blocks the deadline/watchdog task.
        if let Some(bytes) = Envelope::Control(control.clone()).encode() {
            let _ = tx.try_send(WirePacket {
                bytes,
                deadline: Instant::now() + Duration::from_millis(2000),
                generation: None,
            });
        }
    }
}

fn envelope(
    bytes: Vec<u8>,
    runtime: &Option<Runtime>,
    motion: bool,
    now: u64,
    probe_epoch: Option<u64>,
) -> std::result::Result<Vec<u8>, &'static str> {
    let Some(runtime) = runtime else {
        return Ok(bytes);
    };
    let epoch = runtime
        .machine
        .receive_epoch(now)
        .or(probe_epoch)
        .ok_or("recovery receive gate closed")?;
    let session = runtime.machine.session();
    let source = runtime.machine.role();
    let frame = if motion {
        Envelope::Motion {
            session,
            epoch,
            source,
            bytes,
        }
    } else {
        Envelope::Reliable {
            session,
            epoch,
            source,
            frame: mousehop_proto::transport::Frame::decode(&bytes)
                .ok_or("invalid reliable output")?,
        }
    };
    frame.encode().ok_or("invalid recovery output")
}

#[allow(clippy::too_many_arguments)]
async fn run(
    conn: ArcConn,
    dialer: bool,
    mut commands: mpsc::Receiver<Command>,
    deliver: mpsc::Sender<Delivery>,
    ready: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    cancel: CancellationToken,
    timeouts: Timeouts,
    recoverable: bool,
    recovery: Arc<Link>,
) {
    let timeout = timeouts.stall;
    let (wire_tx, mut wire_rx) = mpsc::channel::<WirePacket>(QUEUE);
    let (control_tx, mut control_rx) = mpsc::channel::<WirePacket>(4);
    let motion_slot = Arc::new(MotionSlot::default());
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let (completed, mut completions) = mpsc::channel::<(u64, Option<Generation>)>(QUEUE);
    let kcp_selected = Arc::new(AtomicBool::new(dialer));
    // Only this reader calls DTLS recv. It is never cancelled by an update tick.
    let reader = spawn_local({
        let conn = conn.clone();
        let cancel = cancel.clone();
        async move {
            let mut buf = [0; MAX_CLIPBOARD_SIZE + 32];
            loop {
                let n = tokio::select! { _ = cancel.cancelled() => break, r = conn.recv(&mut buf) => match r {
                    Ok(n) if n > 0 => n,
                    result => { log::warn!("KCP DTLS receive ended: {result:?}"); break; }
                } };
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = input_tx.send(buf[..n].to_vec()) => {
                        if result.is_err() { break; }
                    }
                }
            }
            cancel.cancel();
        }
    });
    let writer = spawn_local({
        let conn = conn.clone();
        let cancel = cancel.clone();
        let kcp_selected = kcp_selected.clone();
        let motion_slot = motion_slot.clone();
        let recovery = recovery.clone();
        async move {
            let mut prefer_motion = false;
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                // Alternate when both lanes are busy: neither can starve the other.
                let packet = if let Ok(packet) = control_rx.try_recv() {
                    packet
                } else if let Some(packet) = if prefer_motion {
                    motion_slot.take()
                } else {
                    None
                } {
                    prefer_motion = false;
                    packet
                } else if let Ok(packet) = wire_rx.try_recv() {
                    prefer_motion = true;
                    packet
                } else if let Some(packet) = motion_slot.take() {
                    prefer_motion = false;
                    packet
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        p = control_rx.recv() => match p { Some(p) => p, None => break },
                        _ = motion_slot.changed.notified() => continue,
                        b = wire_rx.recv() => match b {
                            Some(b) => { prefer_motion = true; b },
                            None => break,
                        }
                    }
                };
                if packet.generation.as_ref().is_some_and(|g| !g.valid()) {
                    continue;
                }
                if Instant::now() >= packet.deadline && recovery.enabled() {
                    recovery.stall();
                    continue;
                }
                // Automatic incoming Legacy retains its original fixed I/O bound.
                let send_timeout = if recovery.enabled() {
                    Duration::from_millis(super::recovery::RECOVERY_MS)
                } else if kcp_selected.load(Ordering::Acquire) {
                    timeout.duration()
                } else {
                    CLOSE_TIMEOUT
                };
                let deadline = if recovery.enabled() {
                    Instant::now() + send_timeout
                } else {
                    packet.deadline.min(Instant::now() + send_timeout)
                };
                if Instant::now() >= deadline
                    || !matches!(
                        tokio::time::timeout_at(deadline, conn.send(&packet.bytes)).await,
                        Ok(Ok(n)) if n == packet.bytes.len()
                    )
                {
                    log::warn!("KCP DTLS send failed or exceeded {send_timeout:?}");
                    break;
                }
            }
            cancel.cancel();
        }
    });
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ NEXT.fetch_add(1, Ordering::Relaxed))
    .max(1);
    let conv = (id as u32).max(1);
    let mut session = Session::with_timeout(dialer, id, conv, 0, timeouts);
    let mut reliable = if dialer { Some(true) } else { None };
    let mut hybrid = None;
    let mut motion_sender = motion::Sender::default();
    let mut motion_receiver = motion::Receiver::default();
    let mut pending = VecDeque::<Delivery>::new();
    let mut direct_output: Option<WirePacket> = None;
    let start = Instant::now();
    let mut runtime: Option<Runtime> = None;
    let mut recovery_selected = None;
    let mut installed_epoch = 0;
    let mut pending_probe: Option<super::recovery::Round> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: std::result::Result<(), &'static str> = async {
        loop {
            let now = start.elapsed().as_millis() as u64;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if recovery.enabled() && !recovery.frozen() && session.recovery_due(start.elapsed().as_millis() as u64) {
                        recovery.stall();
                        log::warn!("input recovery trigger: contiguous input progress deadline");
                    } else if reliable == Some(true) && (!recovery.frozen() || installed_epoch > runtime.as_ref().map_or(0, |r| r.machine.epoch())) {
                        if let Err(reason) = session.tick(start.elapsed().as_millis() as u64) {
                            if runtime.is_some() && matches!(reason, "reliable input stalled" | "DTLS output stalled") {
                                recovery.stall();
                                log::warn!("input recovery trigger: {reason}");
                            } else { return Err(reason); }
                        }
                    }
                    else if reliable.is_none() && start.elapsed() > Duration::from_secs(6) {
                        return Err("transport Hello timeout");
                    }
                },
                Some((sequence, generation)) = completions.recv() => {
                    if generation.as_ref().is_none_or(Generation::valid) { session.processed(sequence, now)?; }
                },
                permit = wire_tx.reserve(), if direct_output.is_some() || (session.has_output() && (pending_probe.is_some() || runtime.as_ref().is_none_or(|r| r.machine.receive_epoch(now).is_some()))) => {
                    let permit = permit.map_err(|_| "DTLS writer closed")?;
                    if let Some(packet) = direct_output.take() { permit.send(packet); }
                    else if let Some((created, bytes)) = session.pop_output() {
                        let bytes = envelope(bytes, &runtime, false, start.elapsed().as_millis() as u64, pending_probe.map(|r| r.next_epoch()))?;
                        permit.send(WirePacket { bytes, deadline: start + Duration::from_millis(created) + timeout.duration(), generation: recovery.generation() });
                    }
                }
                command = commands.recv(), if direct_output.is_none() && (recovery.frozen() || reliable != Some(true) || !session.ready() || session.can_send()) => {
                    let Some(Command { mut bytes, generation }) = command else { break; };
                    let event = decode(&bytes).ok_or("invalid application send")?;
                    if !direct(&event) && (recovery.frozen() || generation.as_ref().is_some_and(|g| !g.valid())) { continue; }
                    if direct(&event) {
                        if let ProtoEvent::Hello { magic, commit, capabilities } = event {
                            let (b,n): ([u8;MAX_EVENT_SIZE],usize) = ProtoEvent::Hello {
                                magic, commit, capabilities: (capabilities & !protocol::CAP_INPUT_RECOVERY_V1) | CAP_KCP_INPUT_V1 | CAP_UDP_MOTION_V1
                                    | if recoverable { protocol::CAP_INPUT_RECOVERY_V1 } else { 0 }
                                    | if dialer { CAP_KCP_REQUEST } else { 0 } }.into();
                            bytes = b[..n].to_vec();
                        }
                        let io_timeout = if kcp_selected.load(Ordering::Acquire) { timeout.duration() } else { CLOSE_TIMEOUT };
                        direct_output = Some(WirePacket { bytes, deadline: Instant::now() + io_timeout, generation: None });
                    } else if reliable == Some(false) {
                        direct_output = Some(WirePacket { bytes, deadline: Instant::now() + CLOSE_TIMEOUT, generation: None });
                    } else if hybrid == Some(true) {
                        let (unreliable, bytes) = motion_sender.pack(event, bytes)?;
                        if unreliable {
                            let bytes = envelope(bytes, &runtime, true, start.elapsed().as_millis() as u64, None)?;
                            motion_slot.replace(WirePacket { bytes, deadline: Instant::now() + timeout.duration(), generation: recovery.generation() });
                        } else { session.send(&bytes, start.elapsed().as_millis() as u64)?; }
                    } else { session.send(&bytes, start.elapsed().as_millis() as u64)?; }
                }
                permit = deliver.reserve(), if runtime.as_ref().is_none_or(|r| r.machine.receive_epoch(now).is_some()) && (!pending.is_empty() || session.has_received() || (session.ready() && motion_receiver.ready())) => {
                    let permit = permit.map_err(|_| "application receiver closed")?;
                    if pending.is_empty() {
                        if let Some((sequence, mut bytes)) = session.receive() {
                            if hybrid == Some(true) {
                                let frame = motion::Frame::decode(&bytes, true)?;
                                if let Some(bytes) = motion_receiver.checkpoint(&frame, sequence)? {
                                    pending.push_back(Delivery { bytes, receipt: None });
                                }
                                bytes = frame.critical;
                            }
                            let event = decode(&bytes).ok_or("invalid reliable application frame")?;
                            if direct(&event) { return Err("direct event in reliable channel"); }
                            pending.push_back(Delivery { bytes, receipt: Some(Receipt { sequence,
                                completed: completed.clone(), cancel: cancel.clone(), deadline: Instant::now() + timeout.duration(), generation: recovery.generation(), recovery: recovery.clone() }) });
                        } else if let Some(bytes) = motion_receiver.take()? {
                            pending.push_back(Delivery { bytes, receipt: None });
                        }
                    }
                    if let Some(mut delivery) = pending.pop_front() {
                        if recovery.enabled() && delivery.receipt.is_none() {
                            delivery.receipt = Some(Receipt { sequence: 0, completed: completed.clone(), cancel: cancel.clone(), deadline: Instant::now() + timeout.duration(), generation: recovery.generation(), recovery: recovery.clone() });
                        }
                        permit.send(delivery);
                    }
                }
                packet = input_rx.recv(), if runtime.is_some() || session.can_input() => {
                    let Some(mut bytes) = packet else { break; };
                    let now = start.elapsed().as_millis() as u64;
                    if bytes.first() == Some(&protocol::TAG) {
                        if recovery_selected != Some(true) { return Err("unnegotiated input recovery"); }
                        let frame = Envelope::decode(&bytes).ok_or("invalid recovery envelope")?;
                        if runtime.is_none() {
                            let Envelope::Reliable { session: identity, epoch: 0, source: Role::Dialer, frame: mousehop_proto::transport::Frame::Offer { .. } } = &frame else { return Err("invalid recovery bootstrap"); };
                            if dialer { return Err("unexpected recovery bootstrap"); }
                            runtime = Some(Runtime::new(*identity, Role::Acceptor, timeouts.peer.milliseconds(), now, recovery.clone())?);
                        }
                        let runtime = runtime.as_mut().unwrap();
                        if let Envelope::Control(control) = frame {
                            let outcome = runtime.step(now, RecoveryInput::Control(control))?;
                            control_output(&outcome, &control_tx);
                            continue;
                        }
                        if !runtime.machine.accepts_data(now, &frame) && !pending_probe.is_some_and(|round| {
                            matches!(frame, Envelope::Reliable { .. }) && frame.matches(round.session(), round.next_epoch(), Role::Acceptor)
                        }) { continue; }
                        let outcome = runtime.step(now, RecoveryInput::PeerActivity)?;
                        control_output(&outcome, &control_tx);
                        bytes = match frame { Envelope::Reliable { frame, .. } => frame.encode(), Envelope::Motion { bytes, .. } => bytes, _ => unreachable!() };
                    } else if recovery_selected == Some(true) && matches!(bytes.first(), Some(&TAG) | Some(&MOTION_TAG)) {
                        return Err("unscoped input on recovery connection");
                    }
                    if bytes.first() == Some(&MOTION_TAG) {
                        if reliable != Some(true) || hybrid != Some(true) {
                            return Err("unnegotiated UDP motion");
                        }
                        // UDP may overtake the KCP bootstrap. Retain one sample,
                        // but deliver nothing until the reliable session is ready.
                        let frame = motion::Frame::decode(&bytes, false)?;
                        let dependency = frame.dependency;
                        if motion_receiver.datagram(frame) {
                            session.fresh_motion(dependency, start.elapsed().as_millis() as u64);
                        }
                    } else if bytes.first() == Some(&TAG) {
                        if reliable != Some(true) { return Err("unexpected KCP frame in legacy mode"); }
                        session.input(&bytes, start.elapsed().as_millis() as u64)?;
                        if pending_probe.is_some() && session.has_received() { return Err("business input before recovery activation"); }
                    }
                    else {
                        let Some(event) = decode(&bytes) else { continue; };
                        if matches!(event, ProtoEvent::Ping | ProtoEvent::Pong(_)) {
                            // DTLS authenticates the sender. Heartbeats only renew
                            // peer liveness, never reliable consumption deadlines.
                            session.peer_activity(start.elapsed().as_millis() as u64);
                            if let Some(runtime) = runtime.as_mut() {
                                let outcome = runtime.step(now, RecoveryInput::PeerActivity)?;
                                control_output(&outcome, &control_tx);
                            }
                        }
                        if let ProtoEvent::Hello { magic, capabilities, .. } = event {
                            if magic != PROTOCOL_MAGIC { return Err("foreign Hello"); }
                            if !dialer {
                                let requested = capabilities & CAP_KCP_REQUEST != 0;
                                if reliable.is_some_and(|mode| mode != requested) {
                                    return Err("transport selection changed within session");
                                }
                                reliable = Some(requested);
                                kcp_selected.store(requested, Ordering::Release);
                            }
                            if reliable == Some(true) {
                                let recovery_enabled = recoverable && capabilities & protocol::CAP_INPUT_RECOVERY_V1 != 0;
                                if recovery_selected.is_some_and(|previous| previous != recovery_enabled) { return Err("recovery selection changed within session"); }
                                recovery_selected = Some(recovery_enabled);
                                if recovery_enabled && dialer && runtime.is_none() {
                                    let mut identity = [0; 16];
                                    rustls::crypto::ring::default_provider().secure_random.fill(&mut identity).map_err(|_| "recovery random identity failed")?;
                                    runtime = Some(Runtime::new(identity, Role::Dialer, timeouts.peer.milliseconds(), now, recovery.clone())?);
                                }
                                let enabled = capabilities & CAP_UDP_MOTION_V1 != 0;
                                if hybrid.is_some_and(|previous| previous != enabled) {
                                    return Err("motion selection changed within session");
                                }
                                hybrid = Some(enabled);
                                session.hello(capabilities & CAP_KCP_INPUT_V1 != 0, now)?;
                            }
                        } else if !direct(&event) && reliable != Some(false) {
                            return Err("unreliable input before transport selection");
                        }
                        deliver.try_send(Delivery { bytes, receipt: None }).map_err(|_| "application receive budget")?;
                    }
                }
            }
            let now = start.elapsed().as_millis() as u64;
            if recovery.failed.load(Ordering::Acquire) { return Err("local input recovery safety failure"); }
            if let Some(runtime) = runtime.as_mut() {
                if recovery.stalled.swap(false, Ordering::AcqRel) {
                    let outcome = runtime.step(now, RecoveryInput::Start)?;
                    control_output(&outcome, &control_tx);
                }
                if let Some(input) = runtime.response() {
                    if let RecoveryInput::Installed { round, result: Ok(()) } = input {
                        if !runtime.machine.installation_pending(round, now) || pending_probe.is_some() {
                            // Ignore stale/duplicate local callbacks before touching
                            // KCP, motion, or sequence state in the live epoch.
                        } else if dialer {
                            let identity = round.session();
                            let id = (u64::from_le_bytes(identity[..8].try_into().unwrap()) ^ round.next_epoch()).max(1);
                            let conv = ((u64::from_le_bytes(identity[8..].try_into().unwrap()) ^ round.next_epoch()) as u32).max(1);
                            session = Session::recovered(dialer, id, conv, now, timeouts)?;
                            session.recovery_probe()?;
                            installed_epoch = round.next_epoch();
                            motion_sender = motion::Sender::default();
                            motion_receiver = motion::Receiver::default();
                            pending.clear();
                            pending_probe = Some(round);
                        } else {
                            let outcome = runtime.step(now, RecoveryInput::Installed { round, result: Ok(()) })?;
                            control_output(&outcome, &control_tx);
                        }
                    } else {
                        let outcome = runtime.step(now, input)?;
                        control_output(&outcome, &control_tx);
                    }
                }
                if pending_probe.is_some() && session.recovery_probe_confirmed() {
                    let round = pending_probe.take().unwrap();
                    let outcome = runtime.step(now, RecoveryInput::Installed { round, result: Ok(()) })?;
                    control_output(&outcome, &control_tx);
                }
                let outcome = runtime.step(now, RecoveryInput::Tick)?;
                control_output(&outcome, &control_tx);
                // Freeze discards only the retired generation. Once the new
                // receive gate is installed, the peer may send before our last
                // ActivateAck arrives; preserve its checkpoint + critical pair.
                if recovery.frozen() && installed_epoch == runtime.machine.epoch() {
                    pending.clear();
                    motion_slot.take();
                }
                if let Some(epoch) = runtime.machine.receive_epoch(now).filter(|epoch| *epoch > installed_epoch) {
                    let identity = runtime.machine.session();
                    let id = (u64::from_le_bytes(identity[..8].try_into().unwrap()) ^ epoch).max(1);
                    let conv = ((u64::from_le_bytes(identity[8..].try_into().unwrap()) ^ epoch) as u32).max(1);
                    session = Session::recovered(dialer, id, conv, now, timeouts)?;
                    motion_sender = motion::Sender::default();
                    motion_receiver = motion::Receiver::default();
                    pending.clear();
                    installed_epoch = epoch;
                }
            }
            let is_ready = reliable == Some(false) || session.ready() || recovery.frozen() && recovery.enabled();
            ready.store(is_ready, Ordering::Release);
            if is_ready {
                *status.lock().unwrap() = if recovery.frozen() { "Recovering" } else if reliable == Some(false) { "Legacy" } else { "KCP" }.to_owned();
            }
            if direct_output.as_ref().is_some_and(|p| Instant::now() >= p.deadline) {
                if recovery.enabled() { direct_output = None; recovery.stall(); }
                else { return Err("DTLS output stalled"); }
            }
        }
        Ok(())
    }.await;
    if let Err(reason) = result {
        *status.lock().unwrap() = format!("Failed: {reason}");
        log::warn!("KCP session {:?} closed: {reason}", conn.remote_addr());
    } else {
        *status.lock().unwrap() = "Disconnected".to_owned();
    }
    ready.store(false, Ordering::Release);
    recovery.close();
    cancel.cancel();
    reader.abort();
    writer.abort();
    drop(deliver);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, conn.close()).await;
}

#[async_trait]
impl Conn for Peer {
    async fn connect(&self, _addr: SocketAddr) -> Result<()> {
        Err(closed())
    }
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        let mut receiver = self.receive.lock().await;
        let delivery = loop {
            let delivery = tokio::select! { biased; _ = self.cancel.cancelled() => return Err(closed()), b = receiver.recv() => b.ok_or_else(closed)? };
            if delivery
                .receipt
                .as_ref()
                .and_then(Receipt::generation)
                .is_some_and(|g| !g.valid())
            {
                continue;
            }
            break delivery;
        };
        if delivery.bytes.len() > buf.len() {
            self.cancel.cancel();
            return Err(closed());
        }
        let n = delivery.bytes.len();
        buf[..n].copy_from_slice(&delivery.bytes);
        *self.receipt.lock().unwrap() = delivery.receipt;
        Ok(n)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Ok((
            self.recv(buf).await?,
            self.remote_addr().ok_or_else(closed)?,
        ))
    }
    async fn send(&self, buf: &[u8]) -> Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(closed());
        }
        let event = decode(buf).ok_or_else(closed)?;
        if !direct(&event) && self.recovery.frozen() {
            return Ok(buf.len());
        }
        if !direct(&event) && !self.ready.load(Ordering::Acquire) {
            return Err(closed());
        }
        // A full bounded queue is backpressure, not a broken connection.
        // Continue servicing acknowledgements in the driver while the producer waits.
        let queued = tokio::select! {
            _ = self.cancel.cancelled() => false,
            result = tokio::time::timeout(self.timeout, self.send.send(Command { bytes: buf.to_vec(), generation: self.recovery.generation() })) => {
                matches!(result, Ok(Ok(())))
            }
        };
        if !queued {
            if self.recovery.enabled() && !self.cancel.is_cancelled() {
                self.recovery.stall();
                return Ok(buf.len());
            }
            log::warn!(
                "KCP application send queue did not drain within {:?}",
                self.timeout
            );
            self.cancel.cancel();
            return Err(closed());
        }
        Ok(buf.len())
    }
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        if Some(target) != self.remote_addr() {
            return Err(closed());
        }
        self.send(buf).await
    }
    fn local_addr(&self) -> Result<SocketAddr> {
        self.raw.local_addr()
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        self.raw.remote_addr()
    }
    async fn close(&self) -> Result<()> {
        self.cancel.cancel();
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn r12_stale_install_callback_cannot_reset_live_kcp() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_recoverable(
                    araw,
                    Some(InputTransport::KcpRequired),
                    StallTimeout::default(),
                );
                let b = attach_recoverable(braw, None, StallTimeout::default());
                negotiate(&a, &b).await;
                let link = lifecycle(&a).unwrap();
                let identity = link.generation().unwrap().session;
                let mut model = super::super::recovery::Recovery::new(
                    identity,
                    0,
                    Role::Dialer,
                    [1; 16],
                    Role::Dialer,
                    3000,
                    0,
                )
                .unwrap();
                let old_round = model.step(0, RecoveryInput::Start).barrier().unwrap();
                let barriers = [mock_barriers(&a, true), mock_barriers(&b, true)];
                link.stalled.store(true, Ordering::Release);
                recovered(&a, &b, 1).await;
                let mut buf = [0; MAX_EVENT_SIZE];
                a.send(&encode(ProtoEvent::Ack(301))).await.unwrap();
                b.recv(&mut buf).await.unwrap();
                receipt(&b).unwrap().complete();
                link.installed(old_round, Ok(()));
                tokio::time::sleep(Duration::from_millis(30)).await;
                a.send(&encode(ProtoEvent::Ack(302))).await.unwrap();
                let n = tokio::time::timeout(Duration::from_millis(400), b.recv(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&buf[..n], encode(ProtoEvent::Ack(302)));
                receipt(&b).unwrap().complete();
                a.close().await.unwrap();
                b.close().await.unwrap();
                for barrier in barriers {
                    barrier.abort();
                }
            })
            .await;
    }
    #[tokio::test]
    async fn r15_asymmetric_thresholds_and_unnegotiated_peer_keep_safe_behavior() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for supported in [true, false] {
                    let (araw, braw) = pair();
                    let a = attach_recoverable(
                        araw.clone(),
                        Some(InputTransport::KcpRequired),
                        StallTimeout::try_from(100).unwrap(),
                    );
                    let b = if supported {
                        attach_recoverable(braw.clone(), None, StallTimeout::try_from(600).unwrap())
                    } else {
                        attach(braw.clone(), None, StallTimeout::try_from(600).unwrap())
                    };
                    negotiate(&a, &b).await;
                    assert_eq!(lifecycle(&a).unwrap().enabled(), supported);
                    let barriers = [mock_barriers(&a, true), mock_barriers(&b, true)];
                    for raw in [&araw, &braw] {
                        raw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .drop_transport
                            .store(true, Ordering::Relaxed);
                    }
                    a.send(&encode(ProtoEvent::Ack(55))).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(220)).await;
                    if supported {
                        assert!(ready(&a) && ready(&b));
                        for raw in [&araw, &braw] {
                            raw.as_any()
                                .downcast_ref::<MemoryConn>()
                                .unwrap()
                                .drop_transport
                                .store(false, Ordering::Relaxed);
                        }
                        recovered(&a, &b, 1).await;
                    } else {
                        assert!(!ready(&a));
                        assert!(status(&a).contains("reliable input stalled"));
                    }
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                    for barrier in barriers {
                        barrier.abort();
                    }
                }
            })
            .await;
    }
    #[tokio::test]
    async fn r12_final_ack_loss_preserves_new_epoch_motion_checkpoint_and_critical_input() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new().run_until(async {
            let (araw,braw) = pair();
            let a = attach_recoverable(araw, Some(InputTransport::KcpRequired), StallTimeout::default());
            let b = attach_recoverable(braw.clone(), None, StallTimeout::default());
            negotiate(&a,&b).await;
            let barriers = [mock_barriers(&a,true),mock_barriers(&b,true)];
            braw.as_any().downcast_ref::<MemoryConn>().unwrap().drop_activate_ack.store(true, Ordering::Relaxed);
            lifecycle(&a).unwrap().stalled.store(true, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(1), async {
                while lifecycle(&b).unwrap().generation().unwrap().epoch == 0 || lifecycle(&b).unwrap().frozen() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap();
            assert!(lifecycle(&a).unwrap().frozen());
            // Keep UDP out so the reliable checkpoint must produce two local
            // deliveries while the dialer still awaits the final control Ack.
            braw.as_any().downcast_ref::<MemoryConn>().unwrap().drop_motion.store(true, Ordering::Relaxed);
            b.send(&encode(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { time: 1, dx: 5.0, dy: 0.0 })))).await.unwrap();
            b.send(&encode(ProtoEvent::Ack(201))).await.unwrap();
            let mut buf = [0; MAX_EVENT_SIZE];
            let n = tokio::time::timeout(Duration::from_millis(400), a.recv(&mut buf)).await.unwrap().unwrap();
            assert!(matches!(decode(&buf[..n]), Some(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, .. }))) if dx == 5.0));
            receipt(&a).unwrap().complete();
            let n = tokio::time::timeout(Duration::from_millis(400), a.recv(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], encode(ProtoEvent::Ack(201)));
            receipt(&a).unwrap().complete();
            braw.as_any().downcast_ref::<MemoryConn>().unwrap().drop_activate_ack.store(false, Ordering::Relaxed);
            recovered(&a,&b,1).await;
            a.close().await.unwrap(); b.close().await.unwrap();
            for barrier in barriers { barrier.abort(); }
        }).await;
    }
    #[tokio::test]
    async fn r12_send_queue_deadline_recovers_only_when_negotiated() {
        for enabled in [false, true] {
            let (raw, _) = pair();
            let (send, _commands) = mpsc::channel(1);
            send.try_send(Command {
                bytes: encode(ProtoEvent::Ack(1)),
                generation: None,
            })
            .unwrap();
            let (_, receive) = mpsc::channel(1);
            let link = Arc::new(Link::default());
            link.enabled.store(enabled, Ordering::Release);
            let peer = Peer {
                raw,
                send,
                receive: AsyncMutex::new(receive),
                receipt: Mutex::new(None),
                ready: Arc::new(AtomicBool::new(true)),
                status: Arc::new(Mutex::new("KCP".into())),
                cancel: CancellationToken::new(),
                timeout: Duration::from_millis(5),
                recovery: link.clone(),
            };
            assert_eq!(
                peer.send(&encode(ProtoEvent::Ack(2))).await.is_ok(),
                enabled
            );
            assert_eq!(peer.cancel.is_cancelled(), !enabled);
            assert_eq!(link.stalled.load(Ordering::Acquire), enabled);
        }
    }

    #[tokio::test]
    async fn r12_direct_output_deadline_recovery_and_permanent_writer_bound() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for permanent in [false, true] {
                    let (araw, braw) = pair();
                    let a = attach_recoverable(
                        araw.clone(),
                        Some(InputTransport::KcpRequired),
                        StallTimeout::default(),
                    );
                    let b = attach_recoverable(braw, None, StallTimeout::default());
                    negotiate(&a, &b).await;
                    let barriers = [mock_barriers(&a, true), mock_barriers(&b, true)];
                    let consumer = spawn_local({
                        let b = b.clone();
                        async move {
                            let mut buf = [0; MAX_CLIPBOARD_SIZE];
                            while b.recv(&mut buf).await.is_ok() {
                                if let Some(r) = receipt(&b) {
                                    r.complete();
                                }
                            }
                        }
                    });
                    araw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .send_delay
                        .store(if permanent { 10000 } else { 1000 }, Ordering::Relaxed);
                    for _ in 0..20 {
                        a.send(&encode(ProtoEvent::Ping)).await.unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    if permanent {
                        tokio::time::timeout(Duration::from_millis(1600), async {
                            while ready(&a) {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        })
                        .await
                        .unwrap();
                    } else {
                        araw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .send_delay
                            .store(0, Ordering::Relaxed);
                        recovered(&a, &b, 1).await;
                        assert!(Arc::ptr_eq(raw(&a), &araw));
                    }
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                    consumer.abort();
                    for barrier in barriers {
                        barrier.abort();
                    }
                }
            })
            .await;
    }

    #[tokio::test]
    async fn r12_r13_old_udp_and_session_do_not_move_new_epoch_or_replay_pause() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new().run_until(async {
            let (araw,braw) = pair();
            let a = attach_recoverable(araw.clone(), Some(InputTransport::KcpRequired), StallTimeout::default());
            let b = attach_recoverable(braw, None, StallTimeout::default());
            negotiate(&a,&b).await;
            let barriers = [mock_barriers(&a,true),mock_barriers(&b,true)];
            let old = lifecycle(&a).unwrap().generation().unwrap();
            let motion_event = |dx| ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { time: 1, dx, dy: 0.0 }));
            a.send(&encode(motion_event(40.0))).await.unwrap();
            let mut buf = [0; MAX_EVENT_SIZE];
            b.recv(&mut buf).await.unwrap();
            let old_motion_receipt = receipt(&b).unwrap();
            lifecycle(&a).unwrap().stalled.store(true, Ordering::Release);
            recovered(&a,&b,1).await;
            assert!(!old_motion_receipt.valid());
            old_motion_receipt.abort();
            let (_, bytes) = motion::Sender::default().pack(motion_event(900.0), encode(motion_event(900.0))).unwrap();
            for (session,epoch) in [(old.session,0),([99;16],1)] {
                let stale = Envelope::Motion { session, epoch, source: Role::Dialer, bytes: bytes.clone() }.encode().unwrap();
                araw.send(&stale).await.unwrap();
            }
            a.send(&encode(motion_event(3.0))).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(1), b.recv(&mut buf)).await.unwrap().unwrap();
            assert!(matches!(decode(&buf[..n]), Some(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. }))) if dx == 3.0 && dy == 0.0));
            receipt(&b).unwrap().complete();
            assert!(ready(&a) && ready(&b));
            a.close().await.unwrap(); b.close().await.unwrap();
            for barrier in barriers { barrier.abort(); }
        }).await;
    }
    fn mock_barriers(conn: &ArcConn, complete: bool) -> tokio::task::JoinHandle<()> {
        let link = lifecycle(conn).unwrap();
        spawn_local(async move {
            loop {
                if let Some(request) = link.request() {
                    match request {
                        super::super::lifecycle::Request::Barrier(round) if complete => {
                            link.barrier_done(round, link.baseline(10.0, -20.0, 1).ok_or(()))
                        }
                        super::super::lifecycle::Request::Install { round, .. } => {
                            link.installed(round, Ok(()))
                        }
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }
    async fn recovered(a: &ArcConn, b: &ArcConn, epoch: u64) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                assert!(ready(a) && ready(b), "{} / {}", status(a), status(b));
                if [a, b].iter().all(|conn| {
                    let link = lifecycle(conn).unwrap();
                    link.generation().is_some_and(|g| g.epoch == epoch) && !link.frozen()
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn r11_r12_memory_1200_1500ms_blackouts_and_simultaneous_recovery() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for pause in [1200, 1500] {
                    let (araw, braw) = pair();
                    let policy = Timeouts {
                        stall: StallTimeout::try_from(600).unwrap(),
                        peer: super::super::PeerTimeout::try_from(3000).unwrap(),
                    };
                    let a =
                        attach_recoverable(araw.clone(), Some(InputTransport::KcpRequired), policy);
                    let b = attach_recoverable(braw.clone(), None, policy);
                    negotiate(&a, &b).await;
                    let tasks = [mock_barriers(&a, true), mock_barriers(&b, true)];
                    for raw in [&araw, &braw] {
                        raw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .drop_transport
                            .store(true, Ordering::Relaxed);
                    }
                    a.send(&encode(ProtoEvent::Ack(1))).await.unwrap();
                    b.send(&encode(ProtoEvent::Ack(2))).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(pause)).await;
                    for raw in [&araw, &braw] {
                        raw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .drop_transport
                            .store(false, Ordering::Relaxed);
                    }
                    recovered(&a, &b, 1).await;
                    for (from, to) in [(&a, &b), (&b, &a)] {
                        from.send(&encode(ProtoEvent::Ack(99))).await.unwrap();
                        let mut buf = [0; MAX_EVENT_SIZE];
                        let n = tokio::time::timeout(Duration::from_secs(1), to.recv(&mut buf))
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(&buf[..n], encode(ProtoEvent::Ack(99)));
                        receipt(to).unwrap().complete();
                    }
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                    for task in tasks {
                        task.abort();
                    }
                }
            })
            .await;
    }

    #[tokio::test]
    async fn r12_r13_executed_click_lost_ack_old_receipt_and_each_stage_first_loss() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_recoverable(
                    araw.clone(),
                    Some(InputTransport::KcpRequired),
                    StallTimeout::try_from(600).unwrap(),
                );
                let b =
                    attach_recoverable(braw.clone(), None, StallTimeout::try_from(600).unwrap());
                negotiate(&a, &b).await;
                let tasks = [mock_barriers(&a, true), mock_barriers(&b, true)];
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .drop_control_once
                        .store(127, Ordering::Relaxed);
                }
                braw.as_any()
                    .downcast_ref::<MemoryConn>()
                    .unwrap()
                    .drop_reliable
                    .store(true, Ordering::Relaxed);
                let click = encode(ProtoEvent::Input(input_event::Event::Pointer(
                    input_event::PointerEvent::Button {
                        time: 0,
                        button: 272,
                        state: 1,
                    },
                )));
                a.send(&click).await.unwrap();
                let mut buf = [0; MAX_EVENT_SIZE];
                let n = b.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], click);
                let old = receipt(&b).unwrap();
                old.clone().complete(); // OS execution happened; only its acknowledgement is lost.
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !lifecycle(&a).unwrap().frozen() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                braw.as_any()
                    .downcast_ref::<MemoryConn>()
                    .unwrap()
                    .drop_reliable
                    .store(false, Ordering::Relaxed);
                recovered(&a, &b, 1).await;
                assert!(!old.valid());
                old.complete(); // A late completion must not cancel the new generation.
                braw.as_any()
                    .downcast_ref::<MemoryConn>()
                    .unwrap()
                    .drop_reliable
                    .store(false, Ordering::Relaxed);
                assert!(
                    tokio::time::timeout(Duration::from_millis(80), b.recv(&mut buf))
                        .await
                        .is_err()
                );
                a.send(&encode(ProtoEvent::Ack(101))).await.unwrap();
                let n = b.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], encode(ProtoEvent::Ack(101)));
                receipt(&b).unwrap().complete();
                assert!(ready(&a) && ready(&b));
                a.close().await.unwrap();
                b.close().await.unwrap();
                for task in tasks {
                    task.abort();
                }
            })
            .await;
    }

    #[tokio::test]
    async fn r14_blocked_backend_and_one_way_loss_close_with_fixed_deadline() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for fault in 0..3 {
                    let (araw, braw) = pair();
                    let policy = Timeouts {
                        stall: StallTimeout::try_from(600).unwrap(),
                        peer: super::super::PeerTimeout::try_from(3000).unwrap(),
                    };
                    let a = attach_recoverable(araw, Some(InputTransport::KcpRequired), policy);
                    let b = attach_recoverable(braw.clone(), None, policy);
                    negotiate(&a, &b).await;
                    let tasks = [mock_barriers(&a, true), mock_barriers(&b, fault != 0)];
                    if fault == 1 {
                        braw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .drop_transport
                            .store(true, Ordering::Relaxed);
                    }
                    if fault == 2 {
                        braw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .drop_reliable
                            .store(true, Ordering::Relaxed);
                    }
                    let started = Instant::now();
                    lifecycle(&a)
                        .unwrap()
                        .stalled
                        .store(true, Ordering::Release);
                    // Authenticated direct heartbeats cannot renew the recovery deadline.
                    tokio::time::timeout(Duration::from_millis(2400), async {
                        while ready(&a) {
                            let _ = b.send(&encode(ProtoEvent::Ping)).await;
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                    })
                    .await
                    .unwrap();
                    assert!(
                        started.elapsed() >= Duration::from_millis(1900),
                        "early exit: {} / {}",
                        status(&a),
                        status(&b)
                    );
                    assert!(status(&a).contains("recovery failed"), "{}", status(&a));
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                    for task in tasks {
                        task.abort();
                    }
                }
            })
            .await;
    }
    #[tokio::test]
    async fn r11_same_connection_survives_800ms_input_blackout() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_recoverable(
                    araw.clone(),
                    Some(InputTransport::KcpRequired),
                    StallTimeout::try_from(600).unwrap(),
                );
                let b =
                    attach_recoverable(braw.clone(), None, StallTimeout::try_from(600).unwrap());
                negotiate(&a, &b).await;
                let tasks: Vec<_> = [&a, &b]
                    .into_iter()
                    .map(|conn| {
                        let link = lifecycle(conn).unwrap();
                        spawn_local(async move {
                            loop {
                                if let Some(request) = link.request() {
                                    match request {
                                        super::super::lifecycle::Request::Barrier(round) => link
                                            .barrier_done(
                                                round,
                                                link.baseline(10.0, -20.0, 1).ok_or(()),
                                            ),
                                        super::super::lifecycle::Request::Install {
                                            round, ..
                                        } => link.installed(round, Ok(())),
                                        _ => {}
                                    }
                                }
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        })
                    })
                    .collect();
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .drop_transport
                        .store(true, Ordering::Relaxed);
                }
                a.send(&encode(ProtoEvent::Ack(71))).await.unwrap();
                tokio::time::sleep(Duration::from_millis(800)).await;
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .drop_transport
                        .store(false, Ordering::Relaxed);
                }
                assert!(
                    ready(&a) && ready(&b),
                    "R1.1: input blackout must preserve the authenticated connection for recovery"
                );
                tokio::time::timeout(Duration::from_secs(2), async {
                    while lifecycle(&a).unwrap().generation().unwrap().epoch == 0
                        || lifecycle(&b).unwrap().generation().unwrap().epoch == 0
                        || lifecycle(&a).unwrap().frozen()
                        || lifecycle(&b).unwrap().frozen()
                    {
                        assert!(ready(&a) && ready(&b), "{} / {}", status(&a), status(&b));
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(Arc::ptr_eq(raw(&a), &araw));
                a.send(&encode(ProtoEvent::Ack(72))).await.unwrap();
                let mut buf = [0; MAX_EVENT_SIZE];
                let n = tokio::time::timeout(Duration::from_secs(1), b.recv(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&buf[..n], encode(ProtoEvent::Ack(72)));
                receipt(&b).unwrap().complete();
                a.close().await.unwrap();
                b.close().await.unwrap();
                for task in tasks {
                    task.abort();
                }
            })
            .await;
    }
    struct MemoryConn {
        drop_activate_ack: AtomicBool,
        drop_control_once: AtomicU64,
        drop_reliable: AtomicBool,
        tx: mpsc::Sender<Vec<u8>>,
        rx: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
        send_delay: AtomicU64,
        drop_motion: AtomicBool,
        motion_count: AtomicU64,
        data_count: AtomicU64,
        old_peer: AtomicBool,
        drop_transport: AtomicBool,
    }
    #[async_trait]
    impl Conn for MemoryConn {
        async fn connect(&self, _: SocketAddr) -> Result<()> {
            Ok(())
        }
        async fn send(&self, b: &[u8]) -> Result<usize> {
            if let Some(frame) = Envelope::decode(b) {
                match frame {
                    Envelope::Control(control) => {
                        if control.kind == protocol::Kind::ActivateAck
                            && self.drop_activate_ack.load(Ordering::Relaxed)
                        {
                            return Ok(b.len());
                        }
                        let bit = 1 << control.kind as u8;
                        if self.drop_control_once.fetch_and(!bit, Ordering::Relaxed) & bit != 0 {
                            return Ok(b.len());
                        }
                    }
                    Envelope::Reliable { .. } if self.drop_reliable.load(Ordering::Relaxed) => {
                        return Ok(b.len());
                    }
                    Envelope::Motion { .. } if self.drop_motion.load(Ordering::Relaxed) => {
                        return Ok(b.len());
                    }
                    _ => {}
                }
            }
            if matches!(b.first(), Some(&TAG) | Some(&protocol::TAG))
                && self.drop_transport.load(Ordering::Relaxed)
            {
                return Ok(b.len());
            }
            if b.first() == Some(&MOTION_TAG) {
                self.motion_count.fetch_add(1, Ordering::Relaxed);
                if self.drop_motion.load(Ordering::Relaxed) {
                    return Ok(b.len());
                }
            }
            if let Some(mousehop_proto::transport::Frame::Data { payload, .. }) =
                mousehop_proto::transport::Frame::decode(b)
            {
                let mut offset = 0;
                while payload.len() >= offset + 24 {
                    let len =
                        u32::from_le_bytes(payload[offset + 20..offset + 24].try_into().unwrap())
                            as usize;
                    // The eight zero bytes of the handshake bootstrap are not input.
                    let bootstrap =
                        len == 8 && payload.get(offset + 24..offset + 32) == Some(&[0; 8]);
                    if payload[offset + 4] == 81 && !bootstrap {
                        self.data_count.fetch_add(1, Ordering::Relaxed);
                    }
                    offset += 24 + len;
                }
            }
            let delay = self.send_delay.load(Ordering::Relaxed);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            let mut bytes = b.to_vec();
            if self.old_peer.load(Ordering::Relaxed) {
                if let Some(ProtoEvent::Hello {
                    magic,
                    commit,
                    capabilities,
                }) = decode(b)
                {
                    bytes = encode(ProtoEvent::Hello {
                        magic,
                        commit,
                        capabilities: capabilities & !CAP_UDP_MOTION_V1,
                    });
                }
            }
            self.tx.try_send(bytes).map_err(|_| closed())?;
            Ok(b.len())
        }
        async fn recv(&self, b: &mut [u8]) -> Result<usize> {
            let v = self.rx.lock().await.recv().await.ok_or_else(closed)?;
            b[..v.len()].copy_from_slice(&v);
            Ok(v.len())
        }
        async fn recv_from(&self, b: &mut [u8]) -> Result<(usize, SocketAddr)> {
            Ok((self.recv(b).await?, self.local_addr()?))
        }
        async fn send_to(&self, b: &[u8], _: SocketAddr) -> Result<usize> {
            self.send(b).await
        }
        fn local_addr(&self) -> Result<SocketAddr> {
            Ok("127.0.0.1:4242".parse().unwrap())
        }
        fn remote_addr(&self) -> Option<SocketAddr> {
            self.local_addr().ok()
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
            self
        }
    }
    fn pair() -> (ArcConn, ArcConn) {
        let (atx, arx) = mpsc::channel(128);
        let (btx, brx) = mpsc::channel(128);
        (
            Arc::new(MemoryConn {
                drop_activate_ack: AtomicBool::new(false),
                drop_control_once: AtomicU64::new(0),
                drop_reliable: AtomicBool::new(false),
                tx: atx,
                rx: AsyncMutex::new(brx),
                send_delay: AtomicU64::new(0),
                drop_motion: AtomicBool::new(false),
                motion_count: AtomicU64::new(0),
                data_count: AtomicU64::new(0),
                old_peer: AtomicBool::new(false),
                drop_transport: AtomicBool::new(false),
            }),
            Arc::new(MemoryConn {
                drop_activate_ack: AtomicBool::new(false),
                drop_control_once: AtomicU64::new(0),
                drop_reliable: AtomicBool::new(false),
                tx: btx,
                rx: AsyncMutex::new(arx),
                send_delay: AtomicU64::new(0),
                drop_motion: AtomicBool::new(false),
                motion_count: AtomicU64::new(0),
                data_count: AtomicU64::new(0),
                old_peer: AtomicBool::new(false),
                drop_transport: AtomicBool::new(false),
            }),
        )
    }
    fn encode(event: ProtoEvent) -> Vec<u8> {
        let (b, n): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        b[..n].to_vec()
    }
    async fn negotiate(a: &ArcConn, b: &ArcConn) {
        let hello = encode(ProtoEvent::hello(*b"deadbeef"));
        a.send(&hello).await.unwrap();
        b.send(&hello).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !ready(a) || !ready(b) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut buf = [0; MAX_CLIPBOARD_SIZE];
        a.recv(&mut buf).await.unwrap();
        b.recv(&mut buf).await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_r7_d1_mixed_bidirectional_bursts_recover_checkpoints() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_required(araw.clone(), true);
                let b = attach_required(braw.clone(), false);
                negotiate(&a, &b).await;
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .send_delay
                        .store(60, Ordering::Relaxed);
                }
                let restore = async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    for raw in [&araw, &braw] {
                        raw.as_any()
                            .downcast_ref::<MemoryConn>()
                            .unwrap()
                            .send_delay
                            .store(0, Ordering::Relaxed);
                    }
                };
                let produce = |from: ArcConn| async move {
                    for n in 0..32 {
                        from.send(&encode(ProtoEvent::Input(Event::Pointer(
                            PointerEvent::Motion {
                                time: n,
                                dx: 1.0,
                                dy: 0.0,
                            },
                        ))))
                        .await
                        .unwrap();
                        from.send(&encode(ProtoEvent::Ack(n))).await.unwrap();
                    }
                };
                let consume = |to: ArcConn| async move {
                    let mut buf = [0; MAX_EVENT_SIZE];
                    let mut position = 0.0;
                    let mut next = 0;
                    while next < 32 {
                        let n = to.recv(&mut buf).await.unwrap();
                        match decode(&buf[..n]).unwrap() {
                            ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
                                dx, ..
                            })) => position += dx,
                            ProtoEvent::Ack(n) => {
                                assert_eq!(n, next);
                                assert_eq!(position, f64::from(next + 1));
                                next += 1;
                                receipt(&to).unwrap().complete();
                            }
                            event => panic!("unexpected {event:?}"),
                        }
                    }
                };
                tokio::time::timeout(Duration::from_secs(2), async {
                    tokio::join!(
                        restore,
                        produce(a.clone()),
                        produce(b.clone()),
                        consume(a.clone()),
                        consume(b.clone())
                    );
                })
                .await
                .unwrap();
                assert!(ready(&a) && ready(&b));
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn disconnect_r7_d1_permanently_blocked_writer_closes_within_bound() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_required(araw.clone(), true);
                let b = attach_required(braw, false);
                negotiate(&a, &b).await;
                araw.as_any()
                    .downcast_ref::<MemoryConn>()
                    .unwrap()
                    .send_delay
                    .store(60_000, Ordering::Relaxed);
                let start = Instant::now();
                a.send(&encode(ProtoEvent::Ack(1))).await.unwrap();
                tokio::time::timeout(Duration::from_millis(600), async {
                    while ready(&a) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                assert!(start.elapsed() >= Duration::from_millis(250));
                assert!(a.send(&encode(ProtoEvent::Ack(2))).await.is_err());
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn disconnect_r7_d1_short_writer_pause_preserves_reliable_burst() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_required(araw.clone(), true);
                let b = attach_required(braw, false);
                negotiate(&a, &b).await;
                let raw = araw.as_any().downcast_ref::<MemoryConn>().unwrap();
                raw.send_delay.store(80, Ordering::Relaxed);
                let restore = async {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    raw.send_delay.store(0, Ordering::Relaxed);
                };
                let producer = async {
                    for n in 0..48 {
                        a.send(&encode(ProtoEvent::Ack(n))).await.unwrap();
                    }
                };
                let consumer = async {
                    let mut buf = [0; MAX_EVENT_SIZE];
                    for n in 0..48 {
                        let len = b.recv(&mut buf).await.unwrap();
                        assert_eq!(&buf[..len], encode(ProtoEvent::Ack(n)));
                        receipt(&b).unwrap().complete();
                    }
                };
                tokio::time::timeout(Duration::from_secs(2), async {
                    tokio::join!(restore, producer, consumer);
                })
                .await
                .unwrap();
                assert!(ready(&a) && ready(&b));
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn disconnect_r7_d2_fresh_motion_survives_missing_kcp_feedback() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_required(araw.clone(), true);
                let b = attach_required(braw.clone(), false);
                negotiate(&a, &b).await;
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .drop_transport
                        .store(true, Ordering::Relaxed);
                }
                let mut buf = [0; MAX_EVENT_SIZE];
                for n in 0..45 {
                    for (from, to) in [(&a, &b), (&b, &a)] {
                        let bytes =
                            encode(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
                                time: n,
                                dx: 1.0,
                                dy: 0.0,
                            })));
                        from.send(&bytes).await.unwrap();
                        let len = to.recv(&mut buf).await.unwrap();
                        assert_eq!(&buf[..len], bytes);
                    }
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                assert!(ready(&a) && ready(&b));
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn direct_heartbeats_renew_custom_peer_timeout_without_kcp_feedback() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let timeouts = Timeouts {
                    stall: StallTimeout::default(),
                    peer: super::super::PeerTimeout::try_from(600).unwrap(),
                };
                let a = attach(araw.clone(), Some(InputTransport::KcpRequired), timeouts);
                let b = attach(braw.clone(), None, timeouts);
                negotiate(&a, &b).await;
                for raw in [&araw, &braw] {
                    raw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .drop_transport
                        .store(true, Ordering::Relaxed);
                }
                let mut buf = [0; MAX_EVENT_SIZE];
                for _ in 0..8 {
                    for (from, to, event) in
                        [(&a, &b, ProtoEvent::Ping), (&b, &a, ProtoEvent::Pong(true))]
                    {
                        let bytes = encode(event);
                        from.send(&bytes).await.unwrap();
                        let n = to.recv(&mut buf).await.unwrap();
                        assert_eq!(&buf[..n], bytes);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                assert!(
                    ready(&a) && ready(&b),
                    "heartbeats maintain both roles past 600ms"
                );
                tokio::time::timeout(Duration::from_millis(1000), async {
                    while ready(&a) || ready(&b) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("custom peer timeout closes silent sessions");
                assert!(status(&a).contains("peer activity timed out"));
                assert!(status(&b).contains("peer activity timed out"));
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn motion_uses_udp_and_lost_final_motion_precedes_reliable_button() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new()
            .run_until(async {
                for reverse in [false, true] {
                    let (araw, braw) = pair();
                    let a = attach_required(araw.clone(), true);
                    let b = attach_required(braw.clone(), false);
                    negotiate(&a, &b).await;
                    let (from, to, raw) = if reverse {
                        (&b, &a, &braw)
                    } else {
                        (&a, &b, &araw)
                    };
                    let raw = raw.as_any().downcast_ref::<MemoryConn>().unwrap();
                    let motion = |time, dx| {
                        encode(ProtoEvent::HandoverInput {
                            serial: 1,
                            event: Event::Pointer(PointerEvent::Motion { time, dx, dy: 0.0 }),
                        })
                    };
                    let mut buf = [0; MAX_EVENT_SIZE];
                    for i in 0..20 {
                        from.send(&motion(i, 1.0)).await.unwrap();
                        let n = to.recv(&mut buf).await.unwrap();
                        assert_eq!(&buf[..n], motion(i, 1.0));
                        assert!(receipt(to).is_none());
                    }
                    assert_eq!(raw.motion_count.load(Ordering::Relaxed), 20);
                    assert_eq!(raw.data_count.load(Ordering::Relaxed), 0);
                    raw.drop_motion.store(true, Ordering::Relaxed);
                    from.send(&motion(21, 5.0)).await.unwrap();
                    let button = encode(ProtoEvent::HandoverInput {
                        serial: 1,
                        event: Event::Pointer(PointerEvent::Button {
                            time: 22,
                            button: 272,
                            state: 1,
                        }),
                    });
                    from.send(&button).await.unwrap();
                    let n = to.recv(&mut buf).await.unwrap();
                    assert_eq!(&buf[..n], motion(21, 5.0));
                    assert!(receipt(to).is_none());
                    let n = to.recv(&mut buf).await.unwrap();
                    assert_eq!(&buf[..n], button);
                    receipt(to).unwrap().complete();
                    assert!(raw.data_count.load(Ordering::Relaxed) > 0);
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                }
            })
            .await;
    }

    #[tokio::test]
    async fn motion_overtaking_bootstrap_waits_without_closing_session() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new()
            .run_until(async {
                let (sender, raw_receiver) = pair();
                let receiver = attach_required(raw_receiver, false);
                sender
                    .send(&encode(ProtoEvent::Hello {
                        magic: PROTOCOL_MAGIC,
                        commit: *b"testpeer",
                        capabilities: CAP_KCP_INPUT_V1 | CAP_KCP_REQUEST | CAP_UDP_MOTION_V1,
                    }))
                    .await
                    .unwrap();
                let mut buf = [0; MAX_EVENT_SIZE];
                receiver.recv(&mut buf).await.unwrap();
                let event = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
                    time: 1,
                    dx: 3.0,
                    dy: 0.0,
                }));
                let expected = encode(event.clone());
                let (_, bytes) = motion::Sender::default()
                    .pack(event, expected.clone())
                    .unwrap();
                sender.send(&bytes).await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), receiver.recv(&mut buf))
                        .await
                        .is_err()
                );
                assert_eq!(status(&receiver), "Negotiating");
                let mut source = Session::new(true, 42, 17, 0);
                source.hello(true, 0).unwrap();
                for packet in source.drain() {
                    sender.send(&packet).await.unwrap();
                }
                let mut wire = [0; 1200];
                let n = sender.recv(&mut wire).await.unwrap();
                source.input(&wire[..n], 20).unwrap();
                for packet in source.drain() {
                    sender.send(&packet).await.unwrap();
                }
                let n = tokio::time::timeout(Duration::from_secs(1), receiver.recv(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&buf[..n], expected);
                assert!(receipt(&receiver).is_none());
                receiver.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn older_kcp_capability_retains_reliable_motion() {
        use input_event::{Event, PointerEvent};
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                for conn in [&araw, &braw] {
                    conn.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .old_peer
                        .store(true, Ordering::Relaxed);
                }
                let a = attach_required(araw.clone(), true);
                let b = attach_required(braw, false);
                negotiate(&a, &b).await;
                let bytes = encode(ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
                    time: 1,
                    dx: 2.0,
                    dy: 3.0,
                })));
                a.send(&bytes).await.unwrap();
                let mut buf = [0; MAX_EVENT_SIZE];
                let n = b.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], bytes);
                receipt(&b).unwrap().complete();
                assert_eq!(
                    araw.as_any()
                        .downcast_ref::<MemoryConn>()
                        .unwrap()
                        .motion_count
                        .load(Ordering::Relaxed),
                    0
                );
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn burst_input_waits_for_capacity_without_disconnect_or_reordering() {
        let _ = env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Warn)
            .try_init();
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let timeout = StallTimeout::try_from(1000).unwrap();
                let a = attach(araw, Some(InputTransport::KcpRequired), timeout);
                let b = attach(braw, None, timeout);
                negotiate(&a, &b).await;
                let producer = async {
                    for n in 0..256 {
                        a.send(&encode(ProtoEvent::Ack(n))).await.unwrap();
                    }
                };
                let consumer = async {
                    let mut buf = [0; MAX_EVENT_SIZE];
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    for n in 0..256 {
                        let len = b.recv(&mut buf).await.unwrap();
                        assert_eq!(&buf[..len], encode(ProtoEvent::Ack(n)));
                        receipt(&b).unwrap().complete();
                        tokio::task::yield_now().await;
                    }
                };
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(producer, consumer);
                })
                .await
                .unwrap();
                assert!(ready(&a) && ready(&b));
                a.close().await.unwrap();
                b.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn configured_writer_deadline_and_legacy_policy() {
        let (sender, _receiver) = pair();
        sender
            .as_any()
            .downcast_ref::<MemoryConn>()
            .unwrap()
            .send_delay
            .store(200, Ordering::Relaxed);
        assert!(
            !send_before_deadline(
                &sender,
                &[1],
                StallTimeout::try_from(100).unwrap().duration()
            )
            .await
        );
        assert!(
            send_before_deadline(
                &sender,
                &[2],
                StallTimeout::try_from(600).unwrap().duration()
            )
            .await
        );
        assert!(Arc::ptr_eq(
            &sender,
            &attach(
                sender.clone(),
                Some(InputTransport::Legacy),
                StallTimeout::try_from(1).unwrap()
            )
        ));
        tokio::task::LocalSet::new()
            .run_until(async {
                let (sender, receiver) = pair();
                let raw_receiver = receiver.clone();
                let receiver = attach(receiver, None, StallTimeout::try_from(1).unwrap());
                negotiate(&sender, &receiver).await;
                raw_receiver
                    .as_any()
                    .downcast_ref::<MemoryConn>()
                    .unwrap()
                    .send_delay
                    .store(30, Ordering::Relaxed);
                receiver.send(&encode(ProtoEvent::Ping)).await.unwrap();
                let mut buf = [0; MAX_EVENT_SIZE];
                let n = tokio::time::timeout(Duration::from_secs(1), sender.recv(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&buf[..n], encode(ProtoEvent::Ping));
                assert!(ready(&receiver));
                receiver.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn configured_receipt_deadline_incoming_and_outgoing() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for milliseconds in [100, 700] {
                    for reverse in [false, true] {
                        let timeout = StallTimeout::try_from(milliseconds).unwrap();
                        let (araw, braw) = pair();
                        let a = attach(araw, Some(InputTransport::KcpRequired), timeout);
                        let b = attach(braw, None, timeout);
                        negotiate(&a, &b).await;
                        let (from, to) = if reverse { (&b, &a) } else { (&a, &b) };
                        from.send(&encode(ProtoEvent::Ack(1))).await.unwrap();
                        let mut buf = [0; MAX_EVENT_SIZE];
                        tokio::time::timeout(Duration::from_secs(1), to.recv(&mut buf))
                            .await
                            .unwrap()
                            .unwrap();
                        let pending = receipt(to).unwrap();
                        let remaining = pending.deadline.saturating_duration_since(Instant::now());
                        assert!(remaining <= timeout.duration());
                        assert!(remaining > timeout.duration() - Duration::from_millis(80));
                        if milliseconds > 300 {
                            tokio::time::sleep(Duration::from_millis(350)).await;
                            assert!(pending.valid());
                            assert!(ready(from) && ready(to));
                        }
                        let (_done, wait) = tokio::sync::oneshot::channel();
                        tokio::time::timeout(Duration::from_secs(2), pending.clone().after(wait))
                            .await
                            .unwrap();
                        assert!(!pending.valid());
                        a.close().await.unwrap();
                        b.close().await.unwrap();
                    }
                }
            })
            .await;
    }
    #[tokio::test]
    async fn automatic_receiver_handles_legacy_and_kcp_peers_independently() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (old_sender, receiver) = pair();
                let legacy_receiver = attach(receiver, None, StallTimeout::default());
                let (sender, receiver) = pair();
                let kcp_sender = attach(
                    sender,
                    Some(InputTransport::KcpRequired),
                    StallTimeout::default(),
                );
                let kcp_receiver = attach(receiver, None, StallTimeout::default());
                negotiate(&old_sender, &legacy_receiver).await;
                negotiate(&kcp_sender, &kcp_receiver).await;
                assert_eq!(status(&legacy_receiver), "Legacy");
                assert_eq!(status(&kcp_sender), "KCP");
                assert_eq!(status(&kcp_receiver), "KCP");
                let message = encode(ProtoEvent::Ack(42));
                let mut buf = [0; MAX_CLIPBOARD_SIZE];
                old_sender.send(&message).await.unwrap();
                let n = legacy_receiver.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], message.as_slice());
                assert!(receipt(&legacy_receiver).is_none());
                legacy_receiver.send(&message).await.unwrap();
                let n = old_sender.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], message.as_slice());
                kcp_sender.send(&message).await.unwrap();
                let n = kcp_receiver.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], message.as_slice());
                receipt(&kcp_receiver).unwrap().complete();
                kcp_sender.close().await.unwrap();
                assert!(ready(&legacy_receiver));
                legacy_receiver.close().await.unwrap();
                kcp_receiver.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn required_peer_rejects_old_receiver_without_silent_fallback() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (sender, old_receiver) = pair();
                let sender = attach(
                    sender,
                    Some(InputTransport::KcpRequired),
                    StallTimeout::default(),
                );
                old_receiver
                    .send(&encode(ProtoEvent::hello(*b"old-peer")))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !status(&sender).starts_with("Failed:") {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(status(&sender).contains("does not support"));
                assert!(!ready(&sender));
                assert!(sender.send(&encode(ProtoEvent::Ack(1))).await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn capability_only_hello_does_not_select_kcp() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (sender, receiver) = pair();
                let receiver = attach(receiver, None, StallTimeout::default());
                sender
                    .send(&encode(ProtoEvent::Hello {
                        magic: PROTOCOL_MAGIC,
                        commit: *b"testpeer",
                        capabilities: CAP_KCP_INPUT_V1,
                    }))
                    .await
                    .unwrap();
                let mut buf = [0; MAX_CLIPBOARD_SIZE];
                receiver.recv(&mut buf).await.unwrap();
                assert!(ready(&receiver));
                assert_eq!(status(&receiver), "Legacy");
                sender
                    .send(&encode(ProtoEvent::Hello {
                        magic: PROTOCOL_MAGIC,
                        commit: *b"testpeer",
                        capabilities: CAP_KCP_INPUT_V1 | CAP_KCP_REQUEST,
                    }))
                    .await
                    .unwrap();
                assert!(receiver.recv(&mut buf).await.is_err());
                assert!(status(&receiver).contains("changed within session"));
            })
            .await;
    }

    #[tokio::test]
    async fn driver_gate_consumption_clipboard_and_same_address_reconnect() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let original = araw.clone();
                let a = attach_required(araw, true);
                let b = attach_required(braw, false);
                assert!(Arc::ptr_eq(raw(&a), &original));
                assert!(a.send(&encode(ProtoEvent::Ack(9))).await.is_err());
                negotiate(&a, &b).await;
                let input = encode(ProtoEvent::Ack(10));
                a.send(&input).await.unwrap();
                let mut buf = [0; MAX_CLIPBOARD_SIZE];
                let n = tokio::time::timeout(Duration::from_millis(200), b.recv(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&buf[..n], input.as_slice());
                let delivery = receipt(&b).unwrap();
                assert!(delivery.valid());
                delivery.complete();
                let clip = mousehop_proto::encode_clipboard_event(&ProtoEvent::Clipboard {
                    from_fingerprint: "test".into(),
                    content: "clipboard stays direct".into(),
                })
                .unwrap();
                a.send(&clip).await.unwrap();
                let n = b.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], clip.as_slice());
                assert!(receipt(&b).is_none());
                tokio::time::sleep(Duration::from_millis(350)).await;
                assert!(ready(&a) && ready(&b));
                a.send(&input).await.unwrap();
                b.recv(&mut buf).await.unwrap();
                let old = receipt(&b).unwrap();
                b.close().await.unwrap();
                assert!(!old.valid());
                let (araw, braw) = pair();
                let new_a = attach_required(araw, true);
                let new_b = attach_required(braw, false);
                negotiate(&new_a, &new_b).await;
                old.complete(); // cannot acknowledge or poison replacement state
                new_a.send(&encode(ProtoEvent::Ack(42))).await.unwrap();
                let n = new_b.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], encode(ProtoEvent::Ack(42)));
                receipt(&new_b).unwrap().complete();
                new_a.close().await.unwrap();
                new_b.close().await.unwrap();
                a.close().await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn real_dtls_loopback_keeps_certificate_and_bidirectional_delivery() {
        use webrtc_dtls::{
            config::{ClientAuthType, Config, ExtendedMasterSecretType},
            conn::DTLSConn,
            crypto::Certificate,
        };
        use webrtc_util::conn::Listener;
        tokio::task::LocalSet::new()
            .run_until(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let cert =
                        Certificate::generate_self_signed(vec!["kcp-loopback".into()]).unwrap();
                    let listener = webrtc_dtls::listener::listen(
                        "127.0.0.1:0",
                        Config {
                            certificates: vec![cert.clone()],
                            client_auth: ClientAuthType::RequireAnyClientCert,
                            extended_master_secret: ExtendedMasterSecretType::Require,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                    let udp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    udp.connect(listener.addr().await.unwrap()).await.unwrap();
                    let (client, accepted) = tokio::join!(
                        DTLSConn::new(
                            udp,
                            Config {
                                certificates: vec![cert],
                                insecure_skip_verify: true,
                                server_name: "ignored".into(),
                                extended_master_secret: ExtendedMasterSecretType::Require,
                                ..Default::default()
                            },
                            true,
                            None
                        ),
                        listener.accept()
                    );
                    let authenticated: ArcConn = Arc::new(client.unwrap());
                    let a = attach_recoverable(
                        authenticated.clone(),
                        Some(InputTransport::KcpRequired),
                        StallTimeout::default(),
                    );
                    let b = attach_recoverable(accepted.unwrap().0, None, StallTimeout::default());
                    assert!(
                        !raw(&b)
                            .as_any()
                            .downcast_ref::<DTLSConn>()
                            .unwrap()
                            .connection_state()
                            .await
                            .peer_certificates
                            .is_empty()
                    );
                    negotiate(&a, &b).await;
                    let barriers = [mock_barriers(&a, true), mock_barriers(&b, true)];
                    lifecycle(&a)
                        .unwrap()
                        .stalled
                        .store(true, Ordering::Release);
                    recovered(&a, &b, 1).await;
                    assert!(Arc::ptr_eq(raw(&a), &authenticated));
                    let mut buffer = [0; MAX_CLIPBOARD_SIZE];
                    for (from, to) in [(&a, &b), (&b, &a)] {
                        let event = encode(ProtoEvent::HandoverLeave {
                            serial: 42,
                            mode: 0,
                        });
                        from.send(&event).await.unwrap();
                        let n = to.recv(&mut buffer).await.unwrap();
                        assert_eq!(&buffer[..n], event.as_slice());
                        receipt(to).unwrap().complete();
                    }
                    a.close().await.unwrap();
                    b.close().await.unwrap();
                    listener.close().await.unwrap();
                    for barrier in barriers {
                        barrier.abort();
                    }
                })
                .await
                .unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn unfinished_backend_consumption_expires_despite_direct_heartbeats() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (araw, braw) = pair();
                let a = attach_required(araw, true);
                let b = attach_required(braw, false);
                negotiate(&a, &b).await;
                a.send(&encode(ProtoEvent::Ack(1))).await.unwrap();
                let mut buf = [0; MAX_CLIPBOARD_SIZE];
                b.recv(&mut buf).await.unwrap();
                let slow = receipt(&b).unwrap();
                for _ in 0..5 {
                    a.send(&encode(ProtoEvent::Ping)).await.unwrap();
                    b.recv(&mut buf).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(!slow.valid());
                assert!(!ready(&a));
                assert!(!ready(&b));
                assert!(b.recv(&mut buf).await.is_err());
            })
            .await;
    }
}
