//! Wire-level regressions exercising the production client, not a parser substitute.
use super::*;
use flate2::{Compress, Compression, FlushCompress};

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

fn rect(encoding: i32, bounds: (u16, u16, u16, u16), payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [bounds.0, bounds.1, bounds.2, bounds.3] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&encoding.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn resize(width: u16, height: u16) -> Vec<u8> {
    // DesktopSize x/y are ignored, not framebuffer coordinates.
    rect(enc::DESKTOP_SIZE, (u16::MAX, u16::MAX, width, height), &[])
}

fn pixel(x: u16, y: u16, rgba: [u8; 4]) -> Vec<u8> {
    rect(enc::RAW, (x, y, 1, 1), &[rgba[2], rgba[1], rgba[0], 0])
}

fn update(rectangles: Vec<Vec<u8>>) -> Vec<u8> {
    let mut bytes = vec![server_msg::FB_UPDATE, 0];
    bytes.extend_from_slice(&(rectangles.len() as u16).to_be_bytes());
    for rectangle in rectangles {
        bytes.extend(rectangle);
    }
    bytes
}

fn request(incremental: bool, width: u16, height: u16) -> [u8; 10] {
    let mut bytes = [3, u8::from(incremental), 0, 0, 0, 0, 0, 0, 0, 0];
    bytes[6..8].copy_from_slice(&width.to_be_bytes());
    bytes[8..10].copy_from_slice(&height.to_be_bytes());
    bytes
}

async fn assert_quiet(stream: &mut TcpStream) {
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.read_u8())
            .await
            .is_err(),
        "client sent an early/duplicate request or closed the connection"
    );
}

// Each update can be split across writes to test that no response is sent until
// all rectangle headers and payloads have been consumed. All waits are bounded.
async fn exchange(updates: Vec<(Vec<Vec<u8>>, [u8; 10])>) -> Vec<VncEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            handshake(&mut stream, b"RFB 003.008\n", 8).await;
            server_init(&mut stream, 2, 1, 7).await;
            read_setup(&mut stream).await;
            for (fragments, expected) in updates {
                let count = fragments.len();
                for (index, fragment) in fragments.into_iter().enumerate() {
                    stream.write_all(&fragment).await.unwrap();
                    if index + 1 < count {
                        assert_quiet(&mut stream).await;
                    }
                }
                let mut actual = [0; 10];
                stream.read_exact(&mut actual).await.unwrap();
                assert_eq!(actual, expected);
                assert_quiet(&mut stream).await;
            }
            // A Bell followed by a clipboard marker proves the next server
            // message is still aligned, including after compressed payloads.
            stream
                .write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 4, b's', b'y', b'n', b'c'])
                .await
                .unwrap();
            assert!(stream.read_u8().await.is_err());
        };
        let observe = async {
            assert!(matches!(
                next_event(&events_rx).await,
                VncEvent::DesktopSize(2, 1)
            ));
            assert!(matches!(
                next_event(&events_rx).await,
                VncEvent::DesktopName(name) if name == "fixture"
            ));
            let mut events = Vec::new();
            loop {
                let event = next_event(&events_rx).await;
                if matches!(&event, VncEvent::ClipboardText(text) if text == "sync") {
                    commands_tx.send(VncCommand::Disconnect).unwrap();
                    return events;
                }
                events.push(event);
            }
        };
        let (result, (), events) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server,
            observe
        );
        result.unwrap();
        events
    })
    .await
    .expect("DesktopSize exchange timed out")
}

fn assert_size(event: &VncEvent, width: u32, height: u32) {
    assert!(matches!(event, VncEvent::DesktopSize(w, h) if (*w, *h) == (width, height)));
}

fn assert_pixels(event: &VncEvent, expected: &[(u32, u32, [u8; 4])]) {
    let VncEvent::FramebufferRects(rectangles) = event else {
        panic!("expected framebuffer rectangles");
    };
    assert_eq!(rectangles.len(), expected.len());
    for (rectangle, &(x, y, rgba)) in rectangles.iter().zip(expected) {
        assert_eq!(
            (rectangle.x, rectangle.y, rectangle.w, rectangle.h),
            (x, y, 1, 1)
        );
        assert_eq!(rectangle.rgba, rgba);
    }
}

