//! Real-client flow control, including an intentionally paused event consumer.
use super::*;

fn pixel_update(red: u8) -> Vec<u8> {
    vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, red, 0]
}

async fn incremental(stream: &mut TcpStream) {
    let mut bytes = [0; 10];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, [3, 1, 0, 0, 0, 0, 0, 2, 0, 1]);
}

async fn quiet(stream: &mut TcpStream) {
    assert!(
        tokio::time::timeout(Duration::from_millis(60), stream.read_u8())
            .await
            .is_err(),
        "paused consumer must stop requests after two retained updates"
    );
}

#[tokio::test]
async fn paused_consumer_limits_updates_without_blocking_input_or_disconnect() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            handshake(&mut stream, b"RFB 003.008\n", 8).await;
            server_init(&mut stream, 2, 1, 7).await;
            read_setup(&mut stream).await;
            stream.write_all(&pixel_update(1)).await.unwrap();
            incremental(&mut stream).await;
            stream.write_all(&pixel_update(2)).await.unwrap();
            quiet(&mut stream).await;
            commands_tx
                .send(VncCommand::KeyEvent {
                    down: true,
                    keysym: 65,
                })
                .unwrap();
            let mut key = [0; 8];
            tokio::time::timeout(Duration::from_millis(250), stream.read_exact(&mut key))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(key, [4, 1, 0, 0, 0, 0, 0, 65]);
            commands_tx.send(VncCommand::Disconnect).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(250), stream.read_u8())
                    .await
                    .unwrap()
                    .is_err()
            );
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("paused consumer fixture timed out");
}

// Unlike the ordinary assertion fixtures' next_event, this consumer keeps every
// fence alive so draining the channel cannot accidentally acknowledge an update.
async fn raw_event(receiver: &Receiver<VncEvent>) -> VncEvent {
    loop {
        match receiver.try_recv() {
            Ok(event) => return event,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("event channel closed: {error}"),
        }
    }
}

async fn batch(receiver: &Receiver<VncEvent>) -> Vec<VncEvent> {
    let mut events = Vec::new();
    loop {
        let event = raw_event(receiver).await;
        let complete = matches!(event, VncEvent::UpdateComplete(_));
        events.push(event);
        if complete {
            return events;
        }
    }
}

fn reds(events: &[VncEvent]) -> Vec<u8> {
    events
        .iter()
        .filter_map(|event| match event {
            VncEvent::FramebufferRects(rects) => Some(rects.iter().map(|r| r.rgba[0])),
            _ => None,
        })
        .flatten()
        .collect()
}

async fn setup(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().await.unwrap();
    handshake(&mut stream, b"RFB 003.008\n", 8).await;
    server_init(&mut stream, 2, 1, 7).await;
    read_setup(&mut stream).await;
    stream
}

async fn closed(stream: &mut TcpStream) {
    assert!(
        tokio::time::timeout(Duration::from_millis(250), stream.read_u8())
            .await
            .expect("disconnect stalled behind UI credits")
            .is_err()
    );
}

#[tokio::test]
async fn drained_but_held_updates_keep_credits_and_one_release_allows_one_update() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            stream.write_all(&pixel_update(11)).await.unwrap();
            incremental(&mut stream).await;
            stream.write_all(&pixel_update(22)).await.unwrap();
            let first = batch(&events_rx).await;
            let second = batch(&events_rx).await;
            assert_eq!(reds(&first), [11]);
            assert_eq!(reds(&second), [22]);
            assert!(events_rx.is_empty());
            quiet(&mut stream).await;
            // Server controls are still consumed while requests are withheld.
            stream
                .write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 1, b'x'])
                .await
                .unwrap();
            assert!(
                matches!(raw_event(&events_rx).await, VncEvent::ClipboardText(text) if text == "x")
            );
            drop(first);
            incremental(&mut stream).await;
            stream.write_all(&pixel_update(33)).await.unwrap();
            let third = batch(&events_rx).await;
            assert_eq!(reds(&third), [33]);
            assert_eq!(
                reds(&second),
                [22],
                "later pixels mutated a retained snapshot"
            );
            quiet(&mut stream).await;
            drop(second);
            incremental(&mut stream).await;
            quiet(&mut stream).await; // no duplicate request while one is outstanding
            commands_tx.send(VncCommand::Disconnect).unwrap();
            closed(&mut stream).await;
            drop(third);
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("held-consumer fixture timed out");
}

