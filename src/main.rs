//! sharkline: a Claude Code status line.
//!
//! Reads the render JSON Claude Code writes on stdin and prints the "focused"
//! layout of the oh-my-claudecode (OMC) HUD, which these comments call "the
//! HUD": a git line, the main line, and any detail lines. It never
//! fails: every error path degrades to fewer segments (a segment that cannot be
//! worked out is dropped, not the line), nothing goes to stderr, and the exit
//! status is always 0. It does not block either: a render still working after
//! `WATCHDOG` prints the line the payload alone gives and exits.

mod cache;
mod git;
mod json;
mod layout;
mod omc;
mod render;
mod transcript;

use render::{Facts, Opts};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A render JSON is a few KiB; anything past this is not one.
const MAX_STDIN_BYTES: u64 = 1 << 20;
/// A render that has printed nothing by now is stuck on something slow (a
/// `stat` on a hung mount, a pipe that never closes). Claude Code gives a
/// status line a few seconds at most; `git` has its own, shorter limit.
const WATCHDOG: Duration = Duration::from_millis(1500);

/// What arrived on stdin, kept where the watchdog can find it.
static STDIN: OnceLock<Vec<u8>> = OnceLock::new();
/// Set by whichever of the render and the watchdog prints first.
static PRINTED: AtomicBool = AtomicBool::new(false);

fn main() {
    // No panic message on stderr, and no panic at all in the output: print an
    // empty line instead.
    panic::set_hook(Box::new(|_| {}));
    // Without a thread to run the watchdog the render goes ahead unwatched.
    let _ = std::thread::Builder::new().spawn(watchdog);
    let line = panic::catch_unwind(run).unwrap_or_default();
    if !emit(&PRINTED, &mut io::stdout().lock(), &line) {
        // The watchdog got there first and exits the process when it is done.
        std::thread::sleep(WATCHDOG);
    }
}

/// After `WATCHDOG`, print the line the payload gives on its own and exit.
fn watchdog() {
    std::thread::sleep(WATCHDOG);
    let body = STDIN.get().map_or(&[][..], Vec::as_slice);
    let line = panic::catch_unwind(|| fallback(body, &options())).unwrap_or_default();
    if emit(&PRINTED, &mut io::stdout().lock(), &line) {
        std::process::exit(0);
    }
}

/// Print `line` unless `printed` says something has. Whether this call did.
fn emit(printed: &AtomicBool, out: &mut impl Write, line: &str) -> bool {
    if printed.swap(true, Ordering::SeqCst) {
        return false;
    }
    // A closed stdout is not worth failing over.
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    true
}

fn run() -> String {
    let buf = STDIN.get_or_init(|| {
        let mut buf = Vec::new();
        // On a read error keep whatever arrived; the parse below decides.
        let _ = io::stdin()
            .lock()
            .take(MAX_STDIN_BYTES)
            .read_to_end(&mut buf);
        buf
    });
    let input = json::parse(buf);
    let opts = options();
    let facts = gather(&input, &opts);
    render::render(&input, &facts, &opts)
}

/// The line with nothing but the payload in it: no repository, plugin or
/// transcript, so nothing to wait for.
fn fallback(body: &[u8], opts: &Opts) -> String {
    render::render(&json::parse(body), &Facts::default(), opts)
}

fn options() -> Opts {
    Opts {
        color: color_enabled(),
        now: now(),
        columns: columns(),
    }
}

/// Everything the line shows that is not in the payload. Each fact is worked
/// out on its own, so one that fails (or panics) leaves only its segment out.
fn gather(input: &Value, o: &Opts) -> Facts {
    let cache_dir = cache::dir();
    let config = omc::config_dir();
    let dir = render::work_dir(input)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let repo = guard(|| git::find(dir.as_deref()?));
    let head = repo.as_ref().and_then(|r| guard(|| r.head()));
    Facts {
        branch: head.as_ref().map(git::Head::label),
        worktree: repo.as_ref().and_then(|r| r.worktree.clone()),
        status: repo
            .as_ref()
            .and_then(|r| guard(|| git::status(r, head.as_ref(), cache_dir.as_deref(), o.now))),
        profile: guard(profile),
        plugin: guard(|| omc::plugin(config.as_deref()?)),
        updates: guard(|| Some(omc::updates(config.as_deref()?))),
        calls: guard(|| {
            let path = input
                .get("transcript_path")?
                .as_str()
                .filter(|p| !p.is_empty())?;
            let session = input.get("session_id").and_then(Value::as_str);
            transcript::calls(Path::new(path), session, cache_dir.as_deref(), o.now)
        }),
    }
}

/// Run one fact's lookup; a panic is the same as not knowing.
fn guard<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    panic::catch_unwind(AssertUnwindSafe(f)).ok().flatten()
}

/// The name of the config dir when `CLAUDE_CONFIG_DIR` is set, without a
/// leading dot (`~/.claude-work` is profile `claude-work`).
fn profile() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty())?;
    let name = Path::new(&dir).file_name()?.to_string_lossy().into_owned();
    Some(name.strip_prefix('.').unwrap_or(&name).to_string())
}

/// NO_COLOR convention: set and non-empty turns color off.
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Terminal width from `COLUMNS`, which Claude Code sets for the status line
/// command; 0 when absent or not a positive number.
fn columns() -> usize {
    let value = std::env::var("COLUMNS").unwrap_or_default();
    let digits = value
        .trim_start()
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    value.trim_start()[..digits].parse().unwrap_or(0)
}

/// Unix seconds. `SHARKLINE_NOW` overrides the clock (fractions allowed)
/// so output is reproducible in tests.
fn now() -> f64 {
    std::env::var("SHARKLINE_NOW")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/stdin.json");

    fn plain() -> Opts {
        Opts {
            color: false,
            now: 1_750_000_000.0,
            columns: 0,
        }
    }

    #[test]
    fn the_fallback_is_the_payload_alone() {
        assert_eq!(
            fallback(FIXTURE, &plain()),
            "Model: Opus 5.5 xhigh | 5h:[####----]55%(2h20m) wk:[#######-]88%(6d6h) | session:95m | ctx:[###-------]30%"
        );
        // Nothing read yet: the segments that always print.
        assert_eq!(fallback(b"", &plain()), "session:0m | ctx:[----------]0%");
    }

    #[test]
    fn only_the_first_to_print_prints() {
        let printed = AtomicBool::new(false);
        let mut out = Vec::new();
        assert!(emit(&printed, &mut out, "first"));
        assert!(!emit(&printed, &mut out, "second"));
        assert_eq!(out, b"first\n");
    }
}
