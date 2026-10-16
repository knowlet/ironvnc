use crate::connection::{ConnectionParams, FbRect, VncCommand, VncEvent};
use crate::framebuffer::Framebuffer;
use crate::protocol::{
    clipboard::Clipboard,
    encoding::{copyrect, hextile, raw, tight, zrle},
    messages::{encoding as enc, server_msg, PixelFormat},
    security,
};
use anyhow::{bail, ensure, Result};
use crossbeam_channel::{Receiver, Sender};
use flate2::Decompress;
use tight::TightState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

// Explicit resource budgets, including enough pixels for a 7680×4320 desktop.
const MAX_FRAMEBUFFER_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DESKTOP_NAME_BYTES: u32 = 1024 * 1024;
const MAX_UPDATE_RECTANGLES: u16 = 16_384;
// Bound allocation and UI-event churn separately from decoded pixel work.
const MAX_UPDATE_RESIZES: u16 = 16;

pub struct VncClient;

impl VncClient {
    pub async fn connect(
        params: &ConnectionParams,
        event_tx: Sender<VncEvent>,
        cmd_rx: Receiver<VncCommand>,
    ) -> Result<()> {
        let addr = format!("{}:{}", params.host, params.port);
        info!("Connecting to {addr}");
        // Keep the raw TcpStream (no BufReader) through the security handshake:
        // RA2 needs to take ownership of it, and a BufReader could over-read
        // handshake bytes into a buffer that would then be lost.
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Connection timed out after 10 seconds"))??;

        // --- Protocol version handshake ---
        let mut ver_buf = [0u8; 12];
        stream.read_exact(&mut ver_buf).await?;
        let ver_str = std::str::from_utf8(&ver_buf)?;
        debug!("Server version: {ver_str:?}");

        let rfb_minor = negotiate_version(&ver_buf)?;

        let reply = format!("RFB 003.00{}\n", rfb_minor);
        info!("Server offered {ver_str:?}; replying RFB 003.00{rfb_minor}");
        stream.write_all(reply.as_bytes()).await?;

        // --- Security (may replace the stream with an AES-encrypted channel) ---
        let mut stream = security::negotiate(
            stream,
            params.username.as_deref(),
            params.password.as_deref(),
            rfb_minor,
            &event_tx,
            &cmd_rx,
        )
        .await?;

        // --- ClientInit ---
        stream.write_u8(1).await?;

        // --- ServerInit ---
        let fb_width = stream.read_u16().await?;
        let fb_height = stream.read_u16().await?;
        validate_framebuffer_size(fb_width, fb_height)?;

        let bpp = stream.read_u8().await?;
        let depth = stream.read_u8().await?;
        let big_endian = stream.read_u8().await? != 0;
        let true_colour = stream.read_u8().await? != 0;
        let red_max = stream.read_u16().await?;
        let green_max = stream.read_u16().await?;
        let blue_max = stream.read_u16().await?;
        let red_shift = stream.read_u8().await?;
        let green_shift = stream.read_u8().await?;
        let blue_shift = stream.read_u8().await?;
        stream.read_exact(&mut [0u8; 3]).await?;

        let name_len = stream.read_u32().await?;
        ensure!(
            name_len <= MAX_DESKTOP_NAME_BYTES,
            "Desktop name exceeds 1 MiB limit"
        );
        let mut name_bytes = vec![0u8; name_len as usize];
        stream.read_exact(&mut name_bytes).await?;
        let name = String::from_utf8_lossy(&name_bytes).to_string();

        info!("Desktop: {name} ({fb_width}x{fb_height})");
        info!(
            "Server pixel format: {bpp}bpp depth={depth} big_endian={big_endian} \
             true_colour={true_colour} rgb_max=({red_max},{green_max},{blue_max}) \
             rgb_shift=({red_shift},{green_shift},{blue_shift})"
        );

        // Decoders use one known true-colour format, regardless of whether
        // ServerInit advertises indexed colour, RGB565, or a different endian.
        let pf = PixelFormat::preferred();
        send_pixel_format(&mut stream, &pf).await?;

        let _ = event_tx.send(VncEvent::DesktopSize(fb_width as u32, fb_height as u32));
        let _ = event_tx.send(VncEvent::DesktopName(name));

        // --- SetEncodings (prefer ZRLE > Hextile > CopyRect > Raw; Tight last) ---
        let encodings: &[i32] = &[
            enc::ZRLE,
            enc::HEXTILE,
            enc::COPY_RECT,
            enc::RAW,
            enc::DESKTOP_SIZE,
            enc::TIGHT,
            enc::EXTENDED_CLIPBOARD,
        ];
        let mut enc_msg = Vec::with_capacity(4 + encodings.len() * 4);
        enc_msg.push(crate::protocol::messages::client_msg::SET_ENCODINGS);
        enc_msg.push(0);
        enc_msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for &e in encodings {
            enc_msg.extend_from_slice(&e.to_be_bytes());
        }
        stream.write_all(&enc_msg).await?;

        // --- Initial full FramebufferUpdateRequest ---
        send_fb_update_request(&mut stream, false, 0, 0, fb_width, fb_height).await?;

        let mut fb = Framebuffer::new(fb_width as u32, fb_height as u32);
        let mut zrle_decomp = Decompress::new(true);
        let mut tight_state = TightState::new();
        let mut clipboard = Clipboard::default();

        // --- Main message loop ---
        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    VncCommand::Disconnect => return Ok(()),
                    VncCommand::KeyEvent { down, keysym } => {
                        send_key_event(&mut stream, down, keysym).await?;
                    }
                    VncCommand::PointerEvent { buttons, x, y } => {
                        send_pointer_event(&mut stream, buttons, x, y).await?;
                    }
                    VncCommand::SetClipboard(text) => {
                        // Invalid local text must not terminate a working session.
                        if let Err(error) = clipboard.send_local(&mut stream, text).await {
                            if error.downcast_ref::<std::io::Error>().is_some() {
                                return Err(error);
                            }
                            warn!("Cannot send clipboard: {error}");
                        }
                    }
                    // ProvidePassword is only consumed during security negotiation;
                    // ignore any stray commands that arrive after auth completes.
                    VncCommand::ProvidePassword(_) | VncCommand::ProvideCredentials(_) => {}
                }
            }

            // Wait for the next server message, but only briefly: a VNC server
            // replies to an incremental update request only when the screen
            // changes, so on a static screen this read would block forever and
            // starve the command drain above (input never gets sent → deadlock).
            // Time out quickly and loop back to flush any pending input; that
            // input reaches the server, the screen changes, and updates resume.
            // read_u8 goes through a BufReader, so a timed-out read consumes no
            // bytes and is safe to retry.
            let msg_type =
                match tokio::time::timeout(std::time::Duration::from_millis(5), stream.read_u8())
                    .await
                {
                    Ok(r) => r?,
                    Err(_) => continue,
                };
            match msg_type {
                server_msg::FB_UPDATE => {
                    stream.read_u8().await?;
                    let n_rects = stream.read_u16().await?;
                    ensure!(
                        n_rects <= MAX_UPDATE_RECTANGLES,
                        "Framebuffer update has too many rectangles"
                    );
                    let mut update_pixels = 0u64;
                    let mut resized = false;
                    let mut resize_count = 0u16;
                    let mut resize_pixels = 0u64;

                    // Collect the bounds of each drawn rectangle so we can send
                    // only those regions to the UI (partial texture updates).
                    let mut dirty: Vec<(u16, u16, u16, u16)> = Vec::with_capacity(n_rects as usize);

                    for _ in 0..n_rects {
                        let rx = stream.read_u16().await?;
                        let ry = stream.read_u16().await?;
                        let rw = stream.read_u16().await?;
                        let rh = stream.read_u16().await?;
                        let encoding = stream.read_i32().await?;

                        if matches!(
                            encoding,
                            enc::RAW | enc::COPY_RECT | enc::ZRLE | enc::HEXTILE | enc::TIGHT
                        ) {
                            validate_rectangle(&fb, rx, ry, rw, rh)?;
                            update_pixels += u64::from(rw) * u64::from(rh);
                            ensure!(
                                update_pixels <= MAX_FRAMEBUFFER_PIXELS,
                                "Framebuffer update exceeds pixel budget"
                            );
                        }

                        match encoding {
                            enc::RAW => {
                                raw::decode(&mut stream, &mut fb, &pf, rx, ry, rw, rh).await?;
                                dirty.push((rx, ry, rw, rh));
                            }
                            enc::COPY_RECT => {
                                copyrect::decode(&mut stream, &mut fb, rx, ry, rw, rh).await?;
                                dirty.push((rx, ry, rw, rh));
                            }
                            enc::ZRLE => {
                                zrle::decode(
                                    &mut stream,
                                    &mut fb,
                                    &pf,
                                    rx,
                                    ry,
                                    rw,
                                    rh,
                                    &mut zrle_decomp,
                                )
                                .await?;
                                dirty.push((rx, ry, rw, rh));
                            }
                            enc::HEXTILE => {
                                hextile::decode(&mut stream, &mut fb, &pf, rx, ry, rw, rh).await?;
                                dirty.push((rx, ry, rw, rh));
                            }
                            enc::TIGHT => {
                                tight::decode(
                                    &mut stream,
                                    &mut fb,
                                    &pf,
                                    rx,
                                    ry,
                                    rw,
                                    rh,
                                    &mut tight_state,
                                )
                                .await?;
                                dirty.push((rx, ry, rw, rh));
                            }
                            enc::DESKTOP_SIZE => {
                                // DesktopSize may precede data for the new size (RFB 7.7.4).
                                // Resize now so subsequent rectangles use the new bounds.
                                validate_framebuffer_size(rw, rh)?;
                                resize_count += 1;
                                ensure!(
                                    resize_count <= MAX_UPDATE_RESIZES,
                                    "Framebuffer update has too many DesktopSize rectangles"
                                );
                                resize_pixels += u64::from(rw) * u64::from(rh);
                                ensure!(
                                    resize_pixels <= MAX_FRAMEBUFFER_PIXELS,
                                    "Framebuffer update exceeds resize pixel budget"
                                );
                                fb.resize(rw as u32, rh as u32);
                                dirty.clear();
                                resized = true;
                                let _ = event_tx.send(VncEvent::DesktopSize(rw as u32, rh as u32));
                            }
                            enc::CURSOR => {
                                // RichCursor pseudo-encoding: read and discard cursor image + bitmask.
                                let bpp = (pf.bits_per_pixel / 8) as usize;
                                let img_bytes = rw as usize * rh as usize * bpp;
                                let mask_bytes = ((rw as usize + 7) / 8) * rh as usize;
                                ensure!(
                                    img_bytes + mask_bytes <= 16 * 1024 * 1024,
                                    "Cursor exceeds 16 MiB limit"
                                );
                                let mut skip = vec![0u8; img_bytes + mask_bytes];
                                stream.read_exact(&mut skip).await?;
                            }
                            other => {
                                bail!("Unsupported encoding {other} — cannot safely continue (stream would desync)");
                            }
                        }
                    }

                    if !dirty.is_empty() {
                        let rects: Vec<FbRect> = dirty
                            .into_iter()
                            .map(|(rx, ry, rw, rh)| {
                                let (w, h, rgba) =
                                    fb.sub_rgba(rx as u32, ry as u32, rw as u32, rh as u32);
                                FbRect {
                                    x: rx as u32,
                                    y: ry as u32,
                                    w,
                                    h,
                                    rgba,
                                }
                            })
                            .collect();
                        let _ = event_tx.send(VncEvent::FramebufferRects(rects));
                    }

                    // Resizing discards old pixels. Request one full refresh, only after
                    // consuming the entire update, rather than also queuing an incremental.
                    send_fb_update_request(
                        &mut stream,
                        !resized,
                        0,
                        0,
                        fb.width as u16,
                        fb.height as u16,
                    )
                    .await?;
                }

                server_msg::SET_COLOUR_MAP_ENTRIES => {
                    stream.read_u8().await?;
                    let _first = stream.read_u16().await?;
                    let count = stream.read_u16().await?;
                    let mut skip = vec![0u8; count as usize * 6];
                    stream.read_exact(&mut skip).await?;
                }

                server_msg::BELL => {
                    debug!("Bell!");
                }

                server_msg::SERVER_CUT_TEXT => {
                    if let Some(text) = clipboard.receive(&mut stream).await? {
                        let _ = event_tx.send(VncEvent::ClipboardText(text));
                    }
                }

                other => {
                    bail!("Unknown server message type: {other}");
                }
            }
        }
    }
}

