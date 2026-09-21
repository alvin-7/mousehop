use super::*;

fn baseline() -> Baseline {
    Baseline {
        x: 10.0,
        y: -20.0,
        layout: 3,
        ownership: [9; 16],
        owner: Role::Dialer,
    }
}
fn machine(role: Role) -> Recovery {
    Recovery::new([1; 16], 0, role, [9; 16], Role::Dialer, 3000, 0).unwrap()
}
fn control(kind: Kind, epoch: u64) -> Control {
    Control {
        session: [1; 16],
        epoch,
        source: kind.sender().unwrap_or(Role::Acceptor),
        kind,
        baseline: matches!(kind, Kind::Prepared | Kind::Commit).then(baseline),
    }
}

#[test]
fn r14_deadline_checked_on_callbacks_without_tick_and_not_extended() {
    let mut m = machine(Role::Dialer);
    let s = m.step(600, Input::Start);
    let round = s.barrier().unwrap();
    for now in [650, 900, 1500, 2599] {
        m.step(now, Input::PeerActivity);
        m.step(now, Input::Control(control(Kind::Request, 0)));
    }
    let s = m.step(
        2600,
        Input::Barrier {
            round,
            result: Ok(baseline()),
        },
    );
    assert_eq!(s.closed(), Some(CloseReason::RecoveryTimeout));
    assert_eq!(m.send_epoch(2600), None);
    assert_eq!(m.receive_epoch(2600), None);
}

#[test]
fn r12_barrier_and_install_ack_gate_both_directions() {
    let mut a = machine(Role::Dialer);
    let mut b = machine(Role::Acceptor);
    let ra = a.step(0, Input::Start).barrier().unwrap();
    let rb = b
        .step(0, Input::Control(control(Kind::Prepare, 0)))
        .barrier()
        .unwrap();
    assert_eq!(a.send_epoch(0), None);
    assert_eq!(b.receive_epoch(0), None);
    a.step(
        1,
        Input::Barrier {
            round: ra,
            result: Ok(baseline()),
        },
    );
    b.step(
        1,
        Input::Barrier {
            round: rb,
            result: Ok(baseline()),
        },
    );
    a.step(2, Input::Control(control(Kind::Prepared, 0)));
    let install = b
        .step(3, Input::Control(control(Kind::Commit, 0)))
        .install()
        .unwrap();
    assert_eq!(b.receive_epoch(3), None);
    b.step(
        4,
        Input::Installed {
            round: install,
            result: Ok(()),
        },
    );
    assert_eq!(b.receive_epoch(4), Some(1));
    assert_eq!(b.send_epoch(4), None);
    let install = a
        .step(5, Input::Control(control(Kind::CommitAck, 0)))
        .install()
        .unwrap();
    a.step(
        6,
        Input::Installed {
            round: install,
            result: Ok(()),
        },
    );
    assert_eq!(a.receive_epoch(6), Some(1));
    assert_eq!(a.send_epoch(6), None);
    b.step(7, Input::Control(control(Kind::Activate, 0)));
    assert_eq!(b.send_epoch(7), Some(1));
    a.step(8, Input::Control(control(Kind::ActivateAck, 0)));
    assert_eq!(a.send_epoch(8), Some(1));
    // Successful round's old deadline cannot close the new normal epoch.
    assert_eq!(a.step(2000, Input::Tick).closed(), None);
}