#[tokio::test]
async fn unsolicited_update_waits_before_payload_but_preserves_input_and_pixels() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            stream.write_all(&pixel_update(1)).await.unwrap();
            incremental(&mut stream).await;
            stream.write_all(&pixel_update(2)).await.unwrap();
            let first = batch(&events_rx).await;
            let second = batch(&events_rx).await;
            stream.write_all(&pixel_update(3)).await.unwrap(); // no matching request
            stream
                .write_all(&[3, 0, 0, 0, 0, 0, 0, 1, b'z'])
                .await
                .unwrap();
            quiet(&mut stream).await;
            assert!(
                events_rx.is_empty(),
                "unsolicited payload bypassed the credit gate"
            );
            commands_tx
                .send(VncCommand::PointerEvent {
                    buttons: 1,
                    x: 1,
                    y: 0,
                })
                .unwrap();
            let mut pointer = [0; 6];
            tokio::time::timeout(Duration::from_millis(250), stream.read_exact(&mut pointer))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pointer, [5, 1, 0, 1, 0, 0]);
            drop(first);
            let third = batch(&events_rx).await;
            assert_eq!(reds(&third), [3]);
            assert!(
                matches!(raw_event(&events_rx).await, VncEvent::ClipboardText(text) if text == "z")
            );
            quiet(&mut stream).await;
            // The unsolicited update consumed the released credit without an
            // extra request; disconnect works even with the other two held.
            commands_tx.send(VncCommand::Disconnect).unwrap();
            closed(&mut stream).await;
            drop((second, third));
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("unsolicited update fixture timed out");
}

#[tokio::test]
async fn empty_cursor_and_resize_only_updates_all_complete_before_later_pixels() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            // Empty and 1x1 cursor-only updates both occupy a credit.
            stream.write_all(&[0, 0, 0, 0]).await.unwrap();
            incremental(&mut stream).await;
            let mut cursor = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1];
            cursor.extend_from_slice(&enc::CURSOR.to_be_bytes());
            cursor.extend_from_slice(&[0, 0, 0, 0, 0x80]);
            stream.write_all(&cursor).await.unwrap();
            let empty = batch(&events_rx).await;
            let cursor = batch(&events_rx).await;
            assert!(reds(&empty).is_empty() && reds(&cursor).is_empty());
            quiet(&mut stream).await;
            drop(empty);
            incremental(&mut stream).await;
            let mut resize = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 3, 0, 1];
            resize.extend_from_slice(&enc::DESKTOP_SIZE.to_be_bytes());
            stream.write_all(&resize).await.unwrap();
            let resized = batch(&events_rx).await;
            assert!(matches!(resized.first(), Some(VncEvent::DesktopSize(3, 1))));
            assert!(reds(&resized).is_empty());
            quiet(&mut stream).await;
            // While the full request is deferred, accept an unsolicited empty
            // update without forgetting the pending resize/full-refresh state.
            stream.write_all(&[0, 0, 0, 0]).await.unwrap();
            quiet(&mut stream).await;
            drop(cursor);
            let unsolicited = batch(&events_rx).await;
            quiet(&mut stream).await;
            drop(resized);
            let mut full = [0; 10];
            stream.read_exact(&mut full).await.unwrap();
            assert_eq!(full, [3, 0, 0, 0, 0, 0, 0, 3, 0, 1]);
            stream.write_all(&pixel_update(77)).await.unwrap();
            let pixels = batch(&events_rx).await;
            assert_eq!(reds(&pixels), [77]);
            commands_tx.send(VncCommand::Disconnect).unwrap();
            closed(&mut stream).await;
            drop((unsolicited, pixels));
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("non-pixel update fixture timed out");
}

