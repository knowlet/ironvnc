//! Exercise the actual UI event handler without creating a native window/GPU.
use super::*;
use crate::connection::{FbRect, VncCommand};
use std::sync::Arc;
use tokio::sync::Semaphore;

fn pixel(x: u32, rgba: [u8; 4]) -> VncEvent {
    VncEvent::FramebufferRects(vec![FbRect {
        x,
        y: 0,
        w: 1,
        h: 1,
        rgba: rgba.to_vec(),
    }])
}

fn texture_pixels(delta: &egui::TexturesDelta, id: egui::TextureId) -> Vec<Color32> {
    let mut pixels = Vec::new();
    let mut width = 0;
    for (_, change) in delta.set.iter().filter(|(texture, _)| *texture == id) {
        let egui::ImageData::Color(image) = &change.image else {
            panic!("expected color texture")
        };
        if let Some([x, y]) = change.pos {
            for row in 0..image.height() {
                let start = (y + row) * width + x;
                pixels[start..start + image.width()]
                    .copy_from_slice(&image.pixels[row * image.width()..(row + 1) * image.width()]);
            }
        } else {
            width = image.width();
            pixels = image.pixels.clone();
        }
    }
    pixels
}

#[test]
fn actual_ui_prepares_pixels_in_fifo_order_before_releasing_update_fences() {
    let ctx = egui::Context::default();
    let mut app = VncApp::from_initial(None);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, commands) = crossbeam_channel::unbounded();
    app.connection = Some(VncConnection::from_channels(event_rx, cmd_tx));
    let credits = Arc::new(Semaphore::new(2));
    let first = credits.clone().try_acquire_owned().unwrap();
    let second = credits.clone().try_acquire_owned().unwrap();
    event_tx
        .send(VncEvent::DesktopSize(2, 1))
        .unwrap_or_else(|_| panic!("send size"));
    event_tx
        .send(pixel(0, [255, 0, 0, 255]))
        .unwrap_or_else(|_| panic!("send pixels"));
    assert_eq!(credits.available_permits(), 0);
    ctx.begin_pass(Default::default());
    app.process_events(&ctx);
    // Merely dequeuing/preparing rectangle events cannot release a fence that
    // has not arrived yet. Verify the actual ColorImage/set_partial output.
    assert_eq!(credits.available_permits(), 0);
    let old_texture = app.texture.as_ref().unwrap().id();
    let output = ctx.end_pass();
    assert_eq!(
        texture_pixels(&output.textures_delta, old_texture),
        [Color32::RED, Color32::BLACK]
    );

    event_tx
        .send(VncEvent::UpdateComplete(first))
        .unwrap_or_else(|_| panic!("send first fence"));
    event_tx
        .send(VncEvent::DesktopSize(3, 1))
        .unwrap_or_else(|_| panic!("send resize"));
    event_tx
        .send(pixel(1, [0, 0, 255, 255]))
        .unwrap_or_else(|_| panic!("send later pixels"));
    event_tx
        .send(VncEvent::UpdateComplete(second))
        .unwrap_or_else(|_| panic!("send second fence"));
    event_tx
        .send(VncEvent::ClipboardStatus {
            message: "control after pixels".into(),
            is_error: false,
        })
        .unwrap_or_else(|_| panic!("send control"));
    assert_eq!(credits.available_permits(), 0);
    ctx.begin_pass(Default::default());
    app.process_events(&ctx);
    assert_eq!(credits.available_permits(), 2);
    assert_eq!((app.fb_width, app.fb_height), (3, 1));
    assert_eq!(app.status_msg, "control after pixels");
    let new_texture = app.texture.as_ref().unwrap().id();
    let output = ctx.end_pass();
    assert_ne!(old_texture, new_texture);
    assert!(output.textures_delta.free.contains(&old_texture));
    assert_eq!(
        texture_pixels(&output.textures_delta, new_texture),
        [Color32::BLACK, Color32::BLUE, Color32::BLACK]
    );

    // Empty/resize-only completion and terminal events do not leak credits.
    let last = credits.clone().try_acquire_owned().unwrap();
    event_tx
        .send(VncEvent::UpdateComplete(last))
        .unwrap_or_else(|_| panic!("send final fence"));
    event_tx
        .send(VncEvent::Disconnected)
        .unwrap_or_else(|_| panic!("send disconnect"));
    app.process_events(&ctx);
    assert_eq!(credits.available_permits(), 2);
    assert!(app.connection.is_none() && app.texture.is_none());
    assert!(matches!(commands.try_recv(), Ok(VncCommand::Disconnect)));
}

