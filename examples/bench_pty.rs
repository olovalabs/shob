//! PTY caller occupancy and lifecycle timings, without GPUI or inference.
//! Run from the app: cargo run --example bench_pty -- 15
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{BufRead, BufReader};
use std::time::Instant;
use terminal::pty_session::{PendingPty, PreparedPty, PtyEvent};

fn command() -> CommandBuilder {
    let mut command = CommandBuilder::new("/bin/bash");
    command.args(["-c", "printf 'ready\\n'; sleep 30"]);
    command
}
fn size() -> PtySize {
    PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}
fn main() {
    let samples = std::env::args()
        .nth(1)
        .map(|n| n.parse::<usize>().unwrap())
        .unwrap_or(15)
        .clamp(1, 100);
    let mut times: Vec<(&str, f64)> = Vec::new();
    for _ in 0..samples {
        // Exact blocking API boundary previously called by GPUI. Cleanup here
        // is explicit; dropping portable-pty's child alone does not reap it.
        let start = Instant::now();
        let pair = native_pty_system().openpty(size()).unwrap();
        let mut child = pair.slave.spawn_command(command()).unwrap();
        let reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        times.push((
            "blocking_setup_caller",
            start.elapsed().as_secs_f64() * 1000.0,
        ));
        drop(pair.slave);
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        times.push((
            "blocking_setup_first_output",
            start.elapsed().as_secs_f64() * 1000.0,
        ));
        child.kill().unwrap();
        child.wait().unwrap();
        drop(writer);
        drop(pair.master);

        let start = Instant::now();
        let pending = PendingPty::spawn(|| {
            let pair = native_pty_system().openpty(size())?;
            PreparedPty::spawn(pair, command())
        });
        times.push((
            "pending_setup_caller",
            start.elapsed().as_secs_f64() * 1000.0,
        ));
        let PendingPty {
            reader,
            writer,
            control,
            events,
        } = pending;
        smol::block_on(async {
            match events.recv().await.unwrap() {
                PtyEvent::Ready { .. } => {}
                PtyEvent::Error(error) => panic!("{error}"),
                _ => panic!("exit before readiness"),
            }
        });
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        times.push((
            "pending_setup_first_output",
            start.elapsed().as_secs_f64() * 1000.0,
        ));
        control.resize(PtySize {
            cols: 120,
            ..size()
        });
        let stop = Instant::now();
        control.close();
        times.push((
            "pending_close_caller",
            stop.elapsed().as_secs_f64() * 1000.0,
        ));
        smol::block_on(async {
            loop {
                match events.recv().await.unwrap() {
                    PtyEvent::Exited(_) => break,
                    PtyEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        });
        times.push((
            "pending_close_settled",
            stop.elapsed().as_secs_f64() * 1000.0,
        ));
        drop(writer);
        drop(control);
    }
    let mut names: Vec<_> = times.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    println!("metric,samples,p50_ms,p95_ms");
    for name in names {
        let mut values: Vec<_> = times
            .iter()
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| *value)
            .collect();
        values.sort_by(f64::total_cmp);
        println!(
            "{name},{},{:.3},{:.3}",
            values.len(),
            values[values.len() / 2],
            values[(values.len() * 95).div_ceil(100) - 1]
        );
    }
}
