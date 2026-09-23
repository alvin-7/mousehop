use super::core::*;
use mousehop_proto::{MAX_EVENT_SIZE, ProtoEvent};

fn event(n: u32) -> Vec<u8> {
    use input_event::{Event, KeyboardEvent, PointerEvent};
    let e = match n % 8 {
        0 => ProtoEvent::HandoverLeave {
            serial: n + 1,
            mode: 0,
        },
        1 => ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: n,
            key: 29,
            state: 1,
        })),
        2 => ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: n,
            key: 30,
            state: 1,
        })),
        3 => ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: n,
            key: 30,
            state: 0,
        })),
        4 => ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
            time: n,
            button: 272,
            state: 1,
        })),
        5 => ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: n,
            dx: 3.0,
            dy: -2.0,
        })),
        6 => ProtoEvent::Input(Event::Pointer(PointerEvent::Button {
            time: n,
            button: 272,
            state: 0,
        })),
        _ => ProtoEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: 120,
        })),
    };
    let (b, len): ([u8; MAX_EVENT_SIZE], usize) = e.into();
    b[..len].to_vec()
}

fn pair() -> (Session, Session) {
    let mut a = Session::new(true, 31, 7, 0);
    let mut b = Session::new(false, 0, 0, 0);
    a.hello(true, 0).unwrap();
    b.hello(true, 0).unwrap();
    exchange(&mut a, &mut b, 0, false);
    exchange(&mut a, &mut b, 10, false);
    (a, b)
}

#[test]
fn idle_network_pause_recovers_without_reconnecting() {
    for pause in [350, 700, 1200] {
        let (mut a, mut b) = pair();
        for now in (20..=pause).step_by(10) {
            a.tick(now).unwrap();
            b.tick(now).unwrap();
            // Drop every datagram during this idle network pause.
            a.drain();
            b.drain();
        }
        exchange(&mut a, &mut b, pause + 20, false);
        a.send(&event(1), pause + 30).unwrap();
        exchange(&mut a, &mut b, pause + 40, false);
        let (sequence, bytes) = b.receive().expect("same session resumes input");
        assert_eq!(bytes, event(1));
        b.processed(sequence, pause + 40).unwrap();
        exchange(&mut a, &mut b, pause + 50, false);
        assert!(a.ready() && b.ready());
    }
}

#[test]
fn total_silence_after_consumed_key_down_still_closes_both_peers() {
    let (mut a, mut b) = pair();
    a.send(&event(1), 20).unwrap();
    exchange(&mut a, &mut b, 30, false);
    let (sequence, _) = b.receive().unwrap();
    b.processed(sequence, 30).unwrap();
    a.tick(50).unwrap();
    b.tick(50).unwrap();
    exchange(&mut a, &mut b, 50, false);
    // Renew both liveness clocks at a common instant, without changing receipts.
    a.peer_activity(50);
    b.peer_activity(50);
    for now in (60..=1550).step_by(10) {
        a.tick(now).unwrap();
        b.tick(now).unwrap();
        a.drain();
        b.drain();
    }
    for local in [&mut a, &mut b] {
        assert_eq!(local.tick(1551), Err("peer activity timed out"));
        assert!(!local.ready());
    }
}

#[test]
fn authenticated_heartbeats_do_not_extend_reliable_input_guards() {
    for guard in 0..3 {
        let (mut a, mut b) = pair();
        let local = if guard == 0 { &mut a } else { &mut b };
        match guard {
            0 => local.send(&event(1), 20).unwrap(),
            1 => {
                local.input(&segment(81, 1, 1), 20).unwrap();
                local.receive().unwrap();
            }
            _ => progress(local, 1, 20),
        }
        local.drain();
        for now in [100, 200, 300, 320] {
            local.peer_activity(now);
            local.tick(now).unwrap();
            local.drain();
        }
        local.peer_activity(321);
        assert_eq!(local.tick(321), Err("reliable input stalled"));
    }
}