#[tokio::test]
async fn desktop_size_alone_or_last_discards_old_dirty_rectangles() {
    for with_old_rectangle in [false, true] {
        let mut rectangles = Vec::new();
        if with_old_rectangle {
            // This was valid before shrinking, but must not reach the new UI.
            rectangles.push(pixel(1, 0, RED));
        }
        rectangles.push(resize(1, 1));
        let events = exchange(vec![
            (vec![update(rectangles)], request(false, 1, 1)),
            (vec![update(vec![pixel(0, 0, GREEN)])], request(true, 1, 1)),
        ])
        .await;
        assert_eq!(events.len(), 2);
        assert_size(&events[0], 1, 1);
        assert_pixels(&events[1], &[(0, 0, GREEN)]);
    }
}

#[tokio::test]
async fn desktop_size_first_decodes_raw_using_new_width_height_and_stride() {
    let events = exchange(vec![
        (
            vec![update(vec![
                resize(4, 2),
                pixel(3, 1, RED),
                pixel(2, 1, BLUE),
            ])],
            request(false, 4, 2),
        ),
        (vec![update(vec![pixel(3, 1, GREEN)])], request(true, 4, 2)),
    ])
    .await;
    assert_eq!(events.len(), 3);
    assert_size(&events[0], 4, 2);
    assert_pixels(&events[1], &[(3, 1, RED), (2, 1, BLUE)]);
    assert_pixels(&events[2], &[(3, 1, GREEN)]);
}

#[tokio::test]
async fn desktop_size_in_middle_keeps_only_pixels_after_shrink() {
    let events = exchange(vec![
        (
            vec![update(vec![
                pixel(1, 0, RED),
                resize(1, 1),
                pixel(0, 0, GREEN),
            ])],
            request(false, 1, 1),
        ),
        (vec![update(vec![pixel(0, 0, BLUE)])], request(true, 1, 1)),
    ])
    .await;
    assert_eq!(events.len(), 3);
    assert_size(&events[0], 1, 1);
    assert_pixels(&events[1], &[(0, 0, GREEN)]);
    assert_pixels(&events[2], &[(0, 0, BLUE)]);
}

#[tokio::test]
async fn desktop_size_followed_by_hextile_tight_and_copyrect() {
    for rectangle in [
        rect(enc::HEXTILE, (2, 1, 1, 1), &[1, 0, 0, 255, 0]),
        rect(enc::TIGHT, (2, 1, 1, 1), &[0x80, 255, 0, 0]),
        rect(enc::COPY_RECT, (2, 1, 1, 1), &[0, 3, 0, 1]),
    ] {
        let events = exchange(vec![(
            vec![update(vec![resize(4, 2), pixel(3, 1, RED), rectangle])],
            request(false, 4, 2),
        )])
        .await;
        assert_eq!(events.len(), 2);
        assert_size(&events[0], 4, 2);
        assert_pixels(&events[1], &[(3, 1, RED), (2, 1, RED)]);
    }
}

fn zrle_solid(compressor: &mut Compress, x: u16, y: u16, rgba: [u8; 4]) -> Vec<u8> {
    let mut compressed = Vec::with_capacity(64);
    compressor
        .compress_vec(
            &[1, rgba[2], rgba[1], rgba[0]],
            &mut compressed,
            FlushCompress::Sync,
        )
        .unwrap();
    let mut payload = (compressed.len() as u32).to_be_bytes().to_vec();
    payload.extend(compressed);
    rect(enc::ZRLE, (x, y, 1, 1), &payload)
}

