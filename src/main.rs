// On Windows, release builds use the GUI subsystem so launching the app does
// NOT pop up a console window. Debug builds keep the console so developers see
// logs. CLI subcommands (--test, --sftp-test) still work when their output is
// redirected to a file or pipe, which is how they're normally captured.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod connection;
mod framebuffer;
mod profiling;
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

    /// CPU-only pipeline diagnostic against an explicit loopback fixture (no GPU).
    #[arg(long, default_value_t = false, conflicts_with_all = ["test", "sftp_test", "session", "session_id", "password", "username"])]
    profile_cpu: bool,
    /// Separate correctness pass; extra CPU image storage invalidates performance comparisons.
    #[arg(long, default_value_t = false, requires = "profile_cpu")]
    profile_verify_pixels: bool,
    #[arg(long, default_value_t = 120)]
    profile_updates: usize,
    #[arg(long, default_value_t = 16)]
    profile_refresh_ms: u64,

    /// Headless SFTP test: connect over SSH and list the home directory.
    #[arg(long, default_value_t = false)]
    sftp_test: bool,

    /// SSH username (for --sftp-test).
    #[arg(long)]
    user: Option<String>,

    /// Open a saved session by name, in this window.
    #[arg(long, allow_hyphen_values = true)]
    session: Option<String>,

    /// Internal selector for opening one saved session in a new window.
    /// The name and account must still match that row.
    #[arg(
        long,
        hide = true,
        conflicts_with = "session",
        requires_all = ["session_name", "session_host", "session_port"]
    )]
    session_id: Option<String>,

    /// Saved name of the row the parent selected.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    session_name: Option<String>,

    /// Host of the row a parent window selected.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    session_host: Option<String>,

    /// Port of the row a parent window selected.
    #[arg(long, hide = true)]
    session_port: Option<u16>,

    /// VNC username of the row a parent window selected. Absent means none.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    session_username: Option<String>,

    /// The parent row had no unique ID. Do not store a password after auth.
    #[arg(long, hide = true, default_value_t = false)]
    no_session_writeback: bool,

    /// The parent row had an empty legacy ID. The loaded row must still have none.
    #[arg(long, hide = true, default_value_t = false)]
    expect_empty_id: bool,
}

fn launch_account(args: &Args) -> Result<sessions::LaunchAccount> {
    Ok(sessions::LaunchAccount {
        host: args
            .session_host
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--session-host is required"))?,
        port: args
            .session_port
            .ok_or_else(|| anyhow::anyhow!("--session-port is required"))?,
        username: args.session_username.clone(),
    })
}

