//! The command line of the built binary: the arguments that print and exit,
//! and the ones that are ignored.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const VERSION: &str = concat!("sharkline ", env!("CARGO_PKG_VERSION"), "\n");
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stdin.json");
/// The most a run that holds stdin open gets to print. Only a hung binary uses
/// it all: it keeps a regression that blocks from hanging CI, and is not a speed
/// requirement.
const DEADLINE: Duration = Duration::from_secs(10);

/// The binary with a scratch HOME and cache, and none of the variables it reads
/// that would change the output.
fn sharkline(scratch: &str, args: &[&str]) -> Command {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(scratch);
    fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharkline"));
    cmd.args(args)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("NO_COLOR", "1")
        .env("SHARKLINE_NOW", "1750000000")
        .env_remove("COLUMNS")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("OMC_PLUGIN_ROOT");
    cmd
}

/// stdout, stderr and the exit code of a run with stdin closed.
fn printed(args: &[&str]) -> (String, String, Option<i32>) {
    let out = sharkline("print", args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
        out.status.code(),
    )
}

fn help_is_usage(text: &str) {
    assert!(text.starts_with("sharkline: "), "{text}");
    assert!(text.contains("\"statusLine\""), "{text}");
    for var in [
        "NO_COLOR",
        "COLUMNS",
        "CLAUDE_CONFIG_DIR",
        "OMC_PLUGIN_ROOT",
        "XDG_CACHE_HOME",
        "SHARKLINE_NOW",
    ] {
        assert!(text.contains(var), "{var} missing from {text}");
    }
    assert!(text.lines().count() <= 24, "{text}");
}

#[test]
fn version_flags_print_the_version() {
    for flag in ["--version", "-V"] {
        let (out, err, code) = printed(&[flag]);
        assert_eq!((out.as_str(), err.as_str(), code), (VERSION, "", Some(0)));
    }
}

#[test]
fn help_flags_print_the_usage() {
    let mut texts = Vec::new();
    for flag in ["--help", "-h"] {
        let (out, err, code) = printed(&[flag]);
        assert_eq!((err.as_str(), code), ("", Some(0)));
        help_is_usage(&out);
        texts.push(out);
    }
    assert_eq!(texts[0], texts[1]);
}

#[test]
fn the_flags_do_not_read_stdin() {
    // Stdin is an open pipe that is never written: a read would sit there until
    // the watchdog printed a status line instead. There is no timing bound: that
    // line is not the version or the help text, so the content assertions fail a
    // binary that waits on stdin.
    for (flag, is_version) in [
        ("--version", true),
        ("-V", true),
        ("--help", false),
        ("-h", false),
    ] {
        let mut child = sharkline("pipe", &[flag])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let held_open = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let (sent, received) = mpsc::channel();
        thread::spawn(move || {
            let mut out = String::new();
            let read = stdout.read_to_string(&mut out).map(|_| out);
            let _ = sent.send(read);
        });
        let out = match received.recv_timeout(DEADLINE) {
            Ok(read) => read.unwrap(),
            Err(_) => {
                let _ = child.kill();
                child.wait().unwrap();
                panic!("{flag} printed nothing in {DEADLINE:?}: it waits on stdin");
            }
        };
        let status = child.wait().unwrap();
        drop(held_open);
        assert_eq!(status.code(), Some(0), "{flag}");
        if is_version {
            assert_eq!(out, VERSION, "{flag}");
        } else {
            help_is_usage(&out);
        }
    }
}

#[test]
fn any_other_argument_still_renders() {
    let render = |args: &[&str]| {
        let out = sharkline("render", args)
            .stdin(File::open(FIXTURE).unwrap())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stderr.is_empty(), "{args:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let plain = render(&[]);
    assert!(plain.contains("Model: Opus 5.5"), "{plain}");
    assert_eq!(render(&["--bogus"]), plain);
    // Only the first argument counts.
    assert_eq!(render(&["--bogus", "--version"]), plain);
}