#[tokio::test]
async fn desktop_size_keeps_zrle_stream_state_across_resize_and_next_update() {
    let mut compressor = Compress::new(Compression::default(), true);
    let before = zrle_solid(&mut compressor, 1, 0, RED);
    let after = zrle_solid(&mut compressor, 3, 1, BLUE);
    let next = zrle_solid(&mut compressor, 2, 1, GREEN);
    let events = exchange(vec![
        (
            vec![update(vec![before, resize(4, 2), after])],
            request(false, 4, 2),
        ),
        (vec![update(vec![next])], request(true, 4, 2)),
    ])
    .await;
    assert_eq!(events.len(), 3);
    assert_size(&events[0], 4, 2);
    assert_pixels(&events[1], &[(3, 1, BLUE)]);
    assert_pixels(&events[2], &[(2, 1, GREEN)]);
}

#[tokio::test]
async fn desktop_size_fragmented_update_defers_full_request_until_payload_complete() {
    let bytes = update(vec![resize(4, 2), pixel(3, 1, RED)]);
    let events = exchange(vec![
        (
            vec![
                bytes[..16].to_vec(),
                bytes[16..29].to_vec(),
                bytes[29..].to_vec(),
            ],
            request(false, 4, 2),
        ),
        (vec![update(vec![pixel(3, 1, GREEN)])], request(true, 4, 2)),
        (vec![update(vec![])], request(true, 4, 2)),
    ])
    .await;
    assert_eq!(events.len(), 3);
    assert_size(&events[0], 4, 2);
    assert_pixels(&events[1], &[(3, 1, RED)]);
    assert_pixels(&events[2], &[(3, 1, GREEN)]);
}

#[tokio::test]
async fn desktop_size_multiple_resizes_coalesce_one_full_request_for_final_size() {
    let events = exchange(vec![
        (
            vec![update(vec![
                resize(4, 2),
                pixel(3, 1, RED),
                resize(1, 1),
                pixel(0, 0, BLUE),
            ])],
            request(false, 1, 1),
        ),
        (vec![update(vec![pixel(0, 0, GREEN)])], request(true, 1, 1)),
    ])
    .await;
    assert_eq!(events.len(), 4);
    assert_size(&events[0], 4, 2);
    assert_size(&events[1], 1, 1);
    assert_pixels(&events[2], &[(0, 0, BLUE)]);
    assert_pixels(&events[3], &[(0, 0, GREEN)]);
}

// Keep the server's write half open for invalid headers: rejecting an invalid
// rectangle must not require reading its (deliberately absent) payload.
async fn reject(packet: Vec<u8>, expected: &str, truncate: bool) -> Vec<VncEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (_commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            handshake(&mut stream, b"RFB 003.008\n", 8).await;
            server_init(&mut stream, 2, 1, 7).await;
            read_setup(&mut stream).await;
            stream.write_all(&packet).await.unwrap();
            if truncate {
                stream.shutdown().await.unwrap();
            }
            assert!(
                stream.read_u8().await.is_err(),
                "request sent for invalid update"
            );
        };
        let (result, ()) = tokio::join!(
            VncClient::connect(&connection, events_tx, commands_rx),
            server
        );
        let error = result.unwrap_err();
        if truncate {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        } else {
            assert!(
                error.to_string().contains(expected),
                "unexpected error: {error}"
            );
        }
        let events: Vec<_> = events_rx.try_iter().collect();
        assert!(matches!(events[0], VncEvent::DesktopSize(2, 1)));
        assert!(matches!(&events[1], VncEvent::DesktopName(name) if name == "fixture"));
        events.into_iter().skip(2).collect()
    })
    .await
    .expect("DesktopSize invalid-packet fixture timed out")
}

#[tokio::test]
async fn desktop_size_rejects_invalid_new_bounds_before_all_decoder_payloads() {
    for encoding in [
        enc::RAW,
        enc::COPY_RECT,
        enc::HEXTILE,
        enc::TIGHT,
        enc::ZRLE,
    ] {
        for bounds in [
            (1, 0, 1, 1), // valid in the old 2x1 framebuffer, invalid after shrink
            (0, 1, 1, 1),
            (0, 0, 2, 1),
            (0, 0, 1, 2),
            (u16::MAX, 0, 1, 1),
            (0, u16::MAX, 1, 1),
            (0, 0, 0, 1),
            (0, 0, 1, 0),
        ] {
            let events = reject(
                update(vec![resize(1, 1), rect(encoding, bounds, &[])]),
                "Rectangle is outside the framebuffer",
                false,
            )
            .await;
            assert_eq!(events.len(), 1);
            assert_size(&events[0], 1, 1);
        }
    }
}

