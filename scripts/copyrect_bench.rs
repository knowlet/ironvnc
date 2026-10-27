//! Standalone optimized microbenchmark of the actual framebuffer implementation.
//! Compile with IRONVNC_FRAMEBUFFER_RS set to an absolute framebuffer.rs path.
//! Use --cfg allocation_counts for separate, untimed allocation measurements.

#[allow(dead_code)]
mod framebuffer {
    include!(env!("IRONVNC_FRAMEBUFFER_RS"));
}

use framebuffer::Framebuffer;
use std::hint::black_box;
#[cfg(not(allocation_counts))]
use std::time::Instant;

#[cfg(allocation_counts)]
mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    pub struct Counter;
    static ENABLED: AtomicBool = AtomicBool::new(false);
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static BYTES: AtomicUsize = AtomicUsize::new(0);

    fn record(bytes: usize) {
        if ENABLED.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            BYTES.fetch_add(bytes, Relaxed);
        }
    }

    // Instrument only this standalone, single-threaded executable. Delegation
    // preserves System's allocation/deallocation contracts; never enabled in
    // timing builds or in the viewer.
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record(size);
            unsafe { System.realloc(ptr, layout, size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    pub fn measure(f: impl FnOnce()) -> (usize, usize) {
        CALLS.store(0, Relaxed);
        BYTES.store(0, Relaxed);
        ENABLED.store(true, Relaxed);
        f();
        ENABLED.store(false, Relaxed);
        (CALLS.load(Relaxed), BYTES.load(Relaxed))
    }
}

#[cfg(allocation_counts)]
#[global_allocator]
static ALLOCATOR: allocations::Counter = allocations::Counter;

struct Case {
    name: &'static str,
    frame: (u32, u32),
    src: (u32, u32),
    dst: (u32, u32),
    rect: (u32, u32),
}

impl Case {
    fn copy(&self, fb: &mut Framebuffer) {
        fb.copy_rect(
            black_box(self.dst.0),
            black_box(self.dst.1),
            black_box(self.src.0),
            black_box(self.src.1),
            black_box(self.rect.0),
            black_box(self.rect.1),
        );
        black_box(&fb.pixels);
    }
}

fn main() {
    let cases = [
        Case {
            name: "small_tile",
            frame: (640, 480),
            src: (0, 0),
            dst: (100, 100),
            rect: (32, 32),
        },
        Case {
            name: "1080p_scroll_up",
            frame: (1920, 1080),
            src: (0, 16),
            dst: (0, 0),
            rect: (1920, 1064),
        },
        Case {
            name: "1080p_scroll_down",
            frame: (1920, 1080),
            src: (0, 0),
            dst: (0, 16),
            rect: (1920, 1064),
        },
        Case {
            name: "4k_scroll_up",
            frame: (3840, 2160),
            src: (0, 16),
            dst: (0, 0),
            rect: (3840, 2144),
        },
        Case {
            name: "4k_scroll_down",
            frame: (3840, 2160),
            src: (0, 0),
            dst: (0, 16),
            rect: (3840, 2144),
        },
        Case {
            name: "1080p_horizontal",
            frame: (1920, 1080),
            src: (0, 0),
            dst: (16, 0),
            rect: (1904, 1080),
        },
        Case {
            name: "window_diagonal",
            frame: (1920, 1080),
            src: (100, 100),
            dst: (132, 132),
            rect: (1280, 720),
        },
        Case {
            name: "narrow_column",
            frame: (1920, 1080),
            src: (0, 16),
            dst: (0, 0),
            rect: (1, 1064),
        },
    ];

    #[cfg(not(allocation_counts))]
    println!("case,samples,median_ns,p95_ns");
    #[cfg(allocation_counts)]
    println!("case,allocation_calls,allocated_bytes");

    let selected_case =
        std::env::args().find_map(|arg| arg.strip_prefix("--case=").map(str::to_owned));
    if let Some(name) = &selected_case {
        assert!(
            cases.iter().any(|case| case.name == name),
            "unknown case: {name}"
        );
    }
    for case in cases {
        if selected_case
            .as_deref()
            .is_some_and(|name| name != case.name)
        {
            continue;
        }
        let mut fb = Framebuffer::new(case.frame.0, case.frame.1);
        for (i, byte) in fb.pixels.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(37) ^ (i >> 8)) as u8;
        }
        for _ in 0..16 {
            case.copy(&mut fb);
        }

        #[cfg(allocation_counts)]
        {
            let (calls, bytes) = allocations::measure(|| case.copy(&mut fb));
            println!("{},{calls},{bytes}", case.name);
            if std::env::args().any(|arg| arg == "--assert-zero-allocations") {
                assert_eq!(calls, 0, "{} must not allocate", case.name);
            }
        }
        #[cfg(not(allocation_counts))]
        {
            let mut samples = Vec::with_capacity(501);
            for _ in 0..501 {
                let start = Instant::now();
                case.copy(&mut fb);
                samples.push(start.elapsed().as_nanos());
            }
            samples.sort_unstable();
            println!("{},501,{},{}", case.name, samples[250], samples[475]);
        }
    }
}
