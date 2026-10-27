// On Windows, release builds use the GUI subsystem so launching the app does
// NOT pop up a console window. Debug builds keep the console so developers see
// logs. CLI subcommands (--test, --sftp-test) still work when their output is
// redirected to a file or pipe, which is how they're normally captured.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod connection;
mod framebuffer;
mod protocol;
mod sessions;
mod transfer;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "ironvnc",
    about = "IronVNC — a free, open-source VNC viewer written in Rust"
)]
struct Args {
    /// VNC server host
    #[arg(short = 'H', long)]
    host: Option<String>,

    /// VNC server port
    #[arg(short, long, default_value_t = 5900)]
    port: u16,

    /// VNC password
    #[arg(short = 'P', long)]
    password: Option<String>,

    /// VNC username (for servers requesting username/password authentication).
    #[arg(short = 'U', long)]
    username: Option<String>,

    /// Headless test mode: connect, wait for the first framebuffer, report, exit.
    #[arg(long, default_value_t = false)]
    test: bool,

    /// Headless SFTP test: connect over SSH and list the home directory.
    #[arg(long, default_value_t = false)]
    sftp_test: bool,

    /// SSH username (for --sftp-test).
    #[arg(long)]
    user: Option<String>,

    /// Open a saved session by name, in this window.
    #[arg(long)]
    session: Option<String>,

    /// Internal selector for opening one saved session in a new window.
    #[arg(long, hide = true, conflicts_with = "session")]
    session_id: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    if args.test {
        let host = args
            .host
            .ok_or_else(|| anyhow::anyhow!("--test requires --host"))?;
        return headless_test(host, args.port, args.username, args.password);
    }

    if args.sftp_test {
        let host = args
            .host
            .ok_or_else(|| anyhow::anyhow!("--sftp-test requires --host"))?;
        let user = args
            .user
            .ok_or_else(|| anyhow::anyhow!("--sftp-test requires --user"))?;
        return sftp_test(host, user, args.password.unwrap_or_default());
    }

    // Resolve the initial connection. `--session <name>` opens a saved session
    // (used when launching a session in its own window). Otherwise `--host`;
    // reuse saved credentials only when host:port and the optional username
    // select a single saved session.
    let saved_sessions = sessions::load();
    let selected_session = if let Some(id) = args.session_id.as_deref() {
        Some(sessions::find_by_id(&saved_sessions, id)?)
    } else if let Some(name) = args.session.as_deref() {
        Some(sessions::find_named(&saved_sessions, name)?)
    } else {
        None
    };
    let initial_connection = if let Some(saved) = selected_session {
        Some(connection::ConnectionParams {
            host: saved.host.clone(),
            port: saved.port,
            username: args.username.clone().or_else(|| saved.username.clone()),
            password: args
                .password
                .clone()
                .or_else(|| (!saved.password.is_empty()).then(|| saved.password.clone())),
            source_session: Some(saved.identity()),
        })
    } else {
        args.host.clone().map(|host| {
            let saved =
                sessions::for_endpoint(&saved_sessions, &host, args.port, args.username.as_deref());
            let username = args
                .username
                .clone()
                .or_else(|| saved.and_then(|s| s.username.clone()));
            let password = args.password.clone().or_else(|| {
                saved
                    .filter(|s| !s.password.is_empty())
                    .map(|s| s.password.clone())
            });
            connection::ConnectionParams {
                host,
                port: args.port,
                username,
                password,
                source_session: saved.map(|s| s.identity()),
            }
        })
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("IronVNC")
            .with_inner_size([1100.0, 750.0]),
        ..Default::default()
    };