/// RFC 6143 requires unknown 3.x versions to use the 3.3 handshake. The
/// explicitly recognised 003.889 vendor banner can negotiate standard 3.8;
/// this does not enable any Apple private authentication or encoding.
fn negotiate_version(banner: &[u8; 12]) -> Result<u8> {
    ensure!(
        banner.starts_with(b"RFB ")
            && banner[7] == b'.'
            && banner[11] == b'\n'
            && banner[4..7]
                .iter()
                .chain(&banner[8..11])
                .all(u8::is_ascii_digit),
        "Malformed RFB protocol version banner"
    );
    ensure!(&banner[4..7] == b"003", "Unsupported RFB major version");
    match &banner[8..11] {
        b"008" | b"889" => Ok(8),
        b"007" => Ok(7),
        minor if minor >= b"003".as_slice() => Ok(3),
        _ => bail!("RFB versions earlier than 3.3 are unsupported"),
    }
}

fn validate_framebuffer_size(width: u16, height: u16) -> Result<()> {
    ensure!(width > 0 && height > 0, "Empty framebuffer dimensions");
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_FRAMEBUFFER_PIXELS,
        "Framebuffer exceeds 32 megapixel limit"
    );
    Ok(())
}

fn validate_rectangle(fb: &Framebuffer, x: u16, y: u16, w: u16, h: u16) -> Result<()> {
    ensure!(
        w > 0
            && h > 0
            && u32::from(x) + u32::from(w) <= fb.width
            && u32::from(y) + u32::from(h) <= fb.height,
        "Rectangle is outside the framebuffer"
    );
    Ok(())
}

