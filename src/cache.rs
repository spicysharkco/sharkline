//! The on-disk cache under `${XDG_CACHE_HOME:-~/.cache}/sharkline/`.
//!
//! Every file is a small JSON document, replaced whole by write-to-temp then
//! rename, so a reader sees the old file or the new one and two sessions
//! writing at once cannot interleave. A missing, unreadable or malformed file
//! reads as `None`: the caller recomputes.
//!
//! The directory is private to the user (0700, files 0600): it holds paths and
//! session data.

use serde_json::Value;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

/// Cache documents are a few hundred bytes; anything past this is not one.
const MAX_READ_BYTES: u64 = 64 * 1024;
/// A file nobody has rewritten for this long belongs to a session or repo that
/// is gone (the HUD kept its per-session files for the same 7 days).
const MAX_AGE: Duration = Duration::from_secs(7 * 86_400);
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// The cache directory, or `None` when neither `XDG_CACHE_HOME` (absolute, per
/// the XDG spec) nor `HOME` is usable.
pub fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })?;
    Some(base.join("sharkline"))
}

/// 64-bit FNV-1a as 16 hex digits: a stable, filename-safe key for a path.
pub fn key(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn path_key(path: &Path) -> String {
    key(path.as_os_str().as_bytes())
}

/// The document `name` in `dir`. Not a regular file (a FIFO would block the
/// open), too big, or not JSON: `None`.
pub fn read(dir: &Path, name: &str) -> Option<Value> {
    let path = dir.join(name);
    if !fs::metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_READ_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Replace the document `name` in `dir`, creating the directory on first use.
/// A new entry also sweeps files old enough to be dead. The caller ignores the
/// error: a status line that cannot cache still renders.
pub fn write(dir: &Path, name: &str, doc: &Value) -> io::Result<()> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    ensure_dir(dir)?;
    let target = dir.join(name);
    let tmp = dir.join(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(&tmp)
        .and_then(|mut f| f.write_all(doc.to_string().as_bytes()));
    let is_new = !target.exists();
    let renamed = written.and_then(|()| fs::rename(&tmp, &target));
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    } else if is_new {
        sweep(dir, SystemTime::now());
    }
    renamed
}

/// Try to become the only one doing the work `name` stands for. `Ok(true)`:
/// the claim is yours (release it when done). `Ok(false)`: someone claimed it
/// less than `ttl` ago, whether they are still alive or not; a render that is
/// cancelled cannot release, so a claim older than `ttl` is taken over. `Err`:
/// the claim cannot be recorded, which also means nothing else can be.
///
/// Two renders that both find the same dead claim can both take it over; the
/// price is one extra run.
pub fn claim(dir: &Path, name: &str, ttl: Duration) -> io::Result<bool> {
    ensure_dir(dir)?;
    let path = dir.join(name);
    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
        {
            Ok(mut f) => {
                // Written, not just created: a full disk says so here.
                return match f.write_all(std::process::id().to_string().as_bytes()) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        let _ = fs::remove_file(&path);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A claim in the future is as untrustworthy as one too old.
                let fresh = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age < ttl);
                if fresh {
                    return Ok(false);
                }
                let _ = fs::remove_file(&path);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

pub fn release(dir: &Path, name: &str) {
    let _ = fs::remove_file(dir.join(name));
}

/// `dir` as a directory only the user can enter. One created by an earlier
/// version (0755, its files 0644) is tightened once, files included.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    // `symlink_metadata`: a symlink to a directory somewhere else is the
    // user's own arrangement, and its target's mode is not ours to change.
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            if meta.permissions().mode() & 0o777 != DIR_MODE {
                tighten(dir);
            }
            Ok(())
        }
        _ => DirBuilder::new().recursive(true).mode(DIR_MODE).create(dir),
    }
}

fn tighten(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE));
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let loose = entry
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o777 != FILE_MODE);
        if loose {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(FILE_MODE));
        }
    }
}