#[test]
fn actual_ui_retains_fences_for_the_entire_collected_snapshot() {
    let ctx = egui::Context::default();
    // Consume egui's initial/font repaint passes before installing the probe;
    // otherwise a zero-delay repaint is already pending and is coalesced.
    for _ in 0..2 {
        ctx.begin_pass(Default::default());
        drop(ctx.end_pass());
    }
    let mut app = VncApp::from_initial(None);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _commands) = crossbeam_channel::unbounded();
    app.connection = Some(VncConnection::from_channels(event_rx, cmd_tx));
    let credits = Arc::new(Semaphore::new(2));
    let first = credits.clone().try_acquire_owned().unwrap();
    let second = credits.clone().try_acquire_owned().unwrap();
    for event in [
        VncEvent::DesktopSize(2, 1),
        pixel(0, [255, 0, 0, 255]),
        VncEvent::UpdateComplete(first),
        pixel(1, [0, 255, 0, 255]),
        VncEvent::UpdateComplete(second),
    ] {
        event_tx
            .send(event)
            .unwrap_or_else(|_| panic!("send update batch"));
    }
    assert_eq!(credits.available_permits(), 0);
    // A repaint is requested by the real rectangle handler before its fence.
    // No credit may be released at collection time, before that preparation.
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_callback = observed.clone();
    let credits_callback = credits.clone();
    ctx.set_request_repaint_callback(move |_| {
        if !observed_callback.swap(true, std::sync::atomic::Ordering::SeqCst) {
            assert_eq!(
                credits_callback.available_permits(),
                0,
                "fences released while collecting events"
            );
        }
    });
    ctx.begin_pass(Default::default());
    // Clear any begin-pass repaint notification before exercising the handler.
    observed.store(false, std::sync::atomic::Ordering::SeqCst);
    app.process_events(&ctx);
    assert!(
        observed.load(std::sync::atomic::Ordering::SeqCst),
        "repaint observation did not run"
    );
    assert_eq!(credits.available_permits(), 2);
    let texture = app.texture.as_ref().unwrap().id();
    let output = ctx.end_pass();
    assert_eq!(
        texture_pixels(&output.textures_delta, texture),
        [Color32::RED, Color32::GREEN]
    );
}

#[test]
fn profiling_metadata_does_not_release_credits_or_change_texture_pixels() {
    use crate::profiling::UpdateProfile;
    use std::time::{Duration, Instant};
    let ctx = egui::Context::default();
    let mut app = VncApp::with_sessions(None, Vec::new());
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _commands) = crossbeam_channel::unbounded();
    app.connection = Some(VncConnection::from_channels(event_rx, cmd_tx));
    let credits = Arc::new(Semaphore::new(2));
    let permit = credits.clone().try_acquire_owned().unwrap();
    event_tx.send(VncEvent::DesktopSize(1, 1)).unwrap();
    event_tx
        .send(VncEvent::ProfileUpdate(UpdateProfile {
            sequence: 1,
            started: Instant::now(),
            queued: Instant::now(),
            receive: Duration::ZERO,
            decode_apply: Duration::ZERO,
            framebuffer: Duration::ZERO,
            framebuffer_complete: true,
            snapshot: Duration::ZERO,
            credit_wait: Duration::ZERO,
            payload_bytes: 4,
        }))
        .unwrap();
    ctx.begin_pass(Default::default());
    app.process_events(&ctx);
    drop(ctx.end_pass());
    assert_eq!(credits.available_permits(), 1);
    event_tx.send(pixel(0, [255, 0, 0, 255])).unwrap();
    event_tx.send(VncEvent::UpdateComplete(permit)).unwrap();
    ctx.begin_pass(Default::default());
    app.process_events(&ctx);
    let out = ctx.end_pass();
    let image = &out.textures_delta.set.last().unwrap().1.image;
    let egui::ImageData::Color(image) = image else {
        panic!("color expected")
    };
    assert_eq!(image.pixels, [Color32::RED]);
    assert_eq!(credits.available_permits(), 2);
    assert!(app.profile.active.is_none());
}

#[test]
fn diagnostic_app_does_not_remember_fixture_sessions() {
    let mut app = VncApp::with_sessions(None, Vec::new());
    app.persist_sessions = false;
    app.current_host = "127.0.0.1".into();
    app.current_port = 5900;
    app.desktop_name = "synthetic fixture".into();
    app.remember_session();
    assert!(app.sessions.is_empty());
}