/// Deterministic lossy datagram pump; no sleeping, network, backend, or wall clock.
fn run_loss(drop_kind: Option<Kind>, simultaneous: bool, blackout_until: u64) {
    let mut a = machine(Role::Dialer);
    let mut b = machine(Role::Acceptor);
    let mut wire = std::collections::VecDeque::new();
    let mut dropped = false;
    for now in 0..1900 {
        for (is_a, m) in [(true, &mut a), (false, &mut b)] {
            let start = now == 0 && (simultaneous || !is_a);
            let mut s = m.step(now, if start { Input::Start } else { Input::Tick });
            if let Some(c) = s.control() {
                wire.push_back((is_a, c.clone()));
            }
            if let Some(round) = s.barrier() {
                s = m.step(
                    now,
                    Input::Barrier {
                        round,
                        result: Ok(baseline()),
                    },
                );
                if let Some(c) = s.control() {
                    wire.push_back((is_a, c.clone()));
                }
            }
            if let Some(round) = s.install() {
                s = m.step(
                    now,
                    Input::Installed {
                        round,
                        result: Ok(()),
                    },
                );
                if let Some(c) = s.control() {
                    wire.push_back((is_a, c.clone()));
                }
            }
            assert_eq!(s.closed(), None);
        }
        // Limit delivery per tick and duplicate messages to exercise old stages.
        if let Some((from_a, c)) = wire.pop_front() {
            if now < blackout_until {
                continue;
            }
            if !dropped && Some(c.kind) == drop_kind {
                dropped = true;
                continue;
            }
            let m = if from_a { &mut b } else { &mut a };
            for _ in 0..2 {
                let s = m.step(now, Input::Control(c.clone()));
                assert_eq!(s.closed(), None);
                if let Some(reply) = s.control() {
                    wire.push_back((!from_a, reply.clone()));
                }
            }
        }
        if a.send_epoch(now) == Some(1) && b.send_epoch(now) == Some(1) {
            assert_eq!(a.receive_epoch(now), Some(1));
            assert_eq!(b.receive_epoch(now), Some(1));
            assert!(drop_kind.is_none() || dropped);
            return;
        }
    }
    panic!("did not converge with drop {drop_kind:?}, simultaneous={simultaneous}");
}

#[test]
fn r12_r15_every_stage_first_packet_loss_duplicates_and_simultaneous_start() {
    run_loss(None, true, 0);
    for kind in [
        Kind::Request,
        Kind::Prepare,
        Kind::Prepared,
        Kind::Commit,
        Kind::CommitAck,
        Kind::Activate,
        Kind::ActivateAck,
    ] {
        run_loss(Some(kind), false, 0);
        run_loss(Some(kind), true, 0);
    }
}

#[test]
fn r11_r14_temporary_blackout_recovers_within_original_deadline() {
    for duration in [800, 1200, 1500] {
        run_loss(None, false, duration);
        run_loss(None, true, duration);
    }
}

#[test]
fn r14_peer_timeout_idle_and_bounded_retries() {
    let mut idle = machine(Role::Dialer);
    assert_eq!(idle.step(2999, Input::Tick).state(), State::Normal);
    assert_eq!(
        idle.step(3000, Input::PeerActivity).closed(),
        Some(CloseReason::PeerTimeout)
    );
    let mut m = machine(Role::Dialer);
    let mut count = 0;
    for now in 0..=2000 {
        let s = m.step(now, if now == 0 { Input::Start } else { Input::Tick });
        count += usize::from(s.control().is_some());
    }
    assert_eq!(count, 40);
    assert_eq!(
        m.step(2001, Input::Tick).closed(),
        Some(CloseReason::RecoveryTimeout)
    );
}

#[test]
fn r12_identity_future_epoch_overflow_and_failed_barrier_fail_closed() {
    let mut m = machine(Role::Dialer);
    let mut c = control(Kind::Request, 0);
    c.session = [2; 16];
    assert_eq!(
        m.step(0, Input::Control(c)).closed(),
        Some(CloseReason::Identity)
    );
    let mut m = machine(Role::Dialer);
    assert_eq!(
        m.step(0, Input::Control(control(Kind::Request, 1)))
            .closed(),
        Some(CloseReason::Protocol)
    );
    let mut m = Recovery::new(
        [1; 16],
        u64::MAX,
        Role::Dialer,
        [9; 16],
        Role::Dialer,
        3000,
        0,
    )
    .unwrap();
    assert_eq!(
        m.step(0, Input::Start).closed(),
        Some(CloseReason::EpochExhausted)
    );
    let mut m = machine(Role::Dialer);
    let round = m.step(0, Input::Start).barrier().unwrap();
    assert_eq!(
        m.step(
            1,
            Input::Barrier {
                round,
                result: Err(())
            }
        )
        .closed(),
        Some(CloseReason::BarrierFailed)
    );
}

