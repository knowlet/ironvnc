//! CPU-only full-frame dirty-update benchmark; not a VNC or renderer benchmark.
//! Includes the unchanged production Framebuffer and calls pinned egui APIs.
//! The boolean candidate branch models the small postdecode dirty-list helper;
//! production behavior is separately covered by real-client wire regressions.
use egui::{Color32, ColorImage, Context, TextureOptions};
use std::hint::black_box;
use std::time::Instant;
#[allow(dead_code)]
mod framebuffer {
    include!(env!("IRONVNC_FRAMEBUFFER_RS"));
}
use framebuffer::Framebuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
struct Rect {
    b: Bounds,
    rgba: Vec<u8>,
}
#[derive(Debug)]
struct Sample {
    extraction_us: f64,
    dequeue_us: f64,
    prep_us: f64,
    deltas: usize,
    bytes: usize,
}
fn elapsed_us(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e6
}

fn probe(fb: &Framebuffer, dirty: &[Bounds], collapse_full: bool, oracle: Option<&[u8]>) -> Sample {
    let ctx = Context::default();
    let mut tex = ctx.load_texture(
        "vnc-fb",
        ColorImage::new([fb.width as usize, fb.height as usize], Color32::BLACK),
        TextureOptions::LINEAR,
    );
    let _ = ctx.tex_manager().write().take_delta(); // initial black allocation not part of rectangle update
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<Rect>>();
    let mut selected = dirty.to_vec(); // production dirty-list construction happens in decoder, outside this measurement
    let start = Instant::now();
    // Experimental bounded hypothesis only: a full-frame rectangle covers every
    // dirty region, and all payloads are snapshots of final decoder state.
    let full = Bounds {
        x: 0,
        y: 0,
        w: fb.width,
        h: fb.height,
    };
    if collapse_full && selected.len() > 1 && selected.contains(&full) {
        selected.clear();
        selected.push(full);
    }
    let rects = selected
        .iter()
        .map(|b| {
            let (w, h, rgba) = fb.sub_rgba(b.x, b.y, b.w, b.h);
            assert_eq!((w, h), (b.w, b.h));
            Rect { b: *b, rgba }
        })
        .collect();
    tx.send(rects).unwrap();
    let extraction_us = elapsed_us(start);
    let start = Instant::now();
    let events: Vec<Vec<Rect>> = rx.try_iter().collect();
    let dequeue_us = elapsed_us(start);
    let start = Instant::now();
    for rects in events {
        for r in rects {
            if r.b.w == 0
                || r.b.h == 0
                || r.b.x + r.b.w > fb.width
                || r.b.y + r.b.h > fb.height
                || (r.b.w * r.b.h * 4) as usize != r.rgba.len()
            {
                continue;
            }
            let image =
                ColorImage::from_rgba_unmultiplied([r.b.w as usize, r.b.h as usize], &r.rgba);
            tex.set_partial(
                [r.b.x as usize, r.b.y as usize],
                image,
                TextureOptions::LINEAR,
            );
        }
    }
    let prep_us = elapsed_us(start);
    let delta = ctx.tex_manager().write().take_delta();
    let bytes = delta
        .set
        .iter()
        .map(|(_, d)| d.image.width() * d.image.height() * 4)
        .sum();
    if let Some(expected) = oracle {
        let mut got = vec![0u8; (fb.width * fb.height * 4) as usize];
        for px in got.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        for (_, d) in &delta.set {
            let [x, y] = d.pos.unwrap_or([0, 0]);
            if let egui::ImageData::Color(image) = &d.image {
                for row in 0..image.size[1] {
                    for col in 0..image.size[0] {
                        let to = ((y + row) * fb.width as usize + x + col) * 4;
                        got[to..to + 4]
                            .copy_from_slice(&image.pixels[row * image.size[0] + col].to_array());
                    }
                }
            } else {
                panic!("unexpected font image");
            }
        }
        assert_eq!(
            got, expected,
            "CPU replay of texture deltas differs from final framebuffer"
        );
    }
    black_box(&delta);
    Sample {
        extraction_us,
        dequeue_us,
        prep_us,
        deltas: delta.set.len(),
        bytes,
    }
}
fn percentile(v: &[f64], q: f64) -> f64 {
    let mut sorted = v.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn run_case(name: &str, fb: &Framebuffer, dirty: &[Bounds]) {
    let mut expected = vec![0u8; fb.pixels.len()];
    for p in expected.as_chunks_mut::<4>().0 {
        p[3] = 255;
    }
    for b in dirty {
        for row in 0..b.h {
            let offset = (((b.y + row) * fb.width + b.x) * 4) as usize;
            let bytes = (b.w * 4) as usize;
            expected[offset..offset + bytes].copy_from_slice(&fb.pixels[offset..offset + bytes]);
        }
    }
    let first = [
        probe(fb, dirty, false, Some(&expected)),
        probe(fb, dirty, true, Some(&expected)),
    ];
    for _ in 0..3 {
        for mode in [false, true] {
            black_box(probe(fb, dirty, mode, None));
        }
    }
    let mut rows = [Vec::<Sample>::new(), Vec::<Sample>::new()];
    // Alternate order between repetitions to limit fixed run-order bias.
    for rep in 0..21 {
        for i in [rep % 2, 1 - rep % 2] {
            rows[i].push(probe(fb, dirty, i == 1, None));
        }
    }
    for i in 0..2 {
        let ex: Vec<_> = rows[i].iter().map(|r| r.extraction_us).collect();
        let dq: Vec<_> = rows[i].iter().map(|r| r.dequeue_us).collect();
        let pr: Vec<_> = rows[i].iter().map(|r| r.prep_us).collect();
        println!(
            "{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            name,
            if i == 1 {
                "full_cover_model"
            } else {
                "baseline"
            },
            fb.width,
            fb.height,
            dirty.len(),
            first[i].deltas,
            first[i].bytes,
            percentile(&ex, 0.5),
            percentile(&ex, 0.95),
            percentile(&dq, 0.5),
            percentile(&dq, 0.95),
            percentile(&pr, 0.5),
            percentile(&pr, 0.95)
        );
    }
}
fn pattern(w: u32, h: u32) -> Framebuffer {
    let mut fb = Framebuffer::new(w, h);
    for (i, pixel) in fb.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        pixel.copy_from_slice(&[
            (i & 255) as u8,
            ((i >> 8) & 255) as u8,
            ((i >> 16) & 255) as u8,
            255,
        ]);
    }
    fb
}
fn main() {
    println!("# cpu_only=true; GPU_upload=false; presentation=false; samples=21; warmups=3; paired_alternating_order=true; baseline_revision=d10af413874ac78b3a1b9045efe33611e49eae7d; egui=0.31.1");
    println!("case,mode,width,height,input_rects,output_deltas,retained_payload_bytes,extraction_p50_us,extraction_p95_us,dequeue_p50_us,dequeue_p95_us,prep_p50_us,prep_p95_us");
    for (w, h, n) in [
        (1920, 1080, 1),
        (1920, 1080, 2),
        (1920, 1080, 4),
        (1920, 1080, 8),
        (3840, 2160, 1),
        (3840, 2160, 2),
        (3840, 2160, 4),
    ] {
        assert!((w as u64) * (h as u64) * (n as u64) <= 32 * 1024 * 1024);
        let fb = pattern(w, h);
        run_case("duplicate_full", &fb, &vec![Bounds { x: 0, y: 0, w, h }; n]);
    }
    // Normal-control workloads: full refresh, tiled refresh, scroll damage,
    // overlapping but non-full regions; candidate must not invent bigger uploads.
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let fb = pattern(w, h);
        let mut tiles = vec![];
        for y in (0..h).step_by(64) {
            for x in (0..w).step_by(64) {
                tiles.push(Bounds {
                    x,
                    y,
                    w: 64.min(w - x),
                    h: 64.min(h - y),
                });
            }
        }
        run_case("disjoint_tiles64", &fb, &tiles);
        run_case(
            "single_text64x16",
            &fb,
            &[Bounds {
                x: 100,
                y: 100,
                w: 64,
                h: 16,
            }],
        );
        run_case(
            "full_plus_damage128",
            &fb,
            &[
                Bounds { x: 0, y: 0, w, h },
                Bounds {
                    x: 100,
                    y: 100,
                    w: 128,
                    h: 128,
                },
            ],
        );
        run_case(
            "overlap_no_full",
            &fb,
            &[
                Bounds {
                    x: 0,
                    y: 0,
                    w: w / 2 + 32,
                    h,
                },
                Bounds {
                    x: w / 2 - 32,
                    y: 0,
                    w: w / 2 + 32,
                    h,
                },
            ],
        );
        let mut scroll = pattern(w, h);
        scroll.copy_rect(0, 0, 0, 16, w, h - 16);
        let mut bottom = vec![0x7fu8; (w * 16 * 4) as usize];
        for a in bottom.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        scroll.blit_rgba(0, h - 16, w, 16, &bottom);
        run_case(
            "copyrect_scroll_damage",
            &scroll,
            &[
                Bounds {
                    x: 0,
                    y: 0,
                    w,
                    h: h - 16,
                },
                Bounds {
                    x: 0,
                    y: h - 16,
                    w,
                    h: 16,
                },
            ],
        );
    }
    // Small exact-pixel regression oracles, including actual CopyRect overlap.
    let w = 8;
    let h = 6;
    let full = Bounds { x: 0, y: 0, w, h };
    let all = [
        vec![full],
        vec![
            Bounds {
                x: 0,
                y: 0,
                w: 4,
                h,
            },
            Bounds {
                x: 4,
                y: 0,
                w: 4,
                h,
            },
        ],
        vec![
            Bounds {
                x: 0,
                y: 0,
                w: 6,
                h,
            },
            Bounds {
                x: 2,
                y: 0,
                w: 6,
                h,
            },
        ],
        vec![
            Bounds {
                x: 1,
                y: 1,
                w: 3,
                h: 3,
            },
            full,
            Bounds {
                x: 2,
                y: 2,
                w: 4,
                h: 2,
            },
        ],
    ];
    for dirty in &all {
        let mut fb = pattern(w, h);
        fb.copy_rect(1, 1, 0, 0, 7, 5);
        probe(&fb, dirty, false, Some(&fb.pixels));
        probe(&fb, dirty, true, Some(&fb.pixels));
    }
    eprintln!("PASS: exact final-pixel CPU replay for every workload, plus disjoint/overlapping/full-cover and actual Framebuffer CopyRect controls. No protocol decoder, event-control/resize order, GPU or presentation claims.");
}