#[tokio::test]
async fn dropping_owner_cancels_header_only_credit_wait_even_when_queue_releases_credits() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = crate::connection::VncConnection::connect(params(
            listener.local_addr().unwrap().port(),
        ));
        let mut stream = setup(&listener).await;
        stream.write_all(&pixel_update(1)).await.unwrap();
        incremental(&mut stream).await;
        stream.write_all(&pixel_update(2)).await.unwrap();
        quiet(&mut stream).await;
        stream.write_all(&[0]).await.unwrap(); // unsolicited header, no payload ever
        quiet(&mut stream).await;
        drop(connection); // also drops queued fences and makes credits available
        closed(&mut stream).await;
    })
    .await
    .expect("owner-drop fixture timed out");
}

#[tokio::test]
async fn reconnect_has_fresh_credits_while_old_drained_updates_are_still_held() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection_params = params(listener.local_addr().unwrap().port());
        let old = crate::connection::VncConnection::connect(connection_params.clone());
        let mut stream = setup(&listener).await;
        stream.write_all(&pixel_update(1)).await.unwrap();
        incremental(&mut stream).await;
        stream.write_all(&pixel_update(2)).await.unwrap();
        let first = batch(&old.event_rx).await;
        let second = batch(&old.event_rx).await;
        drop(old);
        closed(&mut stream).await;
        let new = crate::connection::VncConnection::connect(connection_params);
        let mut stream = setup(&listener).await;
        stream.write_all(&pixel_update(3)).await.unwrap();
        incremental(&mut stream).await;
        stream.write_all(&pixel_update(4)).await.unwrap();
        let third = batch(&new.event_rx).await;
        let fourth = batch(&new.event_rx).await;
        quiet(&mut stream).await;
        drop((first, second));
        quiet(&mut stream).await; // old-generation releases cannot grant new credits
        drop(new);
        closed(&mut stream).await;
        assert_eq!(reds(&third), [3]);
        assert_eq!(reds(&fourth), [4]);
    })
    .await
    .expect("reconnect fixture timed out");
}

#[tokio::test]
async fn dropping_command_owner_exits_while_all_update_credits_are_held() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            stream.write_all(&pixel_update(1)).await.unwrap();
            incremental(&mut stream).await;
            stream.write_all(&pixel_update(2)).await.unwrap();
            quiet(&mut stream).await;
            drop(commands_tx);
            closed(&mut stream).await;
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("command-owner fixture timed out");
}

#[tokio::test]
async fn dropping_owner_cancels_after_unsolicited_update_obtains_credit_and_stalls_in_payload() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = crate::connection::VncConnection::connect(params(
            listener.local_addr().unwrap().port(),
        ));
        let command_observer = connection.cmd_tx.clone();
        let mut stream = setup(&listener).await;
        stream.write_all(&pixel_update(1)).await.unwrap();
        incremental(&mut stream).await;
        stream.write_all(&pixel_update(2)).await.unwrap();
        let first = batch(&connection.event_rx).await;
        let second = batch(&connection.event_rx).await;
        // An unsolicited resize followed by a Raw header with no pixel payload.
        let mut packet = vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 3, 0, 1];
        packet.extend_from_slice(&enc::DESKTOP_SIZE.to_be_bytes());
        packet.extend_from_slice(&pixel_update(3)[4..16]);
        stream.write_all(&packet).await.unwrap();
        quiet(&mut stream).await;
        drop(first);
        // Receiving this resize proves the decoder passed the credit gate.
        // There is no command drain before the deliberately absent Raw payload.
        assert!(matches!(
            raw_event(&connection.event_rx).await,
            VncEvent::DesktopSize(3, 1)
        ));
        drop(connection);
        closed(&mut stream).await;
        // Worker exit is independent of a retained command producer and fence.
        for _ in 0..100 {
            if command_observer.send(VncCommand::Disconnect).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(second);
        panic!("worker survived owner cancellation");
    })
    .await
    .expect("post-credit cancellation fixture timed out");
}

#[tokio::test]
async fn explicit_disconnect_interrupts_incomplete_payload_without_dropping_owner() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = crate::connection::VncConnection::connect(params(
            listener.local_addr().unwrap().port(),
        ));
        let mut stream = setup(&listener).await;
        // The initial request already owns a credit. Stall in its Raw payload.
        stream.write_all(&pixel_update(1)[..18]).await.unwrap();
        quiet(&mut stream).await;
        connection.disconnect();
        closed(&mut stream).await;
        loop {
            if matches!(
                raw_event(&connection.event_rx).await,
                VncEvent::Disconnected
            ) {
                break;
            }
        }
    })
    .await
    .expect("explicit disconnect fixture timed out");
}

