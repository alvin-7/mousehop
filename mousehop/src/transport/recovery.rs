//! Deterministic recovery coordinator. Times are local monotonic milliseconds.
//! The caller drains/discards frozen capture input and invalidates old backend
//! tokens before completing Barrier. Installed acknowledges new KCP/motion/
//! receipt state, NOT merely scheduling its installation. Every callback enters
//! through step(), which checks both deadlines before processing it.
use mousehop_proto::transport::recovery::{
    Baseline, Control, Envelope, Kind, Ownership, Role, Session,
};

pub(crate) const RECOVERY_MS: u64 = 2000;
pub(crate) const RETRY_MS: u64 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Normal,
    Quiescing,
    AwaitCommit,
    AwaitCommitAck,
    Installing,
    AwaitActivate,
    AwaitActivateAck,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseReason {
    RecoveryTimeout,
    PeerTimeout,
    EpochExhausted,
    Identity,
    Protocol,
    Ownership,
    BarrierFailed,
    InstallFailed,
    ClockRegression,
}

/// An unforgeable-by-accident callback token; old session/round completions are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Round {
    session: Session,
    epoch: u64,
}
impl Round {
    pub(crate) fn session(self) -> Session {
        self.session
    }
    pub(crate) fn old_epoch(self) -> u64 {
        self.epoch
    }
    pub(crate) fn next_epoch(self) -> u64 {
        self.epoch + 1
    }
}

pub(crate) enum Input {
    Start,
    Control(Control),
    Barrier {
        round: Round,
        result: Result<Baseline, ()>,
    },
    Installed {
        round: Round,
        result: Result<(), ()>,
    },
    /// Only authenticated current-session liveness; never stale data/receipts.
    PeerActivity,
    Tick,
}

/// Immutable, bounded result of one event. No public fields let a driver change
/// a failed operation to success. No queues: at most one outbound control and
/// one outstanding barrier/install token. Repeated tokens must be coalesced.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    state: State,
    closed: Option<CloseReason>,
    control: Option<Control>,
    barrier: Option<Round>,
    install: Option<Round>,
    peer_baseline: Option<Baseline>,
}
impl Outcome {
    pub(crate) fn state(&self) -> State {
        self.state
    }
    pub(crate) fn closed(&self) -> Option<CloseReason> {
        self.closed
    }
    pub(crate) fn control(&self) -> Option<&Control> {
        self.control.as_ref()
    }
    pub(crate) fn barrier(&self) -> Option<Round> {
        self.barrier
    }
    pub(crate) fn install(&self) -> Option<Round> {
        self.install
    }
    pub(crate) fn peer_baseline(&self) -> Option<Baseline> {
        self.peer_baseline
    }
}

pub(crate) struct Recovery {
    session: Session,
    epoch: u64,
    role: Role,
    ownership: Ownership,
    owner: Role,
    state: State,
    closed: Option<CloseReason>,
    started: Option<u64>,
    last_now: u64,
    last_peer: u64,
    peer_ms: u64,
    last_send: Option<u64>,
    local: Option<Baseline>,
    peer: Option<Baseline>,
    prepared: bool,
    pending: Option<Control>,
    // Exactly ONE previous terminal response; replaced at each completion.
    // Neither delayed datagrams nor retries can allocate an unbounded history.
    terminal: Option<Control>,
}

