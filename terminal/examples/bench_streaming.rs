//! Metadata-only parser and snapshot-layout microbenchmark (no window or PTY).
//! Run: cargo run --manifest-path terminal/Cargo.toml --release --example bench_streaming -- 10000
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use terminal::alacritty_terminal::term::cell::{Cell, Flags};
use terminal::alacritty_terminal::term::color::Colors;
use terminal::{ColorPalette, GpuiEventProxy, TerminalRenderer, TerminalState};

struct CountingAllocator;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn report(name: &str, samples: &mut [f64], bytes: usize, allocations: u64) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{name},{},{bytes},{:.3},{:.3},{:.3},{allocations}",
        samples.len(),
        samples[samples.len() / 2],
        samples[(samples.len() - 1) * 95 / 100],
        samples[samples.len() - 1]
    );
}

fn main() {
    let layout_rows = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<usize>().expect("row count"))
        .unwrap_or(48)
        .clamp(1, 20_000);
    println!("case,samples,bytes,p50_ms,p95_ms,max_ms,allocations");
    let plain = b"streamed terminal output with short lines and ordinary spaces\r\n";
    let ansi = b"\x1b[1;38;2;200;120;60mred\x1b[0m \x1b[4munderlined\x1b[0m\r\n";
    for (name, seed, restore) in [
        ("plain", plain.as_slice(), false),
        ("ansi", ansi.as_slice(), false),
        ("long_line", b"abcdefghij".as_slice(), false),
        ("restoration", plain.as_slice(), true),
    ] {
        let bytes: Vec<_> = seed.iter().copied().cycle().take(128 * 1024).collect();
        let (tx, rx) = mpsc::channel();
        let mut state = TerminalState::new(120, 48, GpuiEventProxy::new(tx));
        let mut samples = Vec::with_capacity(101);
        let mut allocations = 0;
        for index in 0..111 {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            if restore {
                state.feed_raw(black_box(&bytes));
            } else {
                black_box(state.process_bytes(black_box(&bytes)));
            }
            let ms = start.elapsed().as_secs_f64() * 1000.;
            let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
            while rx.try_recv().is_ok() {}
            if index >= 10 {
                samples.push(ms);
                allocations += allocated;
            }
        }
        report(name, &mut samples, bytes.len(), allocations);
        black_box(state);
    }

    let renderer = TerminalRenderer::new(
        "monospace".into(),
        Default::default(),
        1.2,
        ColorPalette::default(),
    );
    let colors = Colors::default();
    for styled in [false, true] {
        let cells: Vec<_> = (0..120)
            .map(|index| {
                let mut cell = Cell::default();
                cell.c = b"streamed output "[index % 16] as char;
                if styled && index % 12 < 6 {
                    cell.flags = Flags::BOLD | Flags::UNDERLINE;
                }
                cell
            })
            .collect();
        let mut samples = Vec::with_capacity(101);
        let mut allocations = 0;
        for index in 0..111 {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            for row in 0..layout_rows {
                black_box(renderer.layout_row(
                    row,
                    black_box(&cells).iter().cloned().enumerate(),
                    &colors,
                ));
            }
            let ms = start.elapsed().as_secs_f64() * 1000.;
            let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
            if index >= 10 {
                samples.push(ms);
                allocations += allocated;
            }
        }
        report(
            if styled {
                "snapshot_styled"
            } else {
                "snapshot_plain"
            },
            &mut samples,
            120 * layout_rows,
            allocations,
        );
    }
}