#[test]
fn disconnect_r7_d2_udp_activity_never_renews_reliable_guards() {
    for guard in 0..3 {
        let (mut a, mut b) = pair();
        let local = if guard == 0 { &mut a } else { &mut b };
        match guard {
            0 => local.send(&event(1), 20).unwrap(),
            1 => {
                local.input(&segment(81, 1, 1), 20).unwrap();
                local.receive().unwrap();
            }
            _ => progress(local, 1, 20),
        }
        local.drain();
        for now in [100, 200, 300, 320] {
            local.fresh_motion(0, now);
            assert_eq!(local.tick(now), Ok(()));
            local.drain();
        }
        local.fresh_motion(0, 321);
        assert_eq!(
            local.tick(321),
            Err("reliable input stalled"),
            "guard={guard}"
        );
    }
}

#[test]
fn disconnect_r7_d2_future_motion_starts_but_never_renews_missing_input_deadline() {
    let (_, mut b) = pair();
    b.fresh_motion(1, 20);
    for now in [100, 200, 320] {
        b.fresh_motion(2, now);
        assert_eq!(b.tick(now), Ok(()));
        b.drain();
    }
    b.fresh_motion(3, 321);
    assert_eq!(b.tick(321), Err("reliable input stalled"));
}

#[test]
fn disconnect_r7_d1_output_wait_age_survives_partial_drain() {
    let (mut a, _) = pair();
    a.send(&event(1), 20).unwrap();
    a.send(&event(2), 30).unwrap();
    assert_eq!(a.pop_output().unwrap().0, 20);
    // Taking the first packet must not reset the next packet's original age.
    assert!(!a.output_expired(330));
    assert!(a.output_expired(331));
    assert_eq!(a.pop_output().unwrap().0, 30);
    assert!(!a.has_output());
}

#[test]
fn continuous_receive_progress_does_not_stall() {
    use mousehop_proto::transport::Frame;
    let (_, mut b) = pair();
    let mut received = 0;
    for n in 1u32..=40 {
        let now = u64::from(n) * 50;
        b.input(
            &Frame::Progress {
                id: 31,
                sent: u64::from(n + 1),
                processed: 0,
            }
            .encode(),
            now,
        )
        .unwrap();
        // Valid KCP PUSH segments after the bootstrap (SN 0). Model a
        // progress frame overtaking data by one 50ms batch, without loss.
        let mut message = u64::from(n).to_le_bytes().to_vec();
        message.extend(event(n));
        let mut payload = 7u32.to_le_bytes().to_vec();
        payload.extend([81, 0]);
        payload.extend(128u16.to_le_bytes());
        payload.extend((now as u32 - 50).to_le_bytes());
        payload.extend(n.to_le_bytes());
        payload.extend(0u32.to_le_bytes());
        payload.extend((message.len() as u32).to_le_bytes());
        payload.extend(message);
        b.input(&Frame::Data { id: 31, payload }.encode(), now)
            .unwrap();
        while let Some((seq, bytes)) = b.receive() {
            assert_eq!((seq, bytes), (u64::from(n), event(n)));
            received += 1;
            b.processed(seq, now).unwrap();
        }
        assert_eq!(received, n);
        assert_eq!(b.tick(now), Ok(()), "continuous progress at {now}ms");
        b.drain();
    }
    assert!(b.ready());
    // Deliver the final advertised input and check that the gap was cleared.
    b.input(&segment(81, 41, 41), 2050).unwrap();
    let (seq, bytes) = b.receive().unwrap();
    assert_eq!((seq, bytes), (41, event(41)));
    b.processed(seq, 2050).unwrap();
    progress(&mut b, 41, 2400);
    assert_eq!(b.tick(2400), Ok(()));
    assert!(b.receive().is_none());
}

fn exchange(a: &mut Session, b: &mut Session, now: u64, duplicate: bool) {
    for bytes in a.drain() {
        b.input(&bytes, now).unwrap();
        if duplicate {
            b.input(&bytes, now).unwrap();
        }
    }
    for bytes in b.drain() {
        a.input(&bytes, now).unwrap();
        if duplicate {
            a.input(&bytes, now).unwrap();
        }
    }
}