#[tokio::test]
async fn immediate_disconnect_is_not_lost_before_worker_polls_cancellation() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..8 {
            let connection = crate::connection::VncConnection::connect(params(
                listener.local_addr().unwrap().port(),
            ));
            connection.disconnect();
            assert!(matches!(
                raw_event(&connection.event_rx).await,
                VncEvent::Disconnected
            ));
            assert!(connection.cmd_tx.send(VncCommand::Disconnect).is_err());
        }
    })
    .await
    .expect("immediate cancellation fixture timed out");
}

#[tokio::test]
async fn explicit_disconnect_wakes_synchronous_password_prompt() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = crate::connection::VncConnection::connect(params(
            listener.local_addr().unwrap().port(),
        ));
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut reply = [0; 12];
        stream.read_exact(&mut reply).await.unwrap();
        stream.write_all(&[1, 2]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 2);
        stream.write_all(&[0; 16]).await.unwrap();
        assert!(matches!(
            raw_event(&connection.event_rx).await,
            VncEvent::NeedPassword
        ));
        connection.disconnect();
        closed(&mut stream).await;
        assert!(matches!(
            raw_event(&connection.event_rx).await,
            VncEvent::Disconnected | VncEvent::Error(_)
        ));
    })
    .await
    .expect("password cancellation fixture timed out");
}

#[tokio::test]
async fn persistent_zrle_survives_paused_consumer_and_fragmented_next_update() {
    use flate2::{Compress, Compression, FlushCompress};
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            let mut compressor = Compress::new(Compression::default(), true);
            let mut update = |red| {
                let mut compressed = Vec::with_capacity(64);
                compressor
                    .compress_vec(&[1, 0, 0, red], &mut compressed, FlushCompress::Sync)
                    .unwrap();
                let mut packet = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1];
                packet.extend_from_slice(&enc::ZRLE.to_be_bytes());
                packet.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
                packet.extend(compressed);
                packet
            };
            stream.write_all(&update(11)).await.unwrap();
            incremental(&mut stream).await;
            stream.write_all(&update(22)).await.unwrap();
            let first = batch(&events_rx).await;
            let second = batch(&events_rx).await;
            quiet(&mut stream).await;
            drop(first);
            incremental(&mut stream).await;
            let next = update(33);
            stream.write_all(&next[..next.len() - 1]).await.unwrap();
            quiet(&mut stream).await;
            assert!(
                events_rx.is_empty(),
                "fragmented compressed update completed early"
            );
            stream.write_all(&next[next.len() - 1..]).await.unwrap();
            let third = batch(&events_rx).await;
            assert_eq!(reds(&second), [22]);
            assert_eq!(reds(&third), [33]);
            commands_tx.send(VncCommand::Disconnect).unwrap();
            closed(&mut stream).await;
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        result.unwrap();
    })
    .await
    .expect("persistent ZRLE credit fixture timed out");
}

#[tokio::test]
async fn failed_decode_releases_its_reserved_credit_without_releasing_retained_update() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (_commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let mut stream = setup(&listener).await;
            stream.write_all(&pixel_update(1)).await.unwrap();
            incremental(&mut stream).await;
            let first = batch(&events_rx).await;
            let VncEvent::UpdateComplete(permit) = first.last().unwrap() else {
                panic!("missing fence")
            };
            let credits = permit.semaphore().clone();
            assert_eq!(credits.available_permits(), 0);
            let mut invalid = pixel_update(2);
            invalid[5] = 2; // outside the 2x1 framebuffer; payload must not be read
            stream.write_all(&invalid[..16]).await.unwrap();
            closed(&mut stream).await;
            assert_eq!(credits.available_permits(), 1);
            drop(first);
            assert_eq!(credits.available_permits(), 2);
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("outside the framebuffer"));
    })
    .await
    .expect("decode failure credit fixture timed out");
}
