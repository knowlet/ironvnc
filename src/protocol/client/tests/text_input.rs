use super::*;
use crate::protocol::text_input::TypedText;

#[tokio::test]
async fn macos_compatibility_types_without_clipboard_support_after_rejected_clipboard() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = params(listener.local_addr().unwrap().port());
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let disconnect = commands_tx.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Apple banner mapped to standard RFB 3.8; the server offers no
            // clipboard extension and sends no clipboard request or reply.
            handshake(&mut stream, b"RFB 003.889\n", 8).await;
            server_init(&mut stream, 2, 1, 7).await;
            read_setup(&mut stream).await;

            // Decode actual production traffic. Invalid clipboard input must
            // have emitted no prefix, and typing must not need ClientCutText.
            for keysym in [
                0x41_u32,
                0xe9,
                0x0100_4e2d,
                0x0101_f642,
                0xff0d,
                0x42,
                0xff09,
                0x43,
                0xff0d,
                0x44,
                0xff0d,
            ] {
                for down in [1, 0] {
                    assert_eq!(stream.read_u8().await.unwrap(), 4, "Expected KeyEvent");
                    assert_eq!(stream.read_u8().await.unwrap(), down);
                    assert_eq!(stream.read_u16().await.unwrap(), 0);
                    assert_eq!(stream.read_u32().await.unwrap(), keysym);
                }
            }

            // Further normal input still works after the nonfatal failure and
            // typing operation; no trailing paste chord or stray key is sent.
            for down in [1, 0] {
                assert_eq!(stream.read_u8().await.unwrap(), 4);
                assert_eq!(stream.read_u8().await.unwrap(), down);
                assert_eq!(stream.read_u16().await.unwrap(), 0);
                assert_eq!(stream.read_u32().await.unwrap(), 0xff1b);
            }
            disconnect.send(VncCommand::Disconnect).unwrap();
            assert!(stream.read_u8().await.is_err());
        });

        let client = VncClient::connect(&connection, events_tx, commands_rx);
        let observe = async {
            assert!(matches!(
                next_event(&events_rx).await,
                VncEvent::DesktopSize(2, 1)
            ));
            assert!(matches!(
                next_event(&events_rx).await,
                VncEvent::DesktopName(name) if name == "fixture"
            ));

            commands_tx
                .send(VncCommand::SetClipboard("private prefix\0suffix".into()))
                .unwrap();
            match next_event(&events_rx).await {
                VncEvent::ClipboardStatus { message, is_error } => {
                    assert!(is_error);
                    assert!(message.contains("embedded NUL"));
                    assert!(!message.contains("private prefix"));
                }
                _ => panic!("Expected nonfatal clipboard validation feedback"),
            }

            let text = TypedText::new("Aé中🙂\r\nB\tC\rD\n").unwrap();
            commands_tx.send(VncCommand::TypeText(text)).unwrap();
            match next_event(&events_rx).await {
                VncEvent::ClipboardStatus { message, is_error } => {
                    assert!(!is_error);
                    assert_eq!(message, "Sent 11 text key taps to the server");
                }
                _ => panic!("Expected typing dispatch feedback"),
            }
            for down in [true, false] {
                commands_tx
                    .send(VncCommand::KeyEvent {
                        down,
                        keysym: 0xff1b,
                    })
                    .unwrap();
            }
        };
        let (result, ()) = tokio::join!(client, observe);
        result.unwrap();
        server.await.unwrap();
    })
    .await
    .expect("macOS-compatible text input fixture timed out");
}
