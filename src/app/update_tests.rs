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
