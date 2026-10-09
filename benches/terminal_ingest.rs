//! Throughput of [`vvmux_terminal::Terminal::feed`], the session daemon's per-byte hot path.
//!
//! Every byte a pane prints passes through `feed` on the session actor before anything is
//! rendered, so its throughput bounds how fast a busy pane can scroll and how much CPU an idle
//! session spends on chatty programs. Run with `cargo bench --bench terminal_ingest`; set
//! `VVMUX_BENCH_SECONDS` to change how long each workload runs (default 2).
//!
//! The workloads model plain log output, SGR-heavy colored output, full-screen redraws with cursor
//! addressing, and wide characters. Each reports bytes per second of wall-clock time on one
//! thread, which is also its CPU time: `feed` is single-threaded.
//!
//! The benchmark uses the same global allocator as the `vvmux` binary, so it measures what ships.

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::hint::black_box;
use std::time::{Duration, Instant};

use vvmux_terminal::Terminal;

/// One benchmark input: a name and the bytes fed repeatedly.
struct Workload {
    name: &'static str,
    bytes: Vec<u8>,
}

fn workloads() -> Vec<Workload> {
    let mut plain = Vec::new();
    for line in 0..2_000 {
        plain.extend_from_slice(
            format!(
                "2026-10-08T12:00:{:02}Z INFO request {line} completed in 3ms\r\n",
                line % 60
            )
            .as_bytes(),
        );
    }

    let mut colored = Vec::new();
    for line in 0..2_000 {
        colored.extend_from_slice(
            format!(
                "\x1b[1;32m✓\x1b[0m \x1b[38;5;244mtest\x1b[0m module::case_{line} \x1b[2m... \x1b[0m\
                 \x1b[38;2;80;200;120mok\x1b[0m\r\n"
            )
            .as_bytes(),
        );
    }

    let mut redraw = Vec::new();
    for frame in 0..60 {
        redraw.extend_from_slice(b"\x1b[H");
        for row in 1..=24 {
            redraw.extend_from_slice(
                format!(
                    "\x1b[{row};1H\x1b[48;5;{}m{:<80}\x1b[0m",
                    (row + frame) % 256,
                    frame
                )
                .as_bytes(),
            );
        }
    }

    let mut wide = Vec::new();
    for _ in 0..2_000 {
        wide.extend_from_slice(
            "终端复用器 ターミナル 터미널 🙂 emoji and CJK mixed\r\n".as_bytes(),
        );
    }

    vec![
        Workload {
            name: "plain log lines",
            bytes: plain,
        },
        Workload {
            name: "SGR-heavy colored output",
            bytes: colored,
        },
        Workload {
            name: "full-screen redraws",
            bytes: redraw,
        },
        Workload {
            name: "wide characters",
            bytes: wide,
        },
    ]
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the reported figure is a printed MiB/s rate, not exact arithmetic"
)]
fn main() {
    let seconds = std::env::var("VVMUX_BENCH_SECONDS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(2.0);
    let budget = Duration::from_secs_f64(seconds);
    for workload in workloads() {
        let mut terminal = Terminal::new(24, 80, 10_000);
        // Warm up: grow scrollback to its limit so the measurement includes eviction.
        for _ in 0..8 {
            black_box(terminal.feed(&workload.bytes));
        }
        let started = Instant::now();
        let mut fed = 0_u64;
        while started.elapsed() < budget {
            black_box(terminal.feed(black_box(&workload.bytes)));
            fed += workload.bytes.len() as u64;
        }
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "{:<28} {:>8.1} MiB/s",
            workload.name,
            fed as f64 / elapsed / (1024.0 * 1024.0)
        );
    }
}