fn session_editor(count: usize, editing: usize) -> VncApp {
    let saved = (0..count)
        .map(|i| Session {
            id: format!("fixture-id-{i}"),
            name: format!("Session {i}"),
            host: format!("desktop-{i}.invalid"),
            port: 5900 + i as u16,
            username: Some(format!("vnc-user-{i}")),
            password: format!("dummy-password-{i}"),
            ssh_user: format!("ssh-user-{i}"),
        })
        .collect();
    let mut app = VncApp::with_sessions(None, saved);
    app.persist_sessions = false;
    let s = &app.sessions[editing];
    app.sf_name = s.name.clone();
    app.sf_host = s.host.clone();
    app.sf_port = s.port.to_string();
    app.sf_username = s.username.clone().unwrap();
    app.sf_password = s.password.clone();
    app.session_selected = Some(editing);
    app.sf_editing = Some(editing);
    app
}

#[test]
fn removing_an_earlier_session_keeps_edits_on_the_same_account() {
    let mut app = session_editor(3, 1);
    let mut edited = app.sessions[1].clone();
    let other = app.sessions[2].clone();
    app.sf_name = "Updated session".into();
    app.sf_password = "updated dummy password".into();
    edited.name = app.sf_name.clone();
    edited.password = app.sf_password.clone();
    app.remove_session(0);
    app.apply_session_form();
    assert_eq!(app.sessions, [edited, other]);
    assert_eq!(app.session_selected, Some(0));
    assert_eq!(app.sf_editing, Some(0));
}

#[test]
fn removing_an_earlier_session_keeps_the_last_session_editable() {
    let mut app = session_editor(2, 1);
    let mut expected = app.sessions[1].clone();
    app.sf_name = "Still editable".into();
    expected.name = app.sf_name.clone();
    app.remove_session(0);
    app.apply_session_form();
    assert_eq!(app.sessions, [expected]);
    assert_eq!(app.session_selected, Some(0));
    assert_eq!(app.sf_editing, Some(0));
}

#[test]
fn removing_a_later_session_keeps_the_current_editor() {
    let mut app = session_editor(3, 0);
    let other = app.sessions[1].clone();
    let mut edited = app.sessions[0].clone();
    app.sf_name = "Updated first".into();
    edited.name = app.sf_name.clone();
    app.remove_session(2);
    app.apply_session_form();
    assert_eq!(app.sessions, [edited, other]);
    assert_eq!(app.session_selected, Some(0));
    assert_eq!(app.sf_editing, Some(0));
}

#[test]
fn removing_the_edited_session_does_not_overwrite_its_neighbor() {
    let mut app = session_editor(3, 1);
    let first = app.sessions[0].clone();
    let last = app.sessions[2].clone();
    let removed_id = app.sessions[1].id.clone();
    app.remove_session(1);
    assert_eq!(app.session_selected, None);
    assert_eq!(app.sf_editing, None);
    // Existing behavior retains the form as a new, independently identified
    // session. Saving it must not replace either surviving account.
    app.apply_session_form();
    assert_eq!(app.sessions.len(), 3);
    assert_eq!(app.sessions[0], first);
    assert_eq!(app.sessions[1], last);
    assert_ne!(app.sessions[2].id, removed_id);
    assert!(app.sessions[2].ssh_user.is_empty());
}

#[test]
fn removing_sessions_adjusts_selection_and_editor_independently() {
    for selected in 0..3 {
        for editing in 0..3 {
            for removed in 0..3 {
                let mut app = session_editor(3, editing);
                app.session_selected = Some(selected);
                let selected_id = app.sessions[selected].id.clone();
                let editing_id = app.sessions[editing].id.clone();
                app.remove_session(removed);
                let locate = |id: &str| app.sessions.iter().position(|s| s.id == id);
                assert_eq!(app.session_selected, locate(&selected_id));
                assert_eq!(app.sf_editing, locate(&editing_id));
            }
        }
    }
}

#[test]
fn repeated_removals_preserve_the_new_session_form() {
    let mut app = session_editor(3, 2);
    app.session_selected = None;
    app.sf_editing = None;
    app.sf_name = "New draft".into();
    for index in [1, 0, 0] {
        app.remove_session(index);
        assert_eq!(app.session_selected, None);
        assert_eq!(app.sf_editing, None);
        assert_eq!(app.sf_name, "New draft");
    }
    app.apply_session_form();
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.sessions[0].name, "New draft");
}

#[test]
fn pending_session_connections_follow_the_remaining_account() {
    assert_eq!(VncApp::session_index_after_removal(None, 1), None);
    assert_eq!(VncApp::session_index_after_removal(Some(0), 1), Some(0));
    assert_eq!(VncApp::session_index_after_removal(Some(1), 1), None);
    assert_eq!(VncApp::session_index_after_removal(Some(2), 1), Some(1));
    assert_eq!(VncApp::session_index_after_removal(Some(0), 0), None);
    assert_eq!(VncApp::session_index_after_removal(Some(1), 0), Some(0));
}