fn progress(b: &mut Session, sent: u64, now: u64) {
    b.input(
        &mousehop_proto::transport::Frame::Progress {
            id: 31,
            sent,
            processed: 0,
        }
        .encode(),
        now,
    )
    .unwrap();
}

// A single well-formed KCP segment. SN is the KCP sequence, sequence is the
// application watermark; the acceptor has already consumed bootstrap SN 0.
fn segment(command: u8, sn: u32, sequence: u64) -> Vec<u8> {
    let mut message = sequence.to_le_bytes().to_vec();
    if sequence != 0 {
        message.extend(event(sequence as u32));
    }
    if command == 82 {
        message.clear();
    }
    let mut payload = 7u32.to_le_bytes().to_vec();
    payload.extend([command, 0]);
    payload.extend(128u16.to_le_bytes());
    payload.extend(0u32.to_le_bytes());
    payload.extend(sn.to_le_bytes());
    payload.extend(0u32.to_le_bytes());
    payload.extend((message.len() as u32).to_le_bytes());
    payload.extend(message);
    mousehop_proto::transport::Frame::Data { id: 31, payload }.encode()
}

#[test]
fn configured_timeout_controls_each_session_guard_in_both_roles() {
    use mousehop_proto::transport::Frame;
    for milliseconds in [100, 300, 1000] {
        for dialer in [false, true] {
            for guard in 0..4 {
                let timeout = super::Timeouts {
                    stall: super::StallTimeout::try_from(milliseconds).unwrap(),
                    peer: super::PeerTimeout::try_from(milliseconds * 2).unwrap(),
                };
                let mut a = Session::with_timeout(true, 31, 7, 0, timeout);
                let mut b = Session::with_timeout(false, 0, 0, 0, timeout);
                a.hello(true, 0).unwrap();
                b.hello(true, 0).unwrap();
                exchange(&mut a, &mut b, 0, false);
                exchange(&mut a, &mut b, 10, false);
                let local = if dialer { &mut a } else { &mut b };
                let sent = u64::from(guard == 1 || guard == 3);
                local
                    .input(
                        &Frame::Progress {
                            id: 31,
                            sent,
                            processed: 0,
                        }
                        .encode(),
                        20,
                    )
                    .unwrap();
                if guard == 2 {
                    local.send(&event(1), 20).unwrap();
                }
                if guard == 3 {
                    local
                        .input(&segment(81, u32::from(!dialer), 1), 20)
                        .unwrap();
                    assert!(local.receive().is_some()); // deliberately never consumed
                }
                let limit = if guard == 0 {
                    milliseconds * 2
                } else {
                    milliseconds
                };
                for now in [20 + limit, 21 + limit] {
                    if guard != 0 {
                        local
                            .input(
                                &Frame::Progress {
                                    id: 31,
                                    sent,
                                    processed: 0,
                                }
                                .encode(),
                                now,
                            )
                            .unwrap();
                    }
                    assert_eq!(
                        local.tick(now),
                        if now == 20 + limit {
                            Ok(())
                        } else if guard == 0 {
                            Err("peer activity timed out")
                        } else {
                            Err("reliable input stalled")
                        },
                        "timeout={milliseconds}, dialer={dialer}, guard={guard}"
                    );
                    local.drain();
                }
            }
        }
    }
}