#[tokio::test]
async fn desktop_size_rejects_copyrect_source_outside_new_bounds() {
    let events = reject(
        update(vec![
            resize(1, 1),
            rect(enc::COPY_RECT, (0, 0, 1, 1), &[0, 1, 0, 0]),
        ]),
        "CopyRect is outside the framebuffer",
        false,
    )
    .await;
    assert_eq!(events.len(), 1);
    assert_size(&events[0], 1, 1);
}

#[tokio::test]
async fn desktop_size_rejects_invalid_dimensions_before_allocating_or_requesting() {
    for (width, height, expected) in [
        (0, 1, "Empty framebuffer dimensions"),
        (1, 0, "Empty framebuffer dimensions"),
        (u16::MAX, u16::MAX, "Framebuffer exceeds 32 megapixel limit"),
    ] {
        let events = reject(update(vec![resize(width, height)]), expected, false).await;
        assert!(events.is_empty());
    }
}

#[tokio::test]
async fn desktop_size_truncated_raw_does_not_publish_or_request_partial_update() {
    let mut bytes = update(vec![resize(4, 2), pixel(3, 1, RED)]);
    bytes.pop();
    let events = reject(bytes, "", true).await;
    assert_eq!(events.len(), 1);
    assert_size(&events[0], 4, 2);
}

#[tokio::test]
async fn desktop_size_bounds_resize_count_before_allocation_or_events() {
    let rectangles: Vec<_> = (0..MAX_UPDATE_RESIZES).map(|_| resize(1, 1)).collect();
    // Exactly the allowed count succeeds, and the next update gets a new budget.
    let events = exchange(vec![
        (vec![update(rectangles.clone())], request(false, 1, 1)),
        (vec![update(rectangles)], request(false, 1, 1)),
    ])
    .await;
    assert_eq!(events.len(), 2 * usize::from(MAX_UPDATE_RESIZES));
    for event in &events {
        assert_size(event, 1, 1);
    }

    let events = reject(
        update((0..=MAX_UPDATE_RESIZES).map(|_| resize(1, 1)).collect()),
        "Framebuffer update has too many DesktopSize rectangles",
        false,
    )
    .await;
    // The over-budget marker must not emit a UI event or trigger a request.
    assert_eq!(events.len(), usize::from(MAX_UPDATE_RESIZES));
    for event in &events {
        assert_size(event, 1, 1);
    }
}

#[tokio::test]
async fn desktop_size_bounds_cumulative_resize_pixels_separately_from_data() {
    let side = 4096u16;
    assert_eq!(2 * u64::from(side) * u64::from(side), MAX_FRAMEBUFFER_PIXELS);
    let events = exchange(vec![
        (
            vec![update(vec![
                resize(side, side),
                resize(side, side),
                pixel(side - 1, side - 1, RED),
            ])],
            request(false, side, side),
        ),
        (
            vec![update(vec![resize(1, 1), pixel(0, 0, GREEN)])],
            request(false, 1, 1),
        ),
    ])
    .await;
    assert_eq!(events.len(), 5);
    assert_size(&events[0], u32::from(side), u32::from(side));
    assert_size(&events[1], u32::from(side), u32::from(side));
    assert_pixels(&events[2], &[(u32::from(side - 1), u32::from(side - 1), RED)]);
    assert_size(&events[3], 1, 1);
    assert_pixels(&events[4], &[(0, 0, GREEN)]);

    let events = reject(
        update(vec![resize(side, side), resize(side, side), resize(1, 1)]),
        "Framebuffer update exceeds resize pixel budget",
        false,
    )
    .await;
    // Reject before reallocating or publishing the final 1x1 size.
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_size(event, u32::from(side), u32::from(side));
    }
}
