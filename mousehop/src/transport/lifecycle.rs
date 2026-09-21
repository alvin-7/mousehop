//! Shared, bounded bridge between the transport and the local input owner.
use super::recovery::{Input, Outcome, Recovery, Round, State};
use mousehop_proto::transport::recovery::{Baseline, Role, Session};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) struct Generation {
    pub(crate) session: Session,
    pub(crate) epoch: u64,
    active: Arc<AtomicBool>,
    deadline: Option<Instant>,
    committed: Arc<AtomicBool>,
    peer_deadline: Arc<Mutex<Option<Instant>>>,
}
impl Generation {
    fn new(session: Session, epoch: u64, deadline: Option<Instant>) -> Self {
        Self {
            session,
            epoch,
            active: Arc::new(AtomicBool::new(true)),
            deadline,
            committed: Arc::new(AtomicBool::new(false)),
            peer_deadline: Arc::new(Mutex::new(None)),
        }
    }
    pub(crate) fn valid(&self) -> bool {
        self.active.load(Ordering::Acquire)
            && self
                .peer_deadline
                .lock()
                .unwrap()
                .is_none_or(|deadline| Instant::now() < deadline)
            && (self.committed.load(Ordering::Acquire)
                || self
                    .deadline
                    .is_none_or(|deadline| Instant::now() < deadline))
    }
    fn retire(&self) {
        self.active.store(false, Ordering::Release);
    }
    fn update_peer_deadline(&self, remaining_ms: u64) {
        *self.peer_deadline.lock().unwrap() =
            Some(Instant::now() + Duration::from_millis(remaining_ms));
    }
    fn commit(&self) -> bool {
        if !self.valid() {
            return false;
        }
        self.committed.store(true, Ordering::Release);
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Request {
    Barrier(Round),
    Install { round: Round, peer: Baseline },
    Resumed { epoch: u64, peer: Baseline },
}

#[derive(Default, Debug)]
struct Local {
    request: Option<Request>,
    response: Option<InputResponse>,
    generation: Option<Generation>,
    // None means an ownership transition has not been acknowledged.
    ownership: Option<([u8; 16], Role)>,
}
#[derive(Debug)]
enum InputResponse {
    Barrier(Round, Result<Baseline, ()>),
    Installed(Round, Result<(), ()>),
}

#[derive(Default, Debug)]
pub(crate) struct Link {
    pub(crate) enabled: AtomicBool,
    pub(crate) frozen: AtomicBool,
    pub(crate) stalled: AtomicBool,
    pub(crate) failed: AtomicBool,
    local: Mutex<Local>,
}
impl Link {
    pub(crate) fn stall(&self) {
        if !self.frozen.swap(true, Ordering::AcqRel) {
            if let Some(generation) = self.generation() {
                generation.retire();
            }
            self.stalled.store(true, Ordering::Release);
        }
    }
    pub(crate) fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.close();
    }
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub(crate) fn frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }
    pub(crate) fn generation(&self) -> Option<Generation> {
        self.local.lock().unwrap().generation.clone()
    }
    pub(crate) fn request(&self) -> Option<Request> {
        self.local.lock().unwrap().request.take()
    }
    pub(crate) fn ownership(&self, serial: Option<u32>, owner: Role) {
        let mut local = self.local.lock().unwrap();
        local.ownership = serial.map(|serial| {
            let mut digest = [1; 16];
            digest[..4].copy_from_slice(&serial.to_le_bytes());
            (digest, owner)
        });
    }
    pub(crate) fn barrier_done(&self, round: Round, result: Result<Baseline, ()>) {
        self.local.lock().unwrap().response = Some(InputResponse::Barrier(round, result));
    }
    pub(crate) fn installed(&self, round: Round, result: Result<(), ()>) {
        self.local.lock().unwrap().response = Some(InputResponse::Installed(round, result));
    }
    pub(crate) fn baseline(&self, x: f64, y: f64, layout: u64) -> Option<Baseline> {
        let (ownership, owner) = self.local.lock().unwrap().ownership?;
        Some(Baseline {
            x,
            y,
            layout,
            ownership,
            owner,
        })
    }
    pub(crate) fn close(&self) {
        self.frozen.store(true, Ordering::Release);
        if let Some(generation) = self.generation() {
            generation.retire();
        }
    }
}