#[test]
fn gap_restarts_only_on_contiguous_input_and_clears_when_caught_up() {
    for caught_up in [false, true] {
        let (_, mut b) = pair();
        progress(&mut b, 2, 50);
        let first = vec![segment(81, 1, 1)];
        for bytes in &first {
            b.input(bytes, 200).unwrap();
        }
        let (seq, _) = b.receive().unwrap();
        b.processed(seq, 200).unwrap();
        if caught_up {
            b.input(&segment(81, 2, 2), 210).unwrap();
            let (seq, _) = b.receive().unwrap();
            b.processed(seq, 210).unwrap();
        }
        // Duplicate data and repeated/increasing Progress keep the peer alive,
        // but may not extend the missing input's deadline.
        for now in [300, 400, 500, 501] {
            progress(&mut b, if caught_up { 2 } else { 3 }, now);
            for bytes in &first {
                b.input(bytes, now).unwrap();
            }
            if now == 501 && !caught_up {
                assert_eq!(b.tick(now), Err("reliable input stalled"));
            } else {
                assert_eq!(b.tick(now), Ok(()));
            }
            b.drain();
        }
        if caught_up {
            progress(&mut b, 3, 600);
            assert_eq!(b.tick(900), Ok(()));
            progress(&mut b, 3, 901);
            assert_eq!(b.tick(901), Err("reliable input stalled"));
        }
    }
}

#[test]
fn bootstrap_ack_only_and_out_of_order_data_do_not_extend_gap() {
    for kind in 0..3 {
        let (_, mut b) = pair();
        progress(&mut b, 2, 50);
        let packet = match kind {
            0 => segment(81, 0, 0), // replayed bootstrap
            1 => segment(82, 0, 0), // ACK only
            _ => segment(81, 2, 2), // SN 1 missing: SN 2 cannot advance receive
        };
        for now in [150, 250, 350] {
            progress(&mut b, 2, now);
            b.input(&packet, now).unwrap();
            assert_eq!(b.tick(now), Ok(()));
            b.drain();
        }
        progress(&mut b, 2, 351);
        assert_eq!(b.tick(351), Err("reliable input stalled"), "kind={kind}");
    }
}

#[test]
fn negotiation_never_buffers_input_or_accepts_unsupported_peer() {
    let mut a = Session::new(true, 1, 1, 0);
    assert!(a.send(&event(1), 0).is_err());
    assert!(a.hello(false, 0).is_err());
}

#[test]
fn lost_ready_is_retried_without_resetting_sequence() {
    let mut a = Session::new(true, 31, 7, 0);
    let mut b = Session::new(false, 0, 0, 0);
    a.hello(true, 0).unwrap();
    b.hello(true, 0).unwrap();
    for p in a.drain() {
        b.input(&p, 0).unwrap();
    }
    b.drain(); // lose Ready
    assert!(!a.ready());
    assert!(!b.ready());
    a.tick(100).unwrap();
    exchange(&mut a, &mut b, 100, true);
    assert!(a.ready());
    a.send(&event(9), 101).unwrap();
    for t in (110..250).step_by(10) {
        a.tick(t).unwrap();
        b.tick(t).unwrap();
        exchange(&mut a, &mut b, t, true);
    }
    let delivered = b.receive().unwrap();
    assert_eq!(delivered.1, event(9));
    assert!(b.receive().is_none());
}

#[test]
fn loss_reorder_duplicate_deliver_once_in_order() {
    let (mut a, mut b) = pair();
    for n in 1..=30 {
        a.send(&event(n), 20).unwrap();
    }
    a.drain(); // lose first send including key-up equivalent
    let mut got = Vec::new();
    for t in (30..300).step_by(10) {
        a.tick(t).unwrap();
        b.tick(t).unwrap();
        let mut packets = a.drain();
        packets.reverse();
        for p in packets {
            b.input(&p, t).unwrap();
            b.input(&p, t).unwrap();
        }
        while let Some((seq, bytes)) = b.receive() {
            got.push(bytes);
            b.processed(seq, t).unwrap();
        }
        exchange(&mut a, &mut b, t, true);
    }
    assert_eq!(got, (1..=30).map(event).collect::<Vec<_>>());
}

#[test]
fn transport_ack_is_not_consumption_and_stall_is_terminal() {
    let (mut a, mut b) = pair();
    a.send(&event(1), 20).unwrap();
    for t in (20..300).step_by(10) {
        a.tick(t).unwrap();
        b.tick(t).unwrap();
        exchange(&mut a, &mut b, t, false);
    }
    assert!(a.tick(321).is_err());
    assert!(a.send(&event(2), 322).is_err());
    assert!(b.tick(601).is_err());
}