#[test]
fn r12_duplicate_and_old_callback_cannot_change_completed_barrier() {
    let mut m = machine(Role::Dialer);
    let round = m.step(0, Input::Start).barrier().unwrap();
    let foreign = Round {
        session: [2; 16],
        epoch: 0,
    };
    assert!(
        m.step(
            1,
            Input::Barrier {
                round: foreign,
                result: Err(())
            }
        )
        .barrier()
        .is_some()
    );
    m.step(
        2,
        Input::Barrier {
            round,
            result: Ok(baseline()),
        },
    );
    assert_eq!(
        m.step(
            3,
            Input::Barrier {
                round,
                result: Err(())
            }
        )
        .closed(),
        None
    );
    m.step(4, Input::Control(control(Kind::Prepared, 0)));
    m.step(5, Input::Control(control(Kind::CommitAck, 0)));
    assert_eq!(
        m.step(
            6,
            Input::Installed {
                round,
                result: Err(())
            }
        )
        .closed(),
        Some(CloseReason::InstallFailed)
    );
    assert_eq!(m.receive_epoch(6), None);
}

#[test]
fn r12_final_ack_loss_old_round_reply_does_not_freeze_or_refresh_peer() {
    let mut b = machine(Role::Acceptor);
    for epoch in 0..2 {
        let now = epoch * 100;
        let round = b
            .step(now, Input::Control(control(Kind::Prepare, epoch)))
            .barrier()
            .unwrap();
        b.step(
            now + 1,
            Input::Barrier {
                round,
                result: Ok(baseline()),
            },
        );
        b.step(now + 2, Input::Control(control(Kind::Commit, epoch)));
        b.step(
            now + 3,
            Input::Installed {
                round,
                result: Ok(()),
            },
        );
        let s = b.step(now + 4, Input::Control(control(Kind::Activate, epoch)));
        assert_eq!(s.control().unwrap().kind, Kind::ActivateAck);
        // Final Ack lost: repeated Activate must get the same Ack, with no barrier.
        let s = b.step(now + 54, Input::Control(control(Kind::Activate, epoch)));
        assert_eq!(s.control().unwrap().epoch, epoch);
        assert_eq!(s.barrier(), None);
        assert_eq!(b.send_epoch(now + 54), Some(epoch + 1));
        assert_eq!(
            b.step(
                now + 55,
                Input::Installed {
                    round,
                    result: Err(())
                }
            )
            .closed(),
            None
        );
    }
    assert_eq!(
        b.step(200, Input::Control(control(Kind::Activate, 0)))
            .control(),
        None,
        "only one round cached"
    );
    let s = b.step(3104, Input::Control(control(Kind::Activate, 1)));
    assert_eq!(
        s.closed(),
        Some(CloseReason::PeerTimeout),
        "old controls cannot keep dead peer alive"
    );
}

#[test]
fn r14_peer_deadline_can_win_and_gates_expire_without_tick() {
    let mut m = Recovery::new([1; 16], 0, Role::Dialer, [9; 16], Role::Dialer, 700, 0).unwrap();
    m.step(600, Input::Start);
    assert_eq!(
        m.step(2600, Input::Tick).closed(),
        Some(CloseReason::PeerTimeout)
    );
    let m = machine(Role::Dialer);
    assert_eq!(m.send_epoch(3000), None);
    assert_eq!(m.receive_epoch(3000), None);
    assert_eq!(m.receive_epoch(2999), Some(0));
}