pub(super) struct Runtime {
    pub(super) machine: Recovery,
    pub(super) link: Arc<Link>,
    barrier: Option<Round>,
    install: Option<Round>,
    resumed: u64,
    started: Option<u64>,
    deadline: Option<Instant>,
}
impl Runtime {
    pub(super) fn new(
        session: Session,
        role: Role,
        peer_ms: u64,
        now: u64,
        link: Arc<Link>,
    ) -> Result<Self, &'static str> {
        // Before the first handover, neither endpoint has granted remote input.
        link.ownership(Some(0), Role::Dialer);
        let baseline = link.baseline(0.0, 0.0, 1).unwrap();
        let machine = Recovery::new(
            session,
            0,
            role,
            baseline.ownership,
            baseline.owner,
            peer_ms,
            now,
        )
        .map_err(|_| "invalid recovery identity")?;
        link.local.lock().unwrap().generation = Some(Generation::new(session, 0, None));
        link.generation().unwrap().update_peer_deadline(peer_ms);
        link.enabled.store(true, Ordering::Release);
        Ok(Self {
            machine,
            link,
            barrier: None,
            install: None,
            resumed: 0,
            started: None,
            deadline: None,
        })
    }
    pub(super) fn step(&mut self, now: u64, input: Input) -> Result<Outcome, &'static str> {
        if self.machine.send_epoch(now).is_some() {
            if let Some((ownership, owner)) = self.link.local.lock().unwrap().ownership {
                self.machine.set_ownership(ownership, owner);
            }
        }
        let outcome = self.machine.step(now, input);
        if let Some(reason) = outcome.closed() {
            self.link.close();
            log::warn!(
                "input recovery closed: session={:x?} epoch={} reason={reason:?}",
                self.machine.session(),
                self.machine.epoch()
            );
            return Err("input recovery failed");
        }
        if let Some(round) = outcome.barrier() {
            if self.barrier != Some(round) {
                self.link.close();
                if self.link.local.lock().unwrap().ownership.is_none() {
                    return Err("input recovery during unconfirmed handover");
                }
                self.barrier = Some(round);
                self.started = self.machine.recovery_started();
                self.deadline = Some(
                    Instant::now()
                        + Duration::from_millis(
                            super::recovery::RECOVERY_MS
                                .saturating_sub(now.saturating_sub(self.started.unwrap_or(now))),
                        ),
                );
                self.link.local.lock().unwrap().request = Some(Request::Barrier(round));
                log::info!(
                    "input recovery started: session={:x?} epoch={} deadline={}ms",
                    round.session(),
                    round.old_epoch(),
                    super::recovery::RECOVERY_MS
                );
            }
        }
        if let Some(round) = outcome.install() {
            if self.install != Some(round) {
                self.install = Some(round);
                let mut local = self.link.local.lock().unwrap();
                local.generation = Some(Generation::new(
                    round.session(),
                    round.next_epoch(),
                    self.deadline,
                ));
                local.request = Some(Request::Install {
                    round,
                    peer: outcome.peer_baseline().ok_or("missing peer baseline")?,
                });
            }
        }
        if outcome.state() == State::Normal && self.machine.epoch() > self.resumed {
            if !self
                .link
                .generation()
                .is_some_and(|generation| generation.commit())
            {
                self.link.fail();
                return Err("recovery generation expired before activation");
            }
            self.resumed = self.machine.epoch();
            self.link.frozen.store(false, Ordering::Release);
            self.link.local.lock().unwrap().request = Some(Request::Resumed {
                epoch: self.resumed,
                peer: outcome.peer_baseline().ok_or("missing resumed baseline")?,
            });
            log::info!(
                "input recovery succeeded: session={:x?} epoch={} elapsed={}ms",
                self.machine.session(),
                self.resumed,
                now.saturating_sub(self.started.take().unwrap_or(now))
            );
        }
        if let Some(generation) = self.link.generation() {
            // Queue and backend gates enforce liveness even when the driver
            // has not been scheduled to consume its next watchdog tick.
            generation.update_peer_deadline(self.machine.peer_remaining(now));
        }
        Ok(outcome)
    }
    pub(super) fn response(&mut self) -> Option<Input> {
        let response = self.link.local.lock().unwrap().response.take();
        response.map(|response| match response {
            InputResponse::Barrier(round, result) => Input::Barrier { round, result },
            InputResponse::Installed(round, result) => Input::Installed { round, result },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn r12_r14_prepared_generation_expires_without_driver_poll_and_cannot_revive() {
        let expired = Generation::new([1; 16], 1, Some(Instant::now() - Duration::from_millis(1)));
        assert!(!expired.valid());
        assert!(!expired.commit());
        let normal = Generation::new([1; 16], 0, None);
        assert!(normal.valid());
        normal.retire();
        assert!(!normal.commit());
        let different_session = Generation::new([2; 16], 0, None);
        assert!(different_session.valid());
        different_session.update_peer_deadline(0);
        assert!(!different_session.valid());
    }
}