async fn send_pixel_format<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    pf: &PixelFormat,
) -> Result<()> {
    stream
        .write_all(&[
            crate::protocol::messages::client_msg::SET_PIXEL_FORMAT,
            0,
            0,
            0,
        ])
        .await?;
    stream.write_all(&pf.wire_bytes()).await?;
    Ok(())
}

async fn send_fb_update_request<S>(
    stream: &mut S,
    incremental: bool,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut msg = [0u8; 10];
    msg[0] = crate::protocol::messages::client_msg::FB_UPDATE_REQUEST;
    msg[1] = if incremental { 1 } else { 0 };
    msg[2..4].copy_from_slice(&x.to_be_bytes());
    msg[4..6].copy_from_slice(&y.to_be_bytes());
    msg[6..8].copy_from_slice(&w.to_be_bytes());
    msg[8..10].copy_from_slice(&h.to_be_bytes());
    stream.write_all(&msg).await?;
    Ok(())
}

async fn send_key_event<S>(stream: &mut S, down: bool, keysym: u32) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut msg = [0u8; 8];
    msg[0] = crate::protocol::messages::client_msg::KEY_EVENT;
    msg[1] = if down { 1 } else { 0 };
    msg[4..8].copy_from_slice(&keysym.to_be_bytes());
    stream.write_all(&msg).await?;
    Ok(())
}