    eframe::run_native(
        "IronVNC",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::VncApp::new(cc, initial_connection)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

/// Connect over SSH/SFTP without a GUI and list the home directory.
fn sftp_test(host: String, user: String, password: String) -> Result<()> {
    use std::time::{Duration, Instant};
    use transfer::{TransferEvent, TransferParams, TransferSession};

    println!("[sftp] connecting to {user}@{host}:22 ...");
    let sess = TransferSession::connect(TransferParams {
        host,
        port: 22,
        username: user,
        password,
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match sess.event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(TransferEvent::Connected { home }) => {
                println!("[sftp] authenticated; home = {home}");
            }
            Ok(TransferEvent::DirListing { path, entries }) => {
                println!("[sftp] PASS: listed {path} ({} entries):", entries.len());
                for e in entries.iter().take(20) {
                    let kind = if e.is_dir { "dir " } else { "file" };
                    println!("        [{kind}] {} ({} B)", e.name, e.size);
                }
                sess.disconnect();
                return Ok(());
            }
            Ok(TransferEvent::Status(s)) => println!("[sftp] {s}"),
            Ok(TransferEvent::Error(e)) => {
                println!("[sftp] FAIL: {e}");
                std::process::exit(1);
            }
            Ok(TransferEvent::TransferDone { .. }) => {}
            Ok(TransferEvent::Disconnected) => {
                println!("[sftp] FAIL: disconnected before listing");
                std::process::exit(1);
            }
            Err(_) => {}
        }
    }
    println!("[sftp] FAIL: timed out");
    std::process::exit(1);
}

/// Connect without a GUI, wait for the first framebuffer update, and report:
/// desktop name/size and how much of the first frame is non-black. Black frames
/// are valid; this smoke test never injects remote input.
fn headless_test(
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
) -> Result<()> {
    use connection::{VncConnection, VncEvent};
    use std::time::{Duration, Instant};

    println!("[test] connecting to {host}:{port} ...");
    let conn = VncConnection::connect(connection::ConnectionParams {
        host,
        port,
        username,
        password,
        source_session: None,
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut got_size = None;
    let mut got_name: Option<String> = None;
    let mut fb_buf: Vec<u8> = Vec::new();

    while Instant::now() < deadline {
        match conn.event_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(VncEvent::DesktopSize(w, h)) => {
                got_size = Some((w, h));
                println!("[test] desktop size: {w}x{h}");
            }
            Ok(VncEvent::DesktopName(n)) => {
                println!("[test] desktop name: {n:?}");
                got_name = Some(n);
            }
            Ok(VncEvent::NeedPassword) => {
                println!("[test] FAIL: server asked for a password but none was supplied");
                conn.disconnect();
                std::process::exit(2);
            }
            Ok(VncEvent::NeedCredentials {
                needs_username,
                needs_password,
                ..
            }) => {
                println!("[test] FAIL: missing credentials (username: {needs_username}, password: {needs_password})");
                conn.disconnect();
                std::process::exit(2);
            }
            Ok(VncEvent::FramebufferRects(rects)) => {
                // Apply rects into a local buffer to measure whole-screen content.
                let (fw, fh) = got_size.unwrap_or((0, 0));
                let Some(total) = (fw as usize).checked_mul(fh as usize) else {
                    anyhow::bail!("Framebuffer dimensions exceed addressable memory");
                };
                let Some(frame_len) = total.checked_mul(4) else {
                    anyhow::bail!("Framebuffer dimensions exceed addressable memory");
                };
                if fb_buf.len() != frame_len {
                    fb_buf = vec![0u8; frame_len];
                }
                let mut valid_rect = false;
                for r in &rects {
                    if r.w == 0
                        || r.h == 0
                        || r.x.checked_add(r.w).is_none_or(|x| x > fw)
                        || r.y.checked_add(r.h).is_none_or(|y| y > fh)
                        || (r.w as usize)
                            .checked_mul(r.h as usize)
                            .and_then(|n| n.checked_mul(4))
                            != Some(r.rgba.len())
                    {
                        continue;
                    }
                    valid_rect = true;
                    for row in 0..r.h {
                        let src = row as usize * r.w as usize * 4;
                        let dst = ((r.y + row) as usize * fw as usize + r.x as usize) * 4;
                        let n = r.w as usize * 4;
                        if dst + n <= fb_buf.len() && src + n <= r.rgba.len() {
                            fb_buf[dst..dst + n].copy_from_slice(&r.rgba[src..src + n]);
                        }
                    }
                }
                if !valid_rect {
                    continue;
                }
                let non_black = fb_buf
                    .chunks_exact(4)
                    .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
                    .count();
                let pct = if total > 0 {
                    non_black * 100 / total
                } else {
                    0
                };
                println!(
                    "[test] frame 1: {fw}x{fh}, {non_black}/{total} non-black pixels ({pct}%)"
                );
                println!(
                    "[test] PASS: connected to {:?} ({fw}x{fh}), first framebuffer decoded \
                     ({non_black} non-black px, {pct}%)",
                    got_name.as_deref().unwrap_or("?"),
                );
                conn.disconnect();
                return Ok(());
            }
            Ok(VncEvent::UpdateComplete(permit)) => drop(permit),
            Ok(VncEvent::ClipboardText(_) | VncEvent::ClipboardStatus { .. }) => {}
            Ok(VncEvent::Error(e)) => {
                println!("[test] FAIL: {e}");
                std::process::exit(1);
            }
            Ok(VncEvent::Disconnected) => {
                println!("[test] FAIL: disconnected before first frame");
                std::process::exit(1);
            }
            Err(_) => {} // poll timeout, keep waiting
        }
    }
    println!("[test] FAIL: timed out after 30s with no frames");
    conn.disconnect();
    std::process::exit(1);
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn vnc_username_and_ssh_user_are_distinct_options() {
        let args = Args::try_parse_from([
            "ironvnc",
            "--test",
            "-H",
            "localhost",
            "-U",
            "vnc-user",
            "--user",
            "ssh-user",
        ])
        .unwrap();
        assert_eq!(args.username.as_deref(), Some("vnc-user"));
        assert_eq!(args.user.as_deref(), Some("ssh-user"));
        assert!(args.test);
    }

    #[test]
    fn session_id_selector_is_hidden_and_exclusive_with_session_name() {
        let args = Args::try_parse_from(["ironvnc", "--session-id", "bob-id"]).unwrap();
        assert_eq!(args.session_id.as_deref(), Some("bob-id"));
        assert!(
            Args::try_parse_from(["ironvnc", "--session-id", "bob-id", "--session", "Bob",])
                .is_err()
        );
        let help = Args::try_parse_from(["ironvnc", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("--session-id"));
    }
}
