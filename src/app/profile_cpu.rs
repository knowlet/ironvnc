//! The actual UI CPU event path, without a window, graphics API, or saved sessions.
use super::*;
use crate::profiling::UpdateProfile;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SAMPLES: usize = 10_000;

pub(super) struct Active {
    profile: UpdateProfile,
    queue: Duration,
    conversion: Duration,
    manager: Duration,
    saw_pixels: bool,
}

#[derive(Default)]
pub(super) struct UiProfile {
    pub active: Option<Active>,
    samples: Vec<serde_json::Value>,
    completed: usize,
    dropped: usize,
}
impl UiProfile {
    pub fn start(&mut self, profile: UpdateProfile) {
        self.active = Some(Active {
            queue: profile.queued.elapsed(),
            profile,
            conversion: Duration::ZERO,
            manager: Duration::ZERO,
            saw_pixels: false,
        });
    }
    pub fn pixels_start(&mut self) {
        if let Some(active) = &mut self.active {
            if !active.saw_pixels {
                active.queue = active.profile.queued.elapsed();
                active.saw_pixels = true;
            }
        }
    }
    pub fn add_conversion(&mut self, start: Option<Instant>) {
        if let (Some(active), Some(start)) = (&mut self.active, start) {
            active.conversion += start.elapsed();
        }
    }
    pub fn add_manager(&mut self, start: Option<Instant>) {
        if let (Some(active), Some(start)) = (&mut self.active, start) {
            active.manager += start.elapsed();
        }
    }
    pub fn finish(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let complete = Instant::now();
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        self.completed += 1;
        if self.samples.len() == MAX_SAMPLES {
            self.dropped += 1;
            return;
        }
        let p = active.profile;
        self.samples.push(serde_json::json!({
            "type": "update", "sequence": p.sequence,
            "completed_unix_ns": unix.as_nanos() as u64,
            "receive_ns": p.receive.as_nanos() as u64,
            "decode_apply_ns": p.decode_apply.as_nanos() as u64,
            "snapshot_ns": p.snapshot.as_nanos() as u64,
            "framebuffer_ns": p.framebuffer.as_nanos() as u64,
            "framebuffer_complete": p.framebuffer_complete,
            "decode_residual_ns": p.framebuffer_complete.then(|| p.decode_apply.saturating_sub(p.receive).saturating_sub(p.framebuffer).as_nanos() as u64),
            "credit_wait_ns": p.credit_wait.as_nanos() as u64,
            "queue_ns": active.queue.as_nanos() as u64,
            "texture_conversion_ns": active.conversion.as_nanos() as u64,
            "texture_manager_ns": active.manager.as_nanos() as u64,
            "pipeline_ns": complete.duration_since(p.started).as_nanos() as u64,
            "payload_bytes": p.payload_bytes,
            "gpu_present_ns": serde_json::Value::Null,
        }));
    }
}

pub fn run_cpu_profile(
    host: String,
    port: u16,
    updates: usize,
    refresh_ms: u64,
    verify_pixels: bool,
) -> anyhow::Result<()> {
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|_| anyhow::anyhow!("profile host must be a numeric loopback address"))?;
    anyhow::ensure!(
        ip.is_loopback(),
        "CPU profiling accepts loopback fixtures only"
    );
    anyhow::ensure!(
        (1..=MAX_SAMPLES).contains(&updates),
        "profile updates must be 1..={MAX_SAMPLES}"
    );
    anyhow::ensure!(
        (1..=1000).contains(&refresh_ms),
        "profile refresh must be 1..=1000 ms"
    );
    // Set before spawning the connection worker; no login or session lookup.
    std::env::set_var("IRONVNC_PROFILE", "1");
    let ctx = egui::Context::default();
    let mut app = VncApp::with_sessions(None, Vec::new());
    app.persist_sessions = false;
    app.do_connect(ConnectionParams {
        host,
        port,
        username: None,
        password: None,
        source_session: None,
    });
    let started = Instant::now();
    let deadline =
        Duration::from_secs(60).max(Duration::from_millis(refresh_ms * updates as u64 * 4));
    let interval = Duration::from_millis(refresh_ms);
    let mut oracle = PixelOracle::default();
    while app.profile.completed < updates && started.elapsed() < deadline {
        let pass = Instant::now();
        ctx.begin_pass(Default::default());
        app.process_events(&ctx);
        // Drain and immediately drop texture deltas exactly once per pass. This is
        // a CPU consumer, NOT a GPU renderer or display-frame measurement.
        let output = ctx.end_pass();
        if verify_pixels {
            if let Some(texture) = &app.texture {
                oracle.apply(&output.textures_delta, texture.id());
            }
        }
        drop(output);
        if app.connection.is_none() || app.show_password_dialog {
            break;
        }
        if let Some(remaining) = interval.checked_sub(pass.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
    let elapsed = started.elapsed();
    app.connection = None;
    if verify_pixels {
        println!("{}", oracle.result());
    }
    for sample in &app.profile.samples {
        println!("{sample}");
    }
    println!(
        "{}",
        serde_json::json!({"type":"summary", "expected":updates,
        "completed":app.profile.completed, "samples_dropped":app.profile.dropped,
        "elapsed_ns":elapsed.as_nanos() as u64, "gpu_verified":false,
        "status":app.status_msg})
    );
    anyhow::ensure!(
        app.profile.completed == updates && app.profile.dropped == 0,
        "CPU profile incomplete: {}/{} updates ({})",
        app.profile.completed,
        updates,
        app.status_msg
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_rejects_remote_hosts_and_invalid_limits_before_connecting() {
        assert!(run_cpu_profile("192.0.2.1".into(), 5900, 1, 16, false).is_err());
        assert!(run_cpu_profile("localhost".into(), 5900, 1, 16, false).is_err());
        assert!(run_cpu_profile("127.0.0.1".into(), 5900, MAX_SAMPLES + 1, 16, false).is_err());
        assert!(run_cpu_profile("127.0.0.1".into(), 5900, 1, 0, false).is_err());
    }
}

// Independent CPU reconstruction of egui output for separate correctness runs.
// Never enabled in timed comparisons: its allocations/copies change the workload.
#[derive(Default)]
struct PixelOracle {
    width: usize,
    height: usize,
    pixels: Vec<Color32>,
}
impl PixelOracle {
    fn apply(&mut self, deltas: &egui::TexturesDelta, texture: egui::TextureId) {
        for (_, delta) in deltas.set.iter().filter(|(id, _)| *id == texture) {
            let egui::ImageData::Color(image) = &delta.image else {
                continue;
            };
            if let Some([x, y]) = delta.pos {
                for row in 0..image.height() {
                    let start = (y + row) * self.width + x;
                    self.pixels[start..start + image.width()].copy_from_slice(
                        &image.pixels[row * image.width()..(row + 1) * image.width()],
                    );
                }
            } else {
                self.width = image.width();
                self.height = image.height();
                self.pixels = image.pixels.clone();
            }
        }
    }
    fn result(&self) -> serde_json::Value {
        let mut hash = 0xcbf29ce484222325u64;
        for pixel in &self.pixels {
            for byte in pixel.to_array() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        serde_json::json!({"type":"pixels","width":self.width,"height":self.height,"fnv1a64":format!("{hash:016x}")})
    }
}
