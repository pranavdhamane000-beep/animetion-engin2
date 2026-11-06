//! CLI parsing, export execution, reports and stable exit codes (spec 4.2).
//!
//! The export entry point is still a stub, which WP09 fills in. What WP05 changes is that the
//! binary no longer exits silently: it names the one thing that exists, and it points at the
//! process that actually runs it.
//!
//! Nothing here reads a project file. The process the simulation gate spawns is
//! `crates/anim_test_support/src/bin/wp05_replay.rs`, which builds the authored fixture it
//! compares against — exactly as WP04's `wp04_snapshot` did — so the two fresh processes share
//! one fixture definition and one reporting path rather than approximating each with their own.

/// The usage text, one value so `--help` and the error path cannot disagree.
fn usage() -> &'static str {
    "anim_headless - deterministic offline animation engine\n\n\
     Export execution arrives in WP09 (spec 18). The part that exists today is the\n\
     WP05 state-hash report, reached programmatically rather than from this binary:\n\
     \x20 anim_headless::replay::report\n\
     and run as a separate process by\n\
     \x20 anim_test_support/src/bin/wp05_replay.rs\n"
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--help" | "-h") => print!("{}", usage()),
        Some(other) => {
            eprintln!("unknown option {other:?}\n{}", usage());
            std::process::exit(2);
        }
        None => {
            eprint!("{}", usage());
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    #[test]
    fn the_usage_names_the_real_entry_point() {
        let text = usage();
        assert!(
            text.contains("anim_headless::replay::report"),
            "a stub that says nothing is what this crate shipped for five packages: {text}"
        );
        assert!(text.contains("wp05_replay"), "and the process that uses it");
        assert!(
            !text.contains("2026") && !text.contains("time"),
            "a usage string that varies is a usage string a test cannot pin"
        );
    }
}