#[test]
fn consumption_progress_keeps_a_delayed_scroll_queue_alive_but_duplicates_do_not() {
    use mousehop_proto::transport::Frame;
    for reverse in [false, true] {
        let (a, b) = pair();
        let mut sender = if reverse { b } else { a };
        for n in 0..20 {
            sender.send(&event(n), 20).unwrap();
        }
        sender.drain();
        for n in 1..=7 {
            let now = n * 100;
            sender
                .input(
                    &Frame::Progress {
                        id: 31,
                        sent: 0,
                        processed: n,
                    }
                    .encode(),
                    now,
                )
                .unwrap();
            assert!(sender.tick(now).is_ok(), "consumption advanced at {now}ms");
            sender.drain();
        }
        // A live transport is insufficient: no new application consumption
        // for 301ms must still terminate the stalled session.
        for now in [800, 900, 1000] {
            sender
                .input(
                    &Frame::Progress {
                        id: 31,
                        sent: 0,
                        processed: 7,
                    }
                    .encode(),
                    now,
                )
                .unwrap();
            sender.tick(now).unwrap();
            sender.drain();
        }
        assert_eq!(sender.tick(1001), Err("reliable input stalled"));
    }
}

#[test]
fn idle_progress_does_not_expire_and_buffers_are_bounded() {
    let (mut a, mut b) = pair();
    for t in (20..2000).step_by(10) {
        a.tick(t).unwrap();
        b.tick(t).unwrap();
        exchange(&mut a, &mut b, t, false);
    }
    let mut accepted = 0;
    for n in 0..10000 {
        if a.send(&event(n), 2000).is_err() {
            break;
        }
        accepted += 1;
    }
    assert!(accepted < 256);
    assert!(!a.ready());
}

#[test]
fn old_session_and_malformed_frames_cannot_enter_replacement() {
    let (mut a, mut b) = pair();
    a.send(&event(1), 20).unwrap();
    a.tick(30).unwrap();
    let old = a.drain();
    let mut replacement = Session::new(false, 0, 0, 30);
    replacement.hello(true, 30).unwrap();
    for bytes in old {
        replacement.input(&bytes, 30).unwrap();
    }
    assert!(replacement.receive().is_none());
    assert!(b.input(&[240, 2, 0], 30).is_err());
    assert!(!b.ready());
}

#[test]
fn lost_input_with_only_progress_alive_still_closes_receiver() {
    use mousehop_proto::transport::Frame;
    let (mut a, mut b) = pair();
    a.send(&event(3), 20).unwrap();
    for t in (20..300).step_by(10) {
        a.tick(t).unwrap();
        b.tick(t).unwrap();
        for packet in a.drain() {
            if matches!(Frame::decode(&packet), Some(Frame::Progress { .. })) {
                b.input(&packet, t).unwrap();
            }
        }
        for packet in b.drain() {
            a.input(&packet, t).unwrap();
        }
    }
    assert!(a.tick(321).is_err());
    assert!(b.tick(401).is_err());
}