async fn send_pointer_event<S>(stream: &mut S, buttons: u8, x: u16, y: u16) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut msg = [0u8; 6];
    msg[0] = crate::protocol::messages::client_msg::POINTER_EVENT;
    msg[1] = buttons;
    msg[2..4].copy_from_slice(&x.to_be_bytes());
    msg[4..6].copy_from_slice(&y.to_be_bytes());
    stream.write_all(&msg).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    mod desktop_size;

    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    async fn handshake(server: &mut TcpStream, banner: &[u8; 12], minor: u8) {
        server.write_all(banner).await.unwrap();
        let mut reply = [0; 12];
        server.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, format!("RFB 003.00{minor}\n").as_bytes());
        if minor == 3 {
            server.write_u32(1).await.unwrap();
        } else {
            server.write_all(&[1, 1]).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), 1);
            if minor == 8 {
                server.write_u32(0).await.unwrap();
            }
        }
        assert_eq!(server.read_u8().await.unwrap(), 1);
    }

    async fn server_init(server: &mut TcpStream, width: u16, height: u16, name_len: u32) {
        server.write_u16(width).await.unwrap();
        server.write_u16(height).await.unwrap();
        // Deliberately advertise indexed colour (zero maxima). Decoders must
        // only use the format subsequently requested by SetPixelFormat.
        server
            .write_all(&[8, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        server.write_u32(name_len).await.unwrap();
        if name_len == 7 {
            server.write_all(b"fixture").await.unwrap();
        }
    }

    async fn read_setup(server: &mut TcpStream) {
        let mut set_format = [0; 20];
        server.read_exact(&mut set_format).await.unwrap();
        assert_eq!(&set_format[..4], &[0, 0, 0, 0]);
        assert_eq!(
            &set_format[4..],
            &[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
        );
        assert_eq!(server.read_u8().await.unwrap(), 2);
        assert_eq!(server.read_u8().await.unwrap(), 0);
        let count = server.read_u16().await.unwrap();
        let mut encodings = Vec::new();
        for _ in 0..count {
            encodings.push(server.read_i32().await.unwrap());
        }
        assert!(encodings.contains(&enc::RAW));
        assert!(encodings.contains(&enc::EXTENDED_CLIPBOARD));
        let mut request = [0; 10];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 2, 0, 1]);
    }

    fn params(port: u16) -> ConnectionParams {
        ConnectionParams {
            host: "127.0.0.1".into(),
            port,
            username: None,
            password: None,
            source_session: None,
        }
    }

    async fn next_event(receiver: &Receiver<VncEvent>) -> VncEvent {
        loop {
            if let Ok(event) = receiver.try_recv() {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[test]
    fn version_banners_are_strict_and_unknown_3x_uses_rfc_fallback() {
        for (banner, expected) in [
            (b"RFB 003.003\n", 3),
            (b"RFB 003.007\n", 7),
            (b"RFB 003.008\n", 8),
            (b"RFB 003.889\n", 8),
            (b"RFB 003.009\n", 3),
            (b"RFB 003.006\n", 3),
        ] {
            assert_eq!(negotiate_version(banner).unwrap(), expected);
        }
        for banner in [
            b"RFB 004.001\n",
            b"RFB 003.002\n",
            b"RFB 003.008!",
            b"RFB 00x.008\n",
            b"XYZ 003.008\n",
        ] {
            assert!(negotiate_version(banner).is_err());
        }
    }

    #[test]
    fn framebuffer_and_rectangle_limits_reject_overflowing_coordinates() {
        assert!(validate_framebuffer_size(7680, 4320).is_ok());
        assert!(validate_framebuffer_size(65535, 65535).is_err());
        assert!(validate_framebuffer_size(0, 1).is_err());
        let fb = Framebuffer::new(2, 1);
        assert!(validate_rectangle(&fb, 0, 0, 2, 1).is_ok());
        assert!(validate_rectangle(&fb, u16::MAX, 0, 2, 1).is_err());
        assert!(validate_rectangle(&fb, 1, 0, 2, 1).is_err());
    }

    #[tokio::test]
    async fn real_client_negotiates_all_supported_banners_and_decodes_raw_and_latin1() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for (banner, minor) in [
                (b"RFB 003.003\n", 3), (b"RFB 003.007\n", 7),
                (b"RFB 003.008\n", 8), (b"RFB 003.889\n", 8),
            ] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let connection = params(listener.local_addr().unwrap().port());
                let (events_tx, events_rx) = crossbeam_channel::unbounded();
                let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
                let disconnect = commands_tx.clone();
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    handshake(&mut stream, banner, minor).await;
                    server_init(&mut stream, 2, 1, 7).await;
                    read_setup(&mut stream).await;
                    stream.write_all(&[
                        0, 0, 0, 1, // update, one Raw rectangle
                        0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0,
                        0, 0, 255, 0, 255, 0, 0, 0,
                    ]).await.unwrap();
                    let mut incremental = [0; 10];
                    stream.read_exact(&mut incremental).await.unwrap();
                    assert_eq!(incremental, [3, 1, 0, 0, 0, 0, 0, 2, 0, 1]);
                    stream.write_all(&[3, 0, 0, 0, 0, 0, 0, 2, 0xe9, 0xff]).await.unwrap();
                    let mut cut_text = [0; 10];
                    stream.read_exact(&mut cut_text).await.unwrap();
                    assert_eq!(cut_text, [6, 0, 0, 0, 0, 0, 0, 2, 0xe9, b'?']);
                    disconnect.send(VncCommand::Disconnect).unwrap();
                    // Keep the socket alive until the client has consumed the command.
                    assert!(stream.read_u8().await.is_err());
                });
                // The production client runs on a current-thread runtime.
                // Keep its future local: security negotiation need not be Send.
                let client = VncClient::connect(&connection, events_tx, commands_rx);
                let observe = async {
                    assert!(matches!(next_event(&events_rx).await, VncEvent::DesktopSize(2, 1)));
                    assert!(matches!(next_event(&events_rx).await, VncEvent::DesktopName(name) if name == "fixture"));
                    match next_event(&events_rx).await {
                        VncEvent::FramebufferRects(rects) => {
                            assert_eq!(rects.len(), 1);
                            assert_eq!(rects[0].rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
                        }
                        _ => panic!("Expected decoded framebuffer"),
                    }
                    assert!(matches!(next_event(&events_rx).await, VncEvent::ClipboardText(text) if text == "éÿ"));
                    commands_tx.send(VncCommand::SetClipboard("é🙂".into())).unwrap();
                };
                let (result, ()) = tokio::join!(client, observe);
                result.unwrap();
                server.await.unwrap();
            }
        }).await.expect("loopback fixture timed out");
    }

    #[tokio::test]
    async fn real_client_rejects_out_of_bounds_copyrect_without_panicking() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let connection = params(listener.local_addr().unwrap().port());
            let (events_tx, _) = crossbeam_channel::unbounded();
            let (_commands_tx, commands_rx) = crossbeam_channel::unbounded();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                handshake(&mut stream, b"RFB 003.008\n", 8).await;
                server_init(&mut stream, 2, 1, 7).await;
                read_setup(&mut stream).await;
                stream
                    .write_all(&[
                        0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 1, 0xff, 0xff, 0, 0,
                    ])
                    .await
                    .unwrap();
                assert!(stream.read_u8().await.is_err());
            });
            let error = VncClient::connect(&connection, events_tx, commands_rx)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("outside the framebuffer"));
            server.await.unwrap();
        })
        .await
        .expect("CopyRect fixture timed out");
    }
}
