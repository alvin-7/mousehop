//! Local recovery gate tied to the negotiated transport generation.
use super::*;
use input_emulation::{EmulationError, RecoveryBaseline};

pub(super) type Completion = oneshot::Sender<Result<RecoveryBaseline, EmulationError>>;

#[derive(Clone)]
pub(crate) struct RecoveryToken {
    pub(super) addr: SocketAddr,
    state: Rc<Cell<Gate>>,
    pub(crate) session: [u8; 16],
    pub(crate) epoch: u64,
    generation: Rc<RefCell<Option<crate::transport::lifecycle::Generation>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    Frozen,
    Prepared,
    Active,
    Retired,
}

impl RecoveryToken {
    pub(super) fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.state, &other.state)
    }
    pub(super) fn retired(&self) -> bool {
        self.state.get() == Gate::Retired
    }
    pub(super) fn new(addr: SocketAddr, session: [u8; 16], epoch: u64) -> Self {
        Self {
            addr,
            session,
            epoch,
            state: Rc::new(Cell::new(Gate::Frozen)),
            generation: Rc::new(RefCell::new(None)),
        }
    }
    pub(super) fn retire(&self) {
        self.state.set(Gate::Retired);
    }
    pub(super) fn frozen(&self) -> bool {
        self.state.get() == Gate::Frozen
    }
    pub(super) fn prepare(&self) {
        if self.frozen() {
            self.state.set(Gate::Prepared);
        }
    }
    /// The driver may activate only after the peer handshake. A retired token
    /// can never be reopened, including after reconnect with the same IDs.
    pub(crate) fn activate(&self) -> bool {
        if self.state.get() != Gate::Prepared {
            return false;
        }
        self.state.set(Gate::Active);
        true
    }
    pub(crate) fn valid(&self) -> bool {
        self.state.get() == Gate::Active
            && self.generation.borrow().as_ref().is_none_or(|g| g.valid())
    }
    pub(super) fn bind(&self, generation: crate::transport::lifecycle::Generation) -> bool {
        if generation.session != self.session
            || generation.epoch != self.epoch
            || !generation.valid()
        {
            return false;
        }
        *self.generation.borrow_mut() = Some(generation);
        true
    }
}

pub(super) struct RecoveryRequest {
    pub(super) token: RecoveryToken,
    pub(super) done: Completion,
}

impl RecoveryRequest {
    pub(super) async fn execute(self, emulation: &mut InputEmulation, handle: EmulationHandle) {
        if !self.token.frozen() {
            let _ = self.done.send(Err(EmulationError::EndOfStream));
            return;
        }
        // Deliberately await to completion: dropping a future cannot cancel
        // an OS operation already running. No select/timeout around this call.
        let result = emulation.recovery_barrier(handle).await;
        if !self.token.frozen() {
            let _ = self.done.send(Err(EmulationError::EndOfStream));
            return;
        }
        if result.is_ok() {
            self.token.prepare();
        }
        let _ = self.done.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_r1_2_proxy_drops_old_queue_preserves_other_handle_and_reports_unsupported() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut proxy = EmulationProxy::new(Some(input_emulation::Backend::Dummy));
                assert!(matches!(
                    proxy.event().await,
                    EmulationEvent::EmulationEnabled
                ));
                let addr1 = "127.0.0.1:4242".parse().unwrap();
                let addr2 = "127.0.0.1:4243".parse().unwrap();
                let motion = Event::Pointer(input_event::PointerEvent::Motion {
                    time: 0,
                    dx: 1.0,
                    dy: 0.0,
                });
                for addr in [addr1, addr2] {
                    proxy.consume(motion.clone(), addr);
                }
                let (done, pending) = oneshot::channel();
                proxy.request_tx.send(ProxyRequest::Barrier(done)).unwrap();
                pending.await.unwrap();
                let old = RecoveryToken::new(addr1, [1; 16], 0);
                old.prepare();
                old.activate();
                let other = RecoveryToken::new(addr2, [2; 16], 0);
                other.prepare();
                other.activate();
                proxy
                    .recovery_tokens
                    .borrow_mut()
                    .insert(addr1, old.clone());
                proxy
                    .recovery_tokens
                    .borrow_mut()
                    .insert(addr2, other.clone());
                let queued_old = proxy.consume_epoch(motion.clone(), &old, None);
                let queued_other = proxy.consume_epoch(motion, &other, None);
                let (next, barrier) = proxy.recovery_barrier(addr1, [1; 16], 1).unwrap();
                assert!(!old.valid());
                assert!(matches!(
                    barrier.await.unwrap(),
                    Err(EmulationError::RecoveryUnsupported)
                ));
                assert!(!next.activate());
                assert!(queued_old.await.unwrap().is_err());
                assert!(queued_other.await.unwrap().is_ok());
                assert!(other.valid());
                proxy.terminate().await;
            })
            .await;
    }
    #[test]
    fn recovery_r1_2_old_queue_and_reconnect_tokens_never_reopen() {
        let addr = "127.0.0.1:4242".parse().unwrap();
        let old = RecoveryToken::new(addr, [1; 16], 0);
        assert!(!old.activate());
        old.prepare();
        assert!(old.activate());
        let queued = old.clone();
        old.retire();
        assert!(!queued.valid());
        old.prepare();
        assert!(!old.activate());
        let replacement = RecoveryToken::new(addr, [1; 16], 0);
        replacement.prepare();
        assert!(replacement.activate());
        assert!(!queued.valid());
    }
}
