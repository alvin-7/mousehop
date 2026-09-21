use super::*;
#[cfg(windows)]
#[tokio::test]
#[ignore = "reads the real Windows desktop cursor and display geometry; injects no input"]
async fn r12_windows_real_empty_handle_barrier_reads_actual_baseline() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut emulation = InputEmulation::new(Some(Backend::Windows)).await.unwrap();
            assert!(emulation.create(9876).await);
            let baseline = emulation.recovery_barrier(9876).await.unwrap();
            assert!(
                baseline
                    .layout
                    .rectangles()
                    .any(|(_, rect)| rect.contains(baseline.cursor))
            );
            assert!(!emulation.has_pressed_keys(9876));
            emulation.terminate().await;
        })
        .await;
}
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct State {
    events: Vec<(EmulationHandle, Event)>,
    stopped: Vec<EmulationHandle>,
    fail_release: bool,
    fail_stop: bool,
}

struct BackendMock {
    state: Arc<Mutex<State>>,
    blocked: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}

#[async_trait]
impl Emulation for BackendMock {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some((started, finished)) = self.blocked.take() {
            let _ = started.send(());
            let _ = finished.await;
        }
        let mut state = self.state.lock().unwrap();
        state.events.push((handle, event.clone()));
        if state.fail_release
            && matches!(
                event,
                Event::Keyboard(KeyboardEvent::Key { state: 0, .. })
                    | Event::Pointer(PointerEvent::Button { state: 0, .. })
            )
        {
            return Err(EmulationError::Io(std::io::Error::other(
                "injected release failure",
            )));
        }
        Ok(())
    }
    async fn quiesce(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        let mut state = self.state.lock().unwrap();
        state.stopped.push(handle);
        if state.fail_stop {
            return Err(EmulationError::BackgroundTask("injected".into()));
        }
        Ok(())
    }
    fn recovery_baseline(&mut self) -> Result<RecoveryBaseline, EmulationError> {
        Ok(RecoveryBaseline {
            cursor: (-10, 25),
            layout: DisplayLayout::new([(-100, 0, 200, 100)]),
        })
    }
    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}
    async fn terminate(&mut self) {}
}

fn emulation(state: Arc<Mutex<State>>) -> InputEmulation {
    InputEmulation {
        emulation: Box::new(BackendMock {
            state,
            blocked: None,
        }),
        handles: HashSet::new(),
        pressed_keys: HashMap::new(),
        pressed_buttons: HashMap::new(),
        post_processing: HashMap::new(),
        clipboard: None,
    }
}
fn key(key: u32, state: u8) -> Event {
    Event::Keyboard(KeyboardEvent::Key {
        time: 0,
        key,
        state,
    })
}

#[tokio::test]
async fn recovery_r1_3_release_failure_retains_keys_and_attempts_all_releases() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut em = emulation(state.clone());
    em.create(1).await;
    em.create(2).await;
    em.set_post_processing(
        1,
        ReceivePostProcessing {
            natural_scroll: true,
            ..Default::default()
        },
    );
    for k in [29, 30] {
        em.consume(key(k, 1), 1).await.unwrap();
    }
    em.consume(key(31, 1), 2).await.unwrap();
    em.consume(
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button: input_event::BTN_LEFT,
            state: 1,
        }),
        1,
    )
    .await
    .unwrap();
    state.lock().unwrap().fail_release = true;
    assert!(em.recovery_barrier(1).await.is_err());
    assert_eq!(em.pressed_keys[&1].len(), 2);
    assert_eq!(em.pressed_buttons[&1].len(), 1);
    assert!(em.has_pressed_keys(2));
    assert_eq!(
        state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|(_, e)| matches!(
                e,
                Event::Keyboard(KeyboardEvent::Key { state: 0, .. })
                    | Event::Pointer(PointerEvent::Button { state: 0, .. })
            ))
            .count(),
        3
    );
    state.lock().unwrap().fail_release = false;
    let baseline = em.recovery_barrier(1).await.unwrap();
    assert_eq!(baseline.cursor, (-10, 25));
    assert!(!em.has_pressed_keys(1));
    assert!(em.pressed_buttons[&1].is_empty());
    assert!(em.has_pressed_keys(2));
    assert!(em.post_processing[&1].natural_scroll);
    assert_eq!(state.lock().unwrap().stopped, [1, 1]);
}

#[tokio::test]
async fn recovery_r1_2_failed_background_stop_cannot_certify_success() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut em = emulation(state.clone());
    em.create(1).await;
    em.consume(key(29, 1), 1).await.unwrap();
    state.lock().unwrap().fail_stop = true;
    assert!(matches!(
        em.recovery_barrier(1).await,
        Err(EmulationError::BackgroundTask(_))
    ));
    assert!(!em.has_pressed_keys(1)); // cleanup attempted despite stop failure
}

#[tokio::test]
async fn recovery_r1_2_serial_barrier_waits_for_started_backend_call() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut em = emulation(state.clone());
    em.create(1).await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    em.emulation = Box::new(BackendMock {
        state: state.clone(),
        blocked: Some((started_tx, finish_rx)),
    });
    let completed = Arc::new(AtomicBool::new(false));
    let flag = completed.clone();
    let worker = tokio::spawn(async move {
        em.consume(key(30, 1), 1).await.unwrap();
        let baseline = em.recovery_barrier(1).await.unwrap();
        flag.store(true, Ordering::SeqCst);
        baseline
    });
    started_rx.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!completed.load(Ordering::SeqCst));
    assert!(state.lock().unwrap().stopped.is_empty());
    finish_tx.send(()).unwrap();
    worker.await.unwrap();
    assert!(completed.load(Ordering::SeqCst));
    assert_eq!(state.lock().unwrap().stopped, [1]);
}

#[tokio::test]
async fn recovery_r1_2_unsupported_backend_does_not_report_success() {
    let mut em = emulation(Arc::new(Mutex::new(State::default())));
    em.emulation = Box::new(crate::dummy::DummyEmulation::new());
    em.create(1).await;
    assert!(matches!(
        em.recovery_barrier(1).await,
        Err(EmulationError::RecoveryUnsupported)
    ));
}