fn saved_launch(args: &Args) -> Result<Option<sessions::SavedLaunch>> {
    if args.session_id.is_some() || args.session.is_some() {
        let account = if args.session_host.is_some() || args.session_id.is_some() {
            Some(launch_account(args)?)
        } else {
            None
        };
        return Ok(Some(sessions::SavedLaunch {
            id: args.session_id.clone(),
            key: args.session.clone(),
            expected_name: args.session_name.clone(),
            account,
            no_writeback: args.no_session_writeback,
            expect_empty_id: args.expect_empty_id,
        }));
    }
    Ok(None)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    if args.profile_cpu {
        let host = args
            .host
            .ok_or_else(|| anyhow::anyhow!("--profile-cpu requires --host"))?;
        return app::run_cpu_profile(
            host,
            args.port,
            args.profile_updates,
            args.profile_refresh_ms,
            args.profile_verify_pixels,
        );
    }

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
    let launched = saved_launch(&args)?;
    let selected_session = launched
        .as_ref()
        .map(|launch| sessions::open_saved_launch(&saved_sessions, launch))
        .transpose()?;
    let initial_connection = if let Some((saved, identity)) = selected_session {
        Some(connection::ConnectionParams {
            host: saved.host.clone(),
            port: saved.port,
            username: args.username.clone().or_else(|| saved.username.clone()),
            password: args
                .password
                .clone()
                .or_else(|| (!saved.password.is_empty()).then(|| saved.password.clone())),
            source_session: Some(identity),
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
                source_session: saved.map(|session| session.identity(&saved_sessions)),
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
            Ok(VncEvent::ProfileUpdate(_)) => {}
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
    fn session_id_selector_is_hidden_and_requires_name_and_account() {
        assert!(Args::try_parse_from(["ironvnc", "--session-id", "bob-id"]).is_err());
        let args = Args::try_parse_from([
            "ironvnc",
            "--session-id",
            "bob-id",
            "--session-name",
            "Bob",
            "--session-host",
            "desktop.example",
            "--session-port",
            "5900",
            "--session-username",
            "bob",
        ])
        .unwrap();
        assert_eq!(args.session_id.as_deref(), Some("bob-id"));
        assert_eq!(args.session_name.as_deref(), Some("Bob"));
        assert_eq!(args.session_host.as_deref(), Some("desktop.example"));
        assert_eq!(args.session_port, Some(5900));
        assert_eq!(args.session_username.as_deref(), Some("bob"));
        assert!(Args::try_parse_from([
            "ironvnc",
            "--session-id",
            "bob-id",
            "--session",
            "Bob",
            "--session-name",
            "Bob",
            "--session-host",
            "desktop.example",
            "--session-port",
            "5900",
        ])
        .is_err());
        let help = Args::try_parse_from(["ironvnc", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("--session-id"));
        assert!(!help.contains("--session-name"));
        assert!(!help.contains("--no-session-writeback"));
        assert!(!help.contains("--expect-empty-id"));
    }

    #[test]
    fn hyphenated_session_name_is_a_value_not_a_flag() {
        let args = Args::try_parse_from(["ironvnc", "--session", "-hidden-name"]).unwrap();
        assert_eq!(args.session.as_deref(), Some("-hidden-name"));
        let named = Args::try_parse_from([
            "ironvnc",
            "--session",
            "desktop.example",
            "--session-name",
            "-A",
            "--session-host",
            "-desktop",
            "--session-username",
            "-alice",
        ])
        .unwrap();
        assert_eq!(named.session_name.as_deref(), Some("-A"));
        assert_eq!(named.session_host.as_deref(), Some("-desktop"));
        assert_eq!(named.session_username.as_deref(), Some("-alice"));
    }

    #[test]
    fn shared_id_launch_args_keep_writeback_off_after_recreate() {
        let first = sessions::Session {
            id: "shared-id".into(),
            name: "A".into(),
            host: "desktop.example".into(),
            port: 5900,
            username: Some("shared-user".into()),
            password: "old secret".into(),
            ssh_user: String::new(),
        };
        let mut second = first.clone();
        second.name = "B".into();
        second.password = "other secret".into();
        let argv = sessions::session_window_args(&[first.clone(), second], &first).unwrap();
        let mut cli = vec!["ironvnc".to_string()];
        cli.extend(argv);
        let args = Args::try_parse_from(&cli).unwrap();
        assert!(args.no_session_writeback);
        assert_eq!(args.session_name.as_deref(), Some("A"));
        let launch = saved_launch(&args).unwrap().unwrap();
        let mut recreated = first;
        recreated.id.clear();
        let mut loaded = Vec::new();
        sessions::insert_session(&mut loaded, recreated);
        let (_opened, identity) = sessions::open_saved_launch(&loaded, &launch).unwrap();
        let before = loaded.clone();
        assert!(!sessions::remember_connection(
            &mut loaded,
            "desktop.example",
            5900,
            Some("shared-user".into()),
            "typed password",
            Some(&identity),
        ));
        assert_eq!(loaded, before);
    }

    #[test]
    fn empty_id_launch_args_reject_a_recreated_row() {
        let legacy = sessions::Session {
            id: String::new(),
            name: "A".into(),
            host: "desktop.example".into(),
            port: 5900,
            username: Some("alice".into()),
            password: "old secret".into(),
            ssh_user: String::new(),
        };
        let argv = sessions::session_window_args(std::slice::from_ref(&legacy), &legacy).unwrap();
        let mut cli = vec!["ironvnc".to_string()];
        cli.extend(argv);
        let args = Args::try_parse_from(&cli).unwrap();
        assert!(args.expect_empty_id);
        let launch = saved_launch(&args).unwrap().unwrap();
        let mut loaded = Vec::new();
        sessions::insert_session(&mut loaded, legacy);
        assert!(sessions::open_saved_launch(&loaded, &launch).is_err());
        assert_eq!(loaded[0].password, "old secret");
    }
}
