//! Headless state-hash reporting for G4's fresh-process comparison (spec 14.3, 14.12).
//!
//! A process that starts again, builds
//! [`wp05_fixture::document`](../anim_test_support/wp05_fixture/fn.document.html), runs the
//! export route and writes the report to stdout. The parent test
//! (`tests/wp05_isolation.rs`) spawns it twice with the same arguments and compares the bytes,
//! which is the only form of the 10,000-frame clause that is not self-consistency: no memory,
//! no static, no allocation history and no cache is shared between the two runs.
//!
//! The arguments are configuration, not input. `--cadence` and `--capacity` choose the
//! checkpoint policy, `--frames` shortens the run for the fast comparison, and `--brief` drops
//! the per-tick lines so a long run's aggregate still compares. Nothing here accepts a
//! timestamp, a locale, an environment variable or a path, and the exit status is stable, so
//! two invocations have no way to differ that is not the engine.

use anim_core::id::SceneId;
use anim_test_support::wp05_fixture;

fn main() {
    let mut cadence = 0u64;
    let mut capacity = 0usize;
    let mut frames = wp05_fixture::GATE_FRAMES;
    let mut per_tick = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("{name} needs a value");
                std::process::exit(2);
            })
        };
        match arg.as_str() {
            "--cadence" => {
                cadence = value("--cadence").parse().unwrap_or_else(|e| {
                    eprintln!("--cadence: {e}");
                    std::process::exit(2);
                })
            }
            "--capacity" => {
                capacity = value("--capacity").parse().unwrap_or_else(|e| {
                    eprintln!("--capacity: {e}");
                    std::process::exit(2);
                })
            }
            "--frames" => {
                frames = value("--frames").parse().unwrap_or_else(|e| {
                    eprintln!("--frames: {e}");
                    std::process::exit(2);
                })
            }
            "--brief" => per_tick = false,
            "--help" | "-h" => {
                print!("{}", usage());
                return;
            }
            other => {
                eprintln!("unknown option {other:?}\n{}", usage());
                std::process::exit(2);
            }
        }
    }

    let document = wp05_fixture::document();
    let scene_id: SceneId = wp05_fixture::scene_id();
    let request = anim_headless::replay::ReplayRequest {
        last_tick: frames * i64::from(wp05_fixture::TICKS_PER_FRAME),
        cadence_ticks: cadence,
        capacity,
        per_tick_hashes: per_tick,
    };
    match anim_headless::replay::report(&document, scene_id, &request) {
        Ok(text) => {
            // Written as one buffer rather than line by line: a process killed mid-write
            // leaves a partial report, and the parent's byte comparison would then be
            // comparing a truncated file against a complete one and calling it a
            // determinism failure.
            print!("{text}");
        }
        Err(error) => {
            eprintln!("wp05_replay: {error}");
            std::process::exit(1);
        }
    }
}

fn usage() -> String {
    format!(
        "wp05_replay - deterministic state-hash report for gate G4\n\n\
         \x20 --cadence <ticks>   checkpoint cadence, 0 disables (default 0)\n\
         \x20 --capacity <n>      checkpoint entries kept, 0 disables (default 0)\n\
         \x20 --frames <n>        frames to simulate (default {})\n\
         \x20 --brief             write the aggregate but not the per-tick lines\n\n\
         The document is rebuilt from anim_test_support::wp05_fixture in every process, so the\n\
         comparison is between two runs rather than between two views of one run.\n",
        wp05_fixture::GATE_FRAMES,
    )
}