impl Recovery {
    pub(crate) fn peer_remaining(&self, now: u64) -> u64 {
        self.peer_ms
            .saturating_sub(now.saturating_sub(self.last_peer))
    }
    pub(crate) fn installation_pending(&self, round: Round, now: u64) -> bool {
        self.state == State::Installing && self.round() == round && self.expired(now).is_none()
    }
    /// Update only a confirmed normal-phase handover. An in-flight handover
    /// must be rejected by the caller rather than guessed during recovery.
    pub(crate) fn set_ownership(&mut self, ownership: Ownership, owner: Role) -> bool {
        if self.state != State::Normal || ownership == [0; 16] {
            return false;
        }
        self.ownership = ownership;
        self.owner = owner;
        true
    }
    pub(crate) fn new(
        session: Session,
        epoch: u64,
        role: Role,
        ownership: Ownership,
        owner: Role,
        peer_ms: u64,
        now: u64,
    ) -> Result<Self, CloseReason> {
        if session == [0; 16] || ownership == [0; 16] {
            return Err(CloseReason::Identity);
        }
        if !(1..=60000).contains(&peer_ms) {
            return Err(CloseReason::Protocol);
        }
        Ok(Self {
            session,
            epoch,
            role,
            ownership,
            owner,
            state: State::Normal,
            closed: None,
            started: None,
            last_now: now,
            last_peer: now,
            peer_ms,
            last_send: None,
            local: None,
            peer: None,
            prepared: false,
            pending: None,
            terminal: None,
        })
    }
    pub(crate) fn session(&self) -> Session {
        self.session
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn role(&self) -> Role {
        self.role
    }
    pub(crate) fn recovery_started(&self) -> Option<u64> {
        self.started
    }
    fn round(&self) -> Round {
        Round {
            session: self.session,
            epoch: self.epoch,
        }
    }
    fn expired(&self, now: u64) -> Option<CloseReason> {
        if now < self.last_now {
            return Some(CloseReason::ClockRegression);
        }
        let recovery = self.started.filter(|start| now - start >= RECOVERY_MS);
        let peer = now - self.last_peer >= self.peer_ms;
        // When caller ticks late, report the deadline that actually expired first.
        if let Some(start) = recovery {
            if !peer
                || u128::from(start) + u128::from(RECOVERY_MS)
                    <= u128::from(self.last_peer) + u128::from(self.peer_ms)
            {
                return Some(CloseReason::RecoveryTimeout);
            }
        }
        peer.then_some(CloseReason::PeerTimeout)
    }
    /// Check immediately before every send/injection, including after awaits.
    /// The time-aware gates remain closed even if the caller missed a tick.
    pub(crate) fn send_epoch(&self, now: u64) -> Option<u64> {
        (self.state == State::Normal && self.expired(now).is_none()).then_some(self.epoch)
    }
    pub(crate) fn receive_epoch(&self, now: u64) -> Option<u64> {
        if self.expired(now).is_some() {
            return None;
        }
        match self.state {
            State::Normal => Some(self.epoch),
            State::AwaitActivate | State::AwaitActivateAck => self.epoch.checked_add(1),
            _ => None,
        }
    }
    /// Control frames always use step(Control); this gate is only for data.
    pub(crate) fn accepts_data(&self, now: u64, envelope: &Envelope) -> bool {
        !matches!(envelope, Envelope::Control(_))
            && self
                .receive_epoch(now)
                .is_some_and(|epoch| envelope.matches(self.session, epoch, self.role.peer()))
    }
    fn close(&mut self, reason: CloseReason) {
        self.closed = Some(reason);
        self.state = State::Closed;
        self.pending = None;
    }
    fn frame(&self, kind: Kind) -> Control {
        Control {
            session: self.session,
            epoch: self.epoch,
            source: self.role,
            kind,
            baseline: matches!(kind, Kind::Prepared | Kind::Commit)
                .then_some(self.local)
                .flatten(),
        }
    }
    fn publish(&mut self, kind: Kind) {
        let control = self.frame(kind);
        if self.pending.as_ref() != Some(&control) {
            self.pending = Some(control);
            self.last_send = None;
        }
    }
    fn start(&mut self, now: u64) {
        if self.state != State::Normal {
            return;
        }
        if self.epoch == u64::MAX {
            self.close(CloseReason::EpochExhausted);
            return;
        }
        self.started = Some(now);
        self.state = State::Quiescing;
        self.local = None;
        self.peer = None;
        self.prepared = false;
        self.publish(if self.role == Role::Dialer {
            Kind::Prepare
        } else {
            Kind::Request
        });
    }
    fn baseline_valid(&self, b: Baseline) -> bool {
        b.valid() && b.ownership == self.ownership && b.owner == self.owner
    }
    fn ready(&mut self) {
        if self.state != State::Quiescing || self.local.is_none() {
            return;
        }
        if self.role == Role::Dialer && self.peer.is_some() {
            self.state = State::AwaitCommitAck;
            self.publish(Kind::Commit);
        } else if self.role == Role::Acceptor && self.prepared {
            self.state = State::AwaitCommit;
            self.publish(Kind::Prepared);
        }
    }
    fn finish(&mut self) {
        self.terminal = Some(self.frame(if self.role == Role::Dialer {
            Kind::Activate
        } else {
            Kind::ActivateAck
        }));
        self.epoch += 1;
        self.state = State::Normal;
        self.started = None;
        self.pending = None;
    }
    fn emit(&mut self, now: u64, control: Option<Control>, immediate: bool) -> Option<Control> {
        if control.is_some()
            && (immediate || self.last_send.is_none_or(|last| now - last >= RETRY_MS))
        {
            self.last_send = Some(now);
            control
        } else {
            None
        }
    }
    fn on_control(&mut self, now: u64, c: Control) -> Option<Control> {
        if c.session != self.session || c.source != self.role.peer() {
            self.close(CloseReason::Identity);
            return None;
        }
        if !c.valid() {
            self.close(CloseReason::Protocol);
            return None;
        }
        if c.epoch < self.epoch {
            // No liveness refresh, freeze, or new barrier for a completed round.
            return self
                .terminal
                .as_ref()
                .filter(|t| {
                    t.epoch == c.epoch
                        && !(self.role == Role::Dialer && c.kind == Kind::ActivateAck)
                })
                .cloned();
        }
        if c.epoch > self.epoch {
            self.close(CloseReason::Protocol);
            return None;
        }
        if let Some(b) = c.baseline {
            if !self.baseline_valid(b) || self.peer.is_some_and(|old| old != b) {
                self.close(CloseReason::Ownership);
                return None;
            }
        }
        self.last_peer = now;
        if self.state == State::Normal {
            if !matches!(c.kind, Kind::Request | Kind::Prepare) {
                self.close(CloseReason::Protocol);
                return None;
            }
            self.start(now);
        }
        match (self.role, c.kind) {
            (_, Kind::Request) => {} // Both triggers merge into this one epoch.
            (Role::Acceptor, Kind::Prepare) => {
                self.prepared = true;
                self.ready();
            }
            (Role::Dialer, Kind::Prepared) => {
                self.peer = c.baseline;
                self.ready();
            }
            (Role::Acceptor, Kind::Commit) if self.state == State::AwaitCommit => {
                self.peer = c.baseline;
                self.state = State::Installing;
            }
            (Role::Dialer, Kind::CommitAck) if self.state == State::AwaitCommitAck => {
                self.state = State::Installing;
            }
            (Role::Acceptor, Kind::Activate) if self.state == State::AwaitActivate => {
                self.finish();
                return self.terminal.clone();
            }
            (Role::Dialer, Kind::ActivateAck) if self.state == State::AwaitActivateAck => {
                self.finish();
            }
            // Early or duplicate stages cannot advance gates. Reliable periodic
            // retransmission of the current stage repairs loss/reordering.
            _ => {}
        }
        None
    }
    pub(crate) fn step(&mut self, now: u64, input: Input) -> Outcome {
        let mut reply = None;
        let was_normal = self.state == State::Normal;
        if self.state != State::Closed {
            if let Some(reason) = self.expired(now) {
                self.close(reason);
            } else {
                self.last_now = now;
                match input {
                    Input::Start => self.start(now),
                    Input::PeerActivity => self.last_peer = now,
                    Input::Tick => {}
                    Input::Control(c) => reply = self.on_control(now, c),
                    Input::Barrier { round, result }
                        if round == self.round()
                            && self.state == State::Quiescing
                            && self.local.is_none() =>
                    {
                        match result {
                            Ok(b) if self.baseline_valid(b) => {
                                self.local = Some(b);
                                self.ready();
                            }
                            Ok(_) => self.close(CloseReason::Ownership),
                            Err(()) => self.close(CloseReason::BarrierFailed),
                        }
                    }
                    Input::Installed { round, result }
                        if round == self.round() && self.state == State::Installing =>
                    {
                        if result.is_err() {
                            self.close(CloseReason::InstallFailed);
                        } else if self.role == Role::Dialer {
                            self.state = State::AwaitActivateAck;
                            self.publish(Kind::Activate);
                        } else {
                            self.state = State::AwaitActivate;
                            self.publish(Kind::CommitAck);
                        }
                    }
                    _ => {} // Old-session/round and duplicate callbacks are inert.
                }
            }
        }
        let control = if self.state == State::Closed {
            None
        } else {
            let completed = !was_normal && self.state == State::Normal;
            self.emit(now, reply.or_else(|| self.pending.clone()), completed)
        };
        Outcome {
            state: self.state,
            closed: self.closed,
            control,
            barrier: (self.state == State::Quiescing && self.local.is_none()).then(|| self.round()),
            install: (self.state == State::Installing).then(|| self.round()),
            peer_baseline: self.peer,
        }
    }
}

#[cfg(test)]
mod tests;