#[test]
fn seeded_loss_matrix_reports_input_latency_and_legacy_loss() {
    for threshold in [200, 300, 500] {
        for loss in [1u32, 5, 10] {
            let (mut a, mut b) = pair();
            a.set_stall_ms(threshold);
            b.set_stall_ms(threshold);
            let mut seed = 0xC0FFEEu32;
            let mut pending = Vec::<(u64, bool, Vec<u8>)>::new();
            let mut sent_times = Vec::new();
            let mut got = Vec::new();
            let mut latencies = Vec::new();
            let mut aborted = false;
            for t in (20..3020).step_by(10) {
                if a.tick(t).is_err() || b.tick(t).is_err() {
                    assert!(a.send(&event(1), t).is_err() || b.send(&event(1), t).is_err());
                    assert!(a.tick(t + threshold + 1).is_err());
                    assert!(b.tick(t + threshold + 1).is_err());
                    aborted = true;
                    break;
                }
                if t <= 2000 && (t - 20) % 20 == 0 {
                    let n = sent_times.len() as u32 + 1;
                    a.send(&event(n), t).unwrap();
                    sent_times.push(t);
                }
                for (direction, packets) in [(true, a.drain()), (false, b.drain())] {
                    for packet in packets {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        if seed % 100 < loss {
                            continue;
                        }
                        let due = t + 10 + ((seed >> 8) % 3) as u64 * 10;
                        pending.push((due, direction, packet.clone()));
                        if seed.is_multiple_of(7) {
                            pending.push((due + 10, direction, packet));
                        }
                    }
                }
                pending.reverse();
                let mut future = Vec::new();
                for (due, direction, bytes) in pending.drain(..) {
                    if due > t {
                        future.push((due, direction, bytes));
                        continue;
                    }
                    if direction {
                        b.input(&bytes, t).unwrap();
                    } else {
                        a.input(&bytes, t).unwrap();
                    }
                }
                pending = future;
                while let Some((seq, bytes)) = b.receive() {
                    assert_eq!(seq as usize, got.len() + 1);
                    latencies.push(t - sent_times[got.len()]);
                    got.push(bytes);
                    b.processed(seq, t).unwrap();
                }
            }
            assert_eq!(got, (1..=got.len() as u32).map(event).collect::<Vec<_>>());
            if !aborted {
                assert_eq!(got.len(), sent_times.len());
            }
            // Legacy comparison: same delay/loss/duplication model, independent wire trace
            // because KCP emits retransmissions, ACKs and progress packets.
            let mut legacy_seed = 0xC0FFEEu32;
            let mut legacy_lost = 0;
            let mut legacy_duplicates = 0;
            let mut legacy_latency = Vec::new();
            let mut lost_release = 0;
            for n in 1..=100 {
                legacy_seed = legacy_seed.wrapping_mul(1664525).wrapping_add(1013904223);
                if legacy_seed % 100 < loss {
                    legacy_lost += 1;
                    if n % 8 == 3 || n % 8 == 6 {
                        lost_release += 1;
                    }
                } else {
                    legacy_latency.push(10 + ((legacy_seed >> 8) % 3) * 10);
                    legacy_duplicates += u32::from(legacy_seed.is_multiple_of(7));
                }
            }
            latencies.sort();
            legacy_latency.sort();
            println!(
                "threshold={threshold}ms loss={loss}% KCP={}/{} duplicates=0 abort={aborted} P95={}ms P99={}ms; legacy lost={legacy_lost}/100 lost_release={lost_release} duplicates={legacy_duplicates} P95={}ms P99={}ms (virtual input transport)",
                got.len(),
                sent_times.len(),
                latencies[latencies.len() * 95 / 100],
                latencies[latencies.len() * 99 / 100],
                legacy_latency[legacy_latency.len() * 95 / 100],
                legacy_latency[legacy_latency.len() * 99 / 100]
            );
        }
    }
}

#[test]
fn bursts_below_budget_recover_and_long_outage_closes_both_sides() {
    for burst in [100, 200, 300, 500] {
        let (mut a, mut b) = pair();
        a.send(&event(3), 20).unwrap();
        let mut failed_a = false;
        let mut failed_b = false;
        let mut got = Vec::new();
        for t in (20..1800).step_by(10) {
            failed_a |= a.tick(t).is_err();
            failed_b |= b.tick(t).is_err();
            if t < 20 + burst {
                a.drain();
                b.drain();
                continue;
            }
            if failed_a || failed_b {
                continue;
            }
            exchange(&mut a, &mut b, t, false);
            while let Some((seq, bytes)) = b.receive() {
                got.push(bytes);
                b.processed(seq, t).unwrap();
            }
        }
        if burst == 100 {
            assert_eq!(got, vec![event(3)]);
            assert!(!failed_a && !failed_b);
        }
        if burst >= 300 {
            assert!(failed_a && failed_b);
        }
        println!(
            "burst={burst}ms delivered={} sender_abort={failed_a} receiver_abort={failed_b}",
            got.len()
        );
    }
}