/// Remove files untouched for `MAX_AGE`, which also reaps temp files that a
/// crashed writer left behind.
fn sweep(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! Scratch directories for the tests of the modules that use the cache.

    use std::path::{Path, PathBuf};

    /// Removed on drop. Under the system temp dir, not the target dir: that
    /// sits inside this repository and a branch lookup would find its `.git`.
    pub struct Tmp(pub PathBuf);

    impl Tmp {
        pub fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("sharkline-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Tmp(path)
        }

        pub fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl std::ops::Deref for Tmp {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Tmp;
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Barrier};

    #[test]
    fn write_then_read_round_trips_and_creates_the_directory() {
        let t = Tmp::new("cache-rt");
        let dir = t.join("a/b/sharkline");
        write(&dir, "x.json", &json!({"n": 1})).unwrap();
        assert_eq!(read(&dir, "x.json"), Some(json!({"n": 1})));
    }

    #[test]
    fn missing_dir_file_or_garbage_reads_as_none() {
        let t = Tmp::new("cache-miss");
        assert_eq!(read(&t.join("nope"), "x.json"), None);
        assert_eq!(read(&t, "x.json"), None);
        t.write("bad.json", "{not json");
        assert_eq!(read(&t, "bad.json"), None);
        t.write("empty.json", "");
        assert_eq!(read(&t, "empty.json"), None);
        std::fs::write(t.join("bin.json"), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(read(&t, "bin.json"), None);
    }

    #[test]
    fn oversized_file_is_not_trusted() {
        let t = Tmp::new("cache-big");
        let big = format!("{{\"pad\":\"{}\"}}", "a".repeat(128 * 1024));
        t.write("big.json", &big);
        assert_eq!(
            read(&t, "big.json"),
            None,
            "truncated at the cap, so invalid"
        );
    }

    #[test]
    fn directory_or_fifo_in_place_of_a_file_reads_as_none() {
        let t = Tmp::new("cache-fifo");
        std::fs::create_dir_all(t.join("d.json")).unwrap();
        assert_eq!(read(&t, "d.json"), None);
        let made = std::process::Command::new("mkfifo")
            .arg(t.join("p.json"))
            .status()
            .is_ok_and(|s| s.success());
        if made {
            // Opening a FIFO with no writer blocks forever: run it off-thread.
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = t.0.clone();
            std::thread::spawn(move || {
                let _ = tx.send(read(&dir, "p.json"));
            });
            let got = rx.recv_timeout(Duration::from_secs(5));
            assert_eq!(got, Ok(None), "read must not block on a FIFO");
        }
    }

    #[test]
    fn write_replaces_whole_and_leaves_no_temp_file() {
        let t = Tmp::new("cache-replace");
        write(&t, "x.json", &json!({"v": 1})).unwrap();
        write(&t, "x.json", &json!({"v": 2})).unwrap();
        assert_eq!(read(&t, "x.json"), Some(json!({"v": 2})));
        let names: Vec<_> = std::fs::read_dir(&*t)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["x.json"]);
    }

    #[test]
    fn concurrent_writers_never_leave_a_torn_file() {
        let t = Tmp::new("cache-concurrent");
        let dir = Arc::new(t.0.clone());
        let start = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let (dir, start) = (dir.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for round in 0..50 {
                        let doc = json!({"writer": n, "round": round, "pad": "x".repeat(2000)});
                        write(&dir, "shared.json", &doc).unwrap();
                        let seen = read(&dir, "shared.json").expect("always a whole document");
                        assert_eq!(seen["pad"].as_str().map(str::len), Some(2000));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let leftovers = std::fs::read_dir(&*t)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn unwritable_dir_is_an_error_not_a_panic() {
        let t = Tmp::new("cache-unwritable");
        t.write("blocker", "a file where the directory should be");
        assert!(write(&t.join("blocker"), "x.json", &json!(1)).is_err());
    }

    #[test]
    fn new_entry_sweeps_old_files_only() {
        let t = Tmp::new("cache-sweep");
        let old = t.write("old.json", "{}");
        let fresh = t.write("fresh.json", "{}");
        let stale_tmp = t.write("x.json.1.0.tmp", "{");
        let long_ago = SystemTime::now() - MAX_AGE - Duration::from_secs(3600);
        for p in [&old, &stale_tmp] {
            File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        // Rewriting an existing entry does not sweep.
        write(&t, "fresh.json", &json!({})).unwrap();
        assert!(old.exists() && stale_tmp.exists());
        // A new entry does.
        write(&t, "brand-new.json", &json!({})).unwrap();
        assert!(!old.exists() && !stale_tmp.exists());
        assert!(fresh.exists() && t.join("brand-new.json").exists());
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_directory_and_its_files_are_private() {
        let t = Tmp::new("cache-private");
        let dir = t.join("a/sharkline");
        write(&dir, "x.json", &json!({"n": 1})).unwrap();
        assert!(claim(&dir, "x.run", Duration::from_secs(5)).unwrap());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("x.json")), 0o600);
        assert_eq!(mode(&dir.join("x.run")), 0o600);
    }

    #[test]
    fn a_cache_left_loose_by_an_older_version_is_tightened_once() {
        let t = Tmp::new("cache-loose");
        let dir = t.join("sharkline");
        for rel in ["sharkline/old.json", "elsewhere/other.json"] {
            t.write(rel, "{}");
            fs::set_permissions(t.join(rel), fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        write(&dir, "new.json", &json!({})).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("old.json")), 0o600);
        assert_eq!(mode(&dir.join("new.json")), 0o600);
        assert_eq!(mode(&t.join("elsewhere/other.json")), 0o644);
    }

    #[test]
    fn a_symlinked_cache_directory_is_left_as_the_user_arranged_it() {
        let t = Tmp::new("cache-symlink");
        let target = t.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, t.join("sharkline")).unwrap();
        write(&t.join("sharkline"), "x.json", &json!(1)).unwrap();
        assert_eq!(mode(&target), 0o755);
        assert_eq!(read(&target, "x.json"), Some(json!(1)));
    }

    #[test]
    fn a_claim_is_held_until_released_or_old() {
        let t = Tmp::new("cache-claim");
        let ttl = Duration::from_secs(5);
        assert!(claim(&t, "w.run", ttl).unwrap());
        assert!(!claim(&t, "w.run", ttl).unwrap(), "held");
        release(&t, "w.run");
        assert!(claim(&t, "w.run", ttl).unwrap(), "released");
        // A claim nobody released, such as a cancelled render's, runs out.
        let age = |secs: i64| {
            let at = if secs >= 0 {
                SystemTime::now() - Duration::from_secs(secs as u64)
            } else {
                SystemTime::now() + Duration::from_secs(secs.unsigned_abs())
            };
            File::options()
                .write(true)
                .open(t.join("w.run"))
                .unwrap()
                .set_modified(at)
                .unwrap();
        };
        age(4);
        assert!(!claim(&t, "w.run", ttl).unwrap(), "4 s old is still held");
        age(6);
        assert!(claim(&t, "w.run", ttl).unwrap(), "6 s old is not");
        age(-3600);
        assert!(claim(&t, "w.run", ttl).unwrap(), "from the future is not");
        assert!(!claim(&t, "w.run", ttl).unwrap(), "and the new one is");
    }

    #[test]
    fn exactly_one_of_many_simultaneous_claims_wins() {
        let t = Tmp::new("cache-claim-race");
        let dir = Arc::new(t.0.clone());
        let start = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (dir, start) = (dir.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    claim(&dir, "w.run", Duration::from_secs(5)).unwrap()
                })
            })
            .collect();
        let won = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(won, 1);
    }

    #[test]
    fn a_claim_that_cannot_be_recorded_is_an_error() {
        let t = Tmp::new("cache-claim-fail");
        t.write("blocker", "a file");
        assert!(claim(&t.join("blocker"), "w.run", Duration::from_secs(5)).is_err());
        std::os::unix::fs::symlink(t.join("nowhere"), t.join("dangling")).unwrap();
        assert!(claim(&t.join("dangling"), "w.run", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn key_is_stable_and_distinguishes_paths() {
        assert_eq!(key(b"/a/b"), key(b"/a/b"));
        assert_ne!(key(b"/a/b"), key(b"/a/c"));
        assert_eq!(key(b"").len(), 16);
        assert_eq!(key(b""), "cbf29ce484222325");
    }
}