#[test]
fn r12_changed_ownership_or_baseline_fails_closed() {
    let mut m = machine(Role::Dialer);
    m.step(0, Input::Start);
    let mut c = control(Kind::Prepared, 0);
    c.baseline.as_mut().unwrap().ownership = [7; 16];
    assert_eq!(
        m.step(1, Input::Control(c)).closed(),
        Some(CloseReason::Ownership)
    );
    let mut m = machine(Role::Dialer);
    m.step(0, Input::Start);
    m.step(1, Input::Control(control(Kind::Prepared, 0)));
    let mut c = control(Kind::Prepared, 0);
    c.baseline.as_mut().unwrap().x += 1.0;
    assert_eq!(
        m.step(2, Input::Control(c)).closed(),
        Some(CloseReason::Ownership)
    );
}

#[test]
fn r12_reordered_stages_do_not_open_gates_and_delayed_success_cannot_beat_deadline() {
    let mut b = machine(Role::Acceptor);
    let round = b.step(0, Input::Start).barrier().unwrap();
    for kind in [Kind::Activate, Kind::Commit, Kind::Prepare] {
        b.step(1, Input::Control(control(kind, 0)));
        assert_eq!(b.receive_epoch(1), None);
        assert_eq!(b.send_epoch(1), None);
    }
    b.step(
        2,
        Input::Barrier {
            round,
            result: Ok(baseline()),
        },
    );
    b.step(3, Input::Control(control(Kind::Commit, 0)));
    b.step(
        4,
        Input::Installed {
            round,
            result: Ok(()),
        },
    );
    assert_eq!(b.receive_epoch(1999), Some(1));
    assert_eq!(b.receive_epoch(2000), None);
    assert_eq!(
        b.step(2000, Input::Control(control(Kind::Activate, 0)))
            .closed(),
        Some(CloseReason::RecoveryTimeout)
    );
}

#[test]
fn r12_r13_old_data_session_and_receipts_never_pass_new_receive_gate() {
    let mut m = machine(Role::Acceptor);
    let data = |session, epoch, source| Envelope::Reliable {
        session,
        epoch,
        source,
        frame: mousehop_proto::transport::Frame::Progress {
            id: 1,
            sent: 5,
            processed: 4,
        },
    };
    let old = data([1; 16], 0, Role::Dialer);
    assert!(m.accepts_data(0, &old));
    let round = m
        .step(1, Input::Control(control(Kind::Prepare, 0)))
        .barrier()
        .unwrap();
    assert!(!m.accepts_data(1, &old));
    m.step(
        2,
        Input::Barrier {
            round,
            result: Ok(baseline()),
        },
    );
    m.step(3, Input::Control(control(Kind::Commit, 0)));
    m.step(
        4,
        Input::Installed {
            round,
            result: Ok(()),
        },
    );
    assert!(!m.accepts_data(4, &old));
    assert!(!m.accepts_data(4, &data([2; 16], 1, Role::Dialer)));
    assert!(!m.accepts_data(4, &data([1; 16], 1, Role::Acceptor)));
    assert!(m.accepts_data(4, &data([1; 16], 1, Role::Dialer)));
    assert!(!m.accepts_data(2001, &data([1; 16], 1, Role::Dialer)));
    assert!(!m.accepts_data(4, &Envelope::Control(control(Kind::Activate, 0))));
}

#[test]
fn r14_duplicate_request_flood_never_resets_retry_or_deadline() {
    let mut m = machine(Role::Dialer);
    let mut count = usize::from(m.step(600, Input::Start).control().is_some());
    for now in 601..2600 {
        count += usize::from(
            m.step(now, Input::Control(control(Kind::Request, 0)))
                .control()
                .is_some(),
        );
        assert_eq!(m.recovery_started(), Some(600));
    }
    assert_eq!(count, 40);
    assert_eq!(
        m.step(2600, Input::Start).closed(),
        Some(CloseReason::RecoveryTimeout)
    );
    assert_eq!(
        m.step(2601, Input::PeerActivity).closed(),
        Some(CloseReason::RecoveryTimeout)
    );
}
