//! Repository facts for the status line.
//!
//! The branch and the linked-worktree name are read straight from `.git`, no
//! subprocess. The working-tree counts (staged, modified, untracked, ahead,
//! behind) need `git status`, so they are cached: a hit costs a handful of
//! `stat` calls, and `git` runs only when the cache is missing, malformed,
//! older than `TTL_SECS`, or keyed on files that have since changed, and then
//! only one render per repository runs it at a time.

use crate::cache;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// HEAD and gitfiles are one short line; anything past this is not one.
const MAX_READ_BYTES: u64 = 4096;
const SHORT_SHA_LEN: usize = 7;
/// Edits to tracked files and new untracked files touch none of the files the
/// cache is keyed on, so a hit also expires on age. The HUD kept its git
/// results for the same 30 seconds.
const TTL_SECS: u64 = 30;
/// `git status` in a huge repository can take longer than a status line may
/// block; the HUD gave up after the same second.
const GIT_TIMEOUT: Duration = Duration::from_secs(1);
/// A render that starts `git` leaves a marker for this long. Claude Code
/// cancels renders it has superseded, and one cancelled mid-`git` cannot clear
/// its marker or cache the answer; the `git` it started runs on regardless, so
/// the renders after it wait out the marker instead of each starting another.
const IN_FLIGHT_SECS: Duration = Duration::from_secs(5);
const CACHE_VERSION: u64 = 1;
/// Variables that point git at a repository: inherited from a hook or a
/// `rebase --exec`, they would make it report another repository's status.
const REPO_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// A repository found by walking up from a directory.
pub struct Repo {
    /// The directory holding `.git`.
    pub root: PathBuf,
    /// Per-worktree state: HEAD, index.
    git_dir: PathBuf,
    /// Shared state: refs, packed-refs. Equal to `git_dir` outside linked worktrees.
    common_dir: PathBuf,
    /// Name of a linked worktree (`.git/worktrees/<name>`), else `None`.
    pub worktree: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Head {
    Branch(String),
    /// First seven hex digits of the commit.
    Detached(String),
}

impl Head {
    /// What the status line prints after `branch:`.
    pub fn label(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Detached(sha) => format!("({sha})"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub ahead: u32,
    pub behind: u32,
}

impl Counts {
    pub fn is_clean(&self) -> bool {
        *self == Counts::default()
    }
}

/// The repository containing `dir`: the nearest ancestor with a `.git`.
///
/// A `.git` file (linked worktree, submodule) holds `gitdir: <path>`, and HEAD
/// is per-worktree, so it is read from there. Anything else named `.git` (a
/// FIFO, a socket) is not a repository and is never opened.
pub fn find(dir: &Path) -> Option<Repo> {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        let Ok(meta) = fs::metadata(&dot_git) else {
            continue;
        };
        let git_dir = if meta.is_dir() {
            dot_git
        } else if meta.is_file() {
            gitfile_target(&dot_git, ancestor)?
        } else {
            return None;
        };
        // A linked worktree names its main repository in `commondir`.
        let common = read_small(&git_dir.join("commondir"))
            .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
            .filter(|l| !l.is_empty());
        let (common_dir, worktree) = match common {
            Some(rel) => (
                git_dir.join(rel),
                git_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
            ),
            None => (git_dir.clone(), None),
        };
        return Some(Repo {
            root: ancestor.to_path_buf(),
            git_dir,
            common_dir,
            worktree,
        });
    }
    None
}

impl Repo {
    /// `None` when HEAD is unreadable, unrecognised, or the reftable stub.
    pub fn head(&self) -> Option<Head> {
        let text = read_small(&self.git_dir.join("HEAD"))?;
        let line = text.lines().next()?.trim();
        if let Some(reference) = line.strip_prefix("ref:") {
            let reference = reference.trim();
            // A reftable repository keeps this placeholder in HEAD; the real
            // ref lives in binary tables this reader does not parse.
            if reference == "refs/heads/.invalid" {
                return None;
            }
            let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            return (!name.is_empty()).then(|| Head::Branch(name.to_string()));
        }
        let is_sha = line.len() >= SHORT_SHA_LEN && line.bytes().all(|b| b.is_ascii_hexdigit());
        is_sha.then(|| Head::Detached(line[..SHORT_SHA_LEN].to_string()))
    }
}

/// A small regular file as text. A FIFO or socket would block the open, so
/// only a regular file is opened.
fn read_small(path: &Path) -> Option<String> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(MAX_READ_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The directory a `.git` file points at; a relative path is relative to the
/// directory holding the file.
fn gitfile_target(gitfile: &Path, base: &Path) -> Option<PathBuf> {
    let text = read_small(gitfile)?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    (!target.is_empty()).then(|| base.join(target))
}

/// Working-tree counts for `repo`, from the cache when it is still good.
/// `None` when `git` fails, times out, or there is nowhere to cache: without a
/// cache every render would fork `git`, so the counts are left out instead.
/// Also `None` while another render's `git` is running and nothing is cached.
pub fn status(
    repo: &Repo,
    head: Option<&Head>,
    cache_dir: Option<&Path>,
    now: f64,
) -> Option<Counts> {
    status_via(&["git"], GIT_TIMEOUT, repo, head, cache_dir?, now)
}

/// `status` with the `git` command line and the timeout injectable.
fn status_via(
    git: &[&str],
    timeout: Duration,
    repo: &Repo,
    head: Option<&Head>,
    cache_dir: &Path,
    now: f64,
) -> Option<Counts> {
    let key = cache::path_key(&repo.root);
    let name = format!("git-{key}.json");
    let marker = format!("git-{key}.run");
    let now = now_secs(now);
    let cached = cache::read(cache_dir, &name);
    if let Some(hit) = cached.as_ref().and_then(|doc| hit(doc, repo, head, now)) {
        return hit;
    }
    match cache::claim(cache_dir, &marker, IN_FLIGHT_SECS) {
        Ok(true) => {}
        // Another render is asking `git`, or a cancelled one started it and
        // it is still running: the old counts will do for now.
        Ok(false) => {
            return cached
                .as_ref()
                .and_then(|doc| counts_of(doc, repo))
                .flatten();
        }
        // With nowhere to record the answer every render would ask `git`.
        Err(_) => return None,
    }
    // The render that held the claim may have finished since the read above.
    if let Some(hit) = cache::read(cache_dir, &name).and_then(|doc| hit(&doc, repo, head, now)) {
        cache::release(cache_dir, &marker);
        return hit;
    }
    let ran = run_git(git, &repo.root, timeout);
    let upstream = ran.as_ref().and_then(|(_, up)| up.clone());
    let counts = ran.map(|(c, _)| c);
    let doc = json!({
        "v": CACHE_VERSION,
        "root": repo.root.to_string_lossy(),
        "at": now,
        "key": stamp(repo, head, upstream.as_deref()),
        "up": upstream,
        "c": counts.map(|c| [c.staged, c.modified, c.untracked, c.ahead, c.behind]),
    });
    let _ = cache::write(cache_dir, &name, &doc);
    cache::release(cache_dir, &marker);
    counts
}

fn now_secs(now: f64) -> u64 {
    if now.is_finite() && now > 0.0 {
        now as u64
    } else {
        0
    }
}

/// The cached counts if `doc` is a good, fresh entry for this repository.
/// The outer `None` is a miss; an inner `None` is a remembered failure.
fn hit(doc: &Value, repo: &Repo, head: Option<&Head>, now: u64) -> Option<Option<Counts>> {
    let counts = counts_of(doc, repo)?;
    // A clock that went backwards makes the entry untrustworthy, not young.
    if now.checked_sub(doc.get("at")?.as_u64()?)? >= TTL_SECS {
        return None;
    }
    let upstream = doc.get("up")?.as_str();
    let want: Vec<u64> = doc
        .get("key")?
        .as_array()?
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    (want == stamp(repo, head, upstream)).then_some(counts)
}

/// The counts `doc` holds, however old, if it is a well-formed entry for this
/// repository. The outer `None` is a miss; an inner `None` is a remembered
/// failure.
fn counts_of(doc: &Value, repo: &Repo) -> Option<Option<Counts>> {
    if doc.get("v")?.as_u64()? != CACHE_VERSION
        || doc.get("root")?.as_str()? != repo.root.to_string_lossy()
    {
        return None;
    }
    match doc.get("c")? {
        Value::Null => Some(None),
        c => {
            let n: Vec<u32> = c
                .as_array()?
                .iter()
                .map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect::<Option<_>>()?;
            let [staged, modified, untracked, ahead, behind] = n[..] else {
                return None;
            };
            Some(Some(Counts {
                staged,
                modified,
                untracked,
                ahead,
                behind,
            }))
        }
    }
}

/// Modification times (nanoseconds, 0 when absent) of what `git status`
/// depends on besides file contents: HEAD, the index, the branch's ref, its
/// upstream's ref, and packed-refs.
fn stamp(repo: &Repo, head: Option<&Head>, upstream: Option<&str>) -> [u64; 5] {
    let branch_ref = match head {
        Some(Head::Branch(name)) => Some(repo.common_dir.join(format!("refs/heads/{name}"))),
        _ => None,
    };
    let upstream_ref = upstream.map(|up| repo.common_dir.join(format!("refs/remotes/{up}")));
    [
        Some(repo.git_dir.join("HEAD")),
        Some(repo.git_dir.join("index")),
        branch_ref,
        upstream_ref,
        Some(repo.common_dir.join("packed-refs")),
    ]
    .map(|p| p.map_or(0, |p| mtime_nanos(&p)))
}

fn mtime_nanos(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Run `git status --porcelain -b` in `root`; the counts and the upstream ref
/// name (`origin/main`). `None` on a spawn failure, a non-zero exit, or `timeout`.
///
/// What a repository can make `git status` run is closed off here as far as
/// the counts allow. `core.fsmonitor` names a program that git would run on
/// every call: it is set empty, which git 2.36 and later read as `false` and
/// older ones, which only know a hook path, as no path (`false` would be a
/// program to run there; only 2.39 and 2.55 were run here). The
/// `post-index-change` hook
/// only runs when git writes the index, which `--no-optional-locks` forbids.
/// A clean or smudge filter (`filter.<name>.clean` with a matching
/// `.gitattributes` line) still runs for files whose stat data changed, since
/// `status` has to read them: that is what a shell prompt showing `git status`
/// exposes too, and no switch removes it without changing the counts.
fn run_git(git: &[&str], root: &Path, timeout: Duration) -> Option<(Counts, Option<String>)> {
    let (program, prefix) = git.split_first()?;
    let deadline = Instant::now() + timeout;
    let mut cmd = Command::new(program);
    cmd.args(prefix)
        .args([
            "-c",
            "core.fsmonitor=",
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-b",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(parse_status(BufReader::new(stdout)));
    });
    // End of output is not the end of git: it gets what is left of the
    // timeout to exit, and is killed if it does not.
    let finished = rx
        .recv_timeout(timeout)
        .ok()
        .and_then(|parsed| Some((parsed, wait_until(&mut child, deadline)?)))
        .filter(|(_, status)| status.success());
    match finished {
        Some((parsed, _)) => Some(parsed),
        None => {
            let _ = child.kill();
            reap(child);
            None
        }
    }
}

/// The exit status of `child` if it exits by `deadline`.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Collect a killed child on a thread of its own. Waiting here would hold the
/// render for as long as a `git` stuck in an uninterruptible call takes to die.
fn reap(mut child: Child) {
    let _ = std::thread::Builder::new().spawn(move || {
        let _ = child.wait();
    });
}

/// Count `git status --porcelain -b` lines the way the HUD did: `?` in the
/// first column is untracked; any other non-blank first column is staged; `M`
/// or `D` in the second is modified.
fn parse_status(reader: impl BufRead) -> (Counts, Option<String>) {
    let mut counts = Counts::default();
    let mut upstream = None;
    for (n, line) in reader.split(b'\n').map_while(Result::ok).enumerate() {
        if n == 0 {
            let line = String::from_utf8_lossy(&line);
            counts.ahead = number_after(&line, "ahead ");
            counts.behind = number_after(&line, "behind ");
            upstream = line
                .strip_prefix("## ")
                .and_then(|rest| rest.split_once("..."))
                .and_then(|(_, tail)| tail.split(' ').next())
                .filter(|up| !up.is_empty())
                .map(str::to_string);
            continue;
        }
        let [index, worktree, ..] = line[..] else {
            continue;
        };
        if index == b'?' {
            counts.untracked = counts.untracked.saturating_add(1);
        } else {
            if index != b' ' {
                counts.staged = counts.staged.saturating_add(1);
            }
            if worktree == b'M' || worktree == b'D' {
                counts.modified = counts.modified.saturating_add(1);
            }
        }
    }
    (counts, upstream)
}

/// The number after `key` where `key` starts a word and digits follow
/// (`ahead 3`), else 0. The HUD's `\bahead (\d+)`.
fn number_after(line: &str, key: &str) -> u32 {
    line.match_indices(key)
        .filter(|(at, _)| {
            line[..*at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
        .find_map(|(at, _)| {
            let rest = &line[at + key.len()..];
            let end = rest.bytes().take_while(u8::is_ascii_digit).count();
            (end > 0).then(|| rest[..end].parse().unwrap_or(u32::MAX))
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::testing::Tmp;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Branch label as the status line prints it.
    fn branch(dir: &Path) -> Option<String> {
        find(dir)?.head().map(|h| h.label())
    }

    #[test]
    fn branch_from_repo_root_and_nested_dir() {
        let t = Tmp::new("plain");
        t.write("repo/.git/HEAD", "ref: refs/heads/main\n");
        fs::create_dir_all(t.0.join("repo/a/b")).unwrap();
        assert_eq!(branch(&t.0.join("repo")).as_deref(), Some("main"));
        assert_eq!(branch(&t.0.join("repo/a/b")).as_deref(), Some("main"));
    }

    #[test]
    fn branch_keeps_slashes_in_name() {
        let t = Tmp::new("slash");
        t.write("repo/.git/HEAD", "ref: refs/heads/feat/fast-status\n");
        assert_eq!(
            branch(&t.0.join("repo")).as_deref(),
            Some("feat/fast-status")
        );
    }

    #[test]
    fn linked_worktree_with_absolute_gitdir() {
        let t = Tmp::new("worktree-abs");
        t.write("main/.git/HEAD", "ref: refs/heads/main\n");
        let wt_git = t.0.join("main/.git/worktrees/wt");
        t.write("main/.git/worktrees/wt/HEAD", "ref: refs/heads/feature\n");
        t.write("wt/.git", &format!("gitdir: {}\n", wt_git.display()));
        fs::create_dir_all(t.0.join("wt/src")).unwrap();
        assert_eq!(branch(&t.0.join("wt")).as_deref(), Some("feature"));
        assert_eq!(branch(&t.0.join("wt/src")).as_deref(), Some("feature"));
    }

    #[test]
    fn gitfile_with_relative_gitdir() {
        let t = Tmp::new("worktree-rel");
        t.write("main/.git/worktrees/wt/HEAD", "ref: refs/heads/rel\n");
        t.write("main/sub/.git", "gitdir: ../.git/worktrees/wt\n");
        assert_eq!(branch(&t.0.join("main/sub")).as_deref(), Some("rel"));
    }

    #[test]
    fn linked_worktree_is_named_but_a_submodule_or_main_checkout_is_not() {
        let t = Tmp::new("worktree-name");
        t.write("main/.git/HEAD", "ref: refs/heads/main\n");
        t.write("main/.git/worktrees/wt-one/HEAD", "ref: refs/heads/f\n");
        t.write("main/.git/worktrees/wt-one/commondir", "../..\n");
        t.write("wt/.git", "gitdir: ../main/.git/worktrees/wt-one\n");
        t.write("main/.git/modules/sub/HEAD", "ref: refs/heads/s\n");
        t.write("main/sub/.git", "gitdir: ../.git/modules/sub\n");
        assert_eq!(
            find(&t.0.join("wt")).unwrap().worktree.as_deref(),
            Some("wt-one")
        );
        assert_eq!(find(&t.0.join("main")).unwrap().worktree, None);
        assert_eq!(find(&t.0.join("main/sub")).unwrap().worktree, None);
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let t = Tmp::new("detached");
        t.write("repo/.git/HEAD", &format!("{SHA}\n"));
        assert_eq!(branch(&t.0.join("repo")).as_deref(), Some("(0123456)"));
    }

    #[test]
    fn detached_head_in_worktree() {
        let t = Tmp::new("detached-wt");
        t.write("main/.git/worktrees/wt/HEAD", &format!("{SHA}\n"));
        t.write("wt/.git", "gitdir: ../main/.git/worktrees/wt\n");
        assert_eq!(branch(&t.0.join("wt")).as_deref(), Some("(0123456)"));
    }

    #[test]
    fn nearest_git_wins_over_outer_repo() {
        let t = Tmp::new("nested");
        t.write(".git/HEAD", "ref: refs/heads/outer\n");
        t.write("inner/.git/HEAD", "ref: refs/heads/inner\n");
        assert_eq!(branch(&t.0.join("inner")).as_deref(), Some("inner"));
    }

    #[test]
    fn no_repo_is_none() {
        let t = Tmp::new("norepo");
        fs::create_dir_all(t.0.join("a/b")).unwrap();
        assert_eq!(branch(&t.0.join("a/b")), None);
    }

    #[test]
    fn missing_dir_is_none() {
        let t = Tmp::new("missing");
        assert_eq!(branch(&t.0.join("does/not/exist")), None);
    }

    #[test]
    fn garbage_head_is_none() {
        let t = Tmp::new("garbage-head");
        t.write("repo/.git/HEAD", "not a ref and not a sha\n");
        assert_eq!(branch(&t.0.join("repo")), None);
        t.write("repo/.git/HEAD", "");
        assert_eq!(branch(&t.0.join("repo")), None);
        t.write("repo/.git/HEAD", "ref:\n");
        assert_eq!(branch(&t.0.join("repo")), None);
        t.write("repo/.git/HEAD", "abc123\n");
        assert_eq!(branch(&t.0.join("repo")), None, "too short for a sha");
    }

    #[test]
    fn invalid_utf8_head_is_none() {
        let t = Tmp::new("binary-head");
        let head = t.write("repo/.git/HEAD", "");
        fs::write(head, [0xff, 0xfe, 0x00, 0x80]).unwrap();
        assert_eq!(branch(&t.0.join("repo")), None);
    }

    #[test]
    fn garbage_gitfile_is_none() {
        let t = Tmp::new("garbage-gitfile");
        t.write("wt/.git", "this is not a gitfile\n");
        assert_eq!(branch(&t.0.join("wt")), None);
        t.write("wt/.git", "gitdir:\n");
        assert_eq!(branch(&t.0.join("wt")), None);
    }

    #[test]
    fn gitfile_pointing_nowhere_is_none() {
        let t = Tmp::new("dangling-gitfile");
        t.write("wt/.git", "gitdir: ../gone\n");
        assert_eq!(branch(&t.0.join("wt")), None);
    }

    #[test]
    fn oversized_head_is_bounded_not_trusted() {
        let t = Tmp::new("huge-head");
        let long = "x".repeat(1 << 20);
        t.write("repo/.git/HEAD", &format!("ref: refs/heads/{long}\n"));
        let got = branch(&t.0.join("repo")).unwrap();
        assert!(got.len() <= MAX_READ_BYTES as usize);
    }

    /// Run `f` on another thread and fail instead of hanging if it blocks.
    fn within_five_seconds<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("blocked on a non-regular file")
    }

    fn mkfifo(path: &Path) -> bool {
        Command::new("mkfifo")
            .arg(path)
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn fifo_as_dot_git_does_not_block() {
        let t = Tmp::new("fifo-dotgit");
        fs::create_dir_all(t.join("repo")).unwrap();
        if !mkfifo(&t.join("repo/.git")) {
            return;
        }
        let dir = t.0.join("repo");
        assert_eq!(within_five_seconds(move || branch(&dir)), None);
    }

    #[test]
    fn fifo_as_head_does_not_block() {
        let t = Tmp::new("fifo-head");
        fs::create_dir_all(t.join("repo/.git")).unwrap();
        if !mkfifo(&t.join("repo/.git/HEAD")) {
            return;
        }
        let dir = t.0.join("repo");
        assert_eq!(within_five_seconds(move || branch(&dir)), None);
    }

    #[test]
    fn fifo_as_gitfile_target_head_does_not_block() {
        let t = Tmp::new("fifo-wt-head");
        fs::create_dir_all(t.join("main/.git/worktrees/wt")).unwrap();
        if !mkfifo(&t.join("main/.git/worktrees/wt/HEAD")) {
            return;
        }
        t.write("wt/.git", "gitdir: ../main/.git/worktrees/wt\n");
        let dir = t.0.join("wt");
        assert_eq!(within_five_seconds(move || branch(&dir)), None);
    }

    #[test]
    fn fifo_as_commondir_does_not_block() {
        let t = Tmp::new("fifo-commondir");
        t.write("repo/.git/HEAD", "ref: refs/heads/main\n");
        if !mkfifo(&t.join("repo/.git/commondir")) {
            return;
        }
        let dir = t.0.join("repo");
        assert_eq!(
            within_five_seconds(move || branch(&dir)).as_deref(),
            Some("main")
        );
    }

    #[test]
    fn reftable_stub_head_is_no_branch() {
        let t = Tmp::new("reftable");
        t.write("repo/.git/HEAD", "ref: refs/heads/.invalid\n");
        assert_eq!(branch(&t.0.join("repo")), None);
        // A real branch that merely starts with a dot elsewhere is unaffected.
        t.write("repo/.git/HEAD", "ref: refs/heads/invalid\n");
        assert_eq!(branch(&t.0.join("repo")).as_deref(), Some("invalid"));
    }

    #[test]
    fn only_the_first_line_of_head_counts() {
        let t = Tmp::new("head-lines");
        t.write(
            "repo/.git/HEAD",
            "ref: refs/heads/main\nref: refs/heads/other\n",
        );
        assert_eq!(branch(&t.0.join("repo")).as_deref(), Some("main"));
        t.write("repo/.git/HEAD", &format!("{SHA}\nref: refs/heads/other\n"));
        assert_eq!(branch(&t.0.join("repo")).as_deref(), Some("(0123456)"));
        // Junk on the first line is not rescued by a good second line.
        t.write("repo/.git/HEAD", "junk\nref: refs/heads/main\n");
        assert_eq!(branch(&t.0.join("repo")), None);
        // A blank first line is a first line.
        t.write("repo/.git/HEAD", "\nref: refs/heads/main\n");
        assert_eq!(branch(&t.0.join("repo")), None);
    }

    // ---- status parsing ----

    fn parse(text: &str) -> (Counts, Option<String>) {
        parse_status(text.as_bytes())
    }

    #[test]
    fn status_counts_like_the_hud() {
        let (c, up) = parse(
            "## main...origin/main [ahead 5, behind 2]\n M a\nM  b\nMM c\n?? d\n?? e\nA  f\n D g\n D h\nR  i -> j\n T k\nUU l\n",
        );
        // staged: M  b, MM c, A  f, R  i, UU l; modified: " M a", MM c, " D g", " D h".
        // " T k" counts as neither; the HUD only knew M and D in the second column.
        assert_eq!(
            c,
            Counts {
                staged: 5,
                modified: 4,
                untracked: 2,
                ahead: 5,
                behind: 2
            }
        );
        assert_eq!(up.as_deref(), Some("origin/main"));
    }

    #[test]
    fn branch_line_forms() {
        assert_eq!(parse("## main...origin/main\n").0, Counts::default());
        assert_eq!(parse("## main...origin/main [ahead 3]\n").0.ahead, 3);
        assert_eq!(parse("## main...origin/main [behind 7]\n").0.behind, 7);
        let (c, up) = parse("## main...origin/main [ahead 2, behind 1]\n");
        assert_eq!(
            (c.ahead, c.behind, up.as_deref()),
            (2, 1, Some("origin/main"))
        );
        let (c, up) = parse("## main...origin/main [gone]\n");
        assert_eq!((c.ahead, c.behind), (0, 0));
        assert_eq!(up.as_deref(), Some("origin/main"));
        let (c, up) = parse("## No commits yet on main\n?? a\n");
        assert_eq!((c.untracked, c.ahead), (1, 0));
        assert_eq!(up, None);
        assert_eq!(parse("## HEAD (no branch)\n").1, None);
        assert_eq!(
            parse("## a/b...up/stream/x [ahead 1]\n").1.as_deref(),
            Some("up/stream/x")
        );
    }

    #[test]
    fn ahead_inside_a_word_is_not_a_count() {
        assert_eq!(
            number_after("## go-ahead...origin/go-ahead [ahead 4]", "ahead "),
            4
        );
        assert_eq!(number_after("## x...o/x [behind 2]", "ahead "), 0);
        assert_eq!(number_after("## x...o/x [ahead ]", "ahead "), 0);
        assert_eq!(number_after("## xahead 9", "ahead "), 0);
    }

    #[test]
    fn empty_and_short_lines_do_not_count() {
        let (c, _) = parse("## main\n\n?\nM\n M x\n");
        assert_eq!(
            c,
            Counts {
                modified: 1,
                ..Counts::default()
            }
        );
        assert_eq!(parse("").0, Counts::default());
    }

    #[test]
    fn non_utf8_paths_still_count() {
        let mut out = b"## main\n?? caf\xe9\n M \xff\xfe\n".to_vec();
        out.extend_from_slice(b"A  z\n");
        let (c, _) = parse_status(&out[..]);
        assert_eq!((c.untracked, c.modified, c.staged), (1, 1, 1));
    }

    #[test]
    fn clean_means_all_zero() {
        assert!(Counts::default().is_clean());
        assert!(
            !Counts {
                ahead: 1,
                ..Counts::default()
            }
            .is_clean()
        );
    }

    // ---- the status cache, with a stub in place of git ----

    /// A repo with files for every key the cache watches, and a stub `git`
    /// that logs each run and prints a fixed status.
    struct Rig {
        t: Tmp,
        repo: Repo,
        log: PathBuf,
        script: PathBuf,
    }

    impl Rig {
        fn new(name: &str, body: &str) -> Rig {
            let t = Tmp::new(name);
            t.write("repo/.git/HEAD", "ref: refs/heads/main\n");
            t.write("repo/.git/index", "i");
            t.write("repo/.git/refs/heads/main", "a\n");
            t.write("repo/.git/refs/remotes/origin/main", "b\n");
            let log = t.0.join("runs.log");
            let script = t.write(
                "git.sh",
                &format!("echo run >> '{}'\n{body}\n", log.display()),
            );
            let repo = find(&t.0.join("repo")).unwrap();
            Rig {
                t,
                repo,
                log,
                script,
            }
        }

        fn ok(name: &str) -> Rig {
            Rig::new(
                name,
                "printf '## main...origin/main [ahead 2, behind 1]\\n M a\\nM  b\\n?? c\\n'",
            )
        }

        fn runs(&self) -> usize {
            fs::read_to_string(&self.log).map_or(0, |s| s.lines().count())
        }

        fn status_at(&self, now: f64) -> Option<Counts> {
            self.status_timeout(now, GIT_TIMEOUT)
        }

        fn status_timeout(&self, now: f64, timeout: Duration) -> Option<Counts> {
            let head = self.repo.head();
            let script = self.script.to_str().unwrap();
            status_via(
                &["sh", script],
                timeout,
                &self.repo,
                head.as_ref(),
                &self.t.join("cache"),
                now,
            )
        }

        fn touch(&self, rel: &str, secs_after_epoch: u64) {
            let f = File::options().write(true).open(self.t.join(rel)).unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
                .unwrap();
        }
    }

    const COUNTS: Counts = Counts {
        staged: 1,
        modified: 1,
        untracked: 1,
        ahead: 2,
        behind: 1,
    };

    #[test]
    fn warm_cache_does_not_run_git() {
        let rig = Rig::ok("git-warm");
        assert_eq!(rig.status_at(1000.0), Some(COUNTS));
        assert_eq!(rig.runs(), 1);
        for later in [1000.0, 1010.5, 1029.9] {
            assert_eq!(rig.status_at(later), Some(COUNTS));
        }
        assert_eq!(
            rig.runs(),
            1,
            "three more renders inside the TTL, no new run"
        );
    }

    #[test]
    fn expired_ttl_runs_git_again() {
        let rig = Rig::ok("git-ttl");
        rig.status_at(1000.0);
        assert_eq!(rig.status_at(1030.0), Some(COUNTS));
        assert_eq!(rig.runs(), 2, "30 s old is stale");
        rig.status_at(1031.0);
        assert_eq!(rig.runs(), 2, "the refresh re-armed the TTL");
    }

    #[test]
    fn clock_going_backwards_is_not_a_hit() {
        let rig = Rig::ok("git-clock");
        rig.status_at(5000.0);
        rig.status_at(100.0);
        assert_eq!(rig.runs(), 2);
    }

    #[test]
    fn each_watched_file_invalidates_the_cache() {
        let rig = Rig::ok("git-keys");
        rig.status_at(1000.0);
        let mut runs = 1;
        for (i, rel) in [
            "repo/.git/index",
            "repo/.git/HEAD",
            "repo/.git/refs/heads/main",
            "repo/.git/refs/remotes/origin/main",
        ]
        .into_iter()
        .enumerate()
        {
            rig.touch(rel, 1_700_000_000 + i as u64);
            rig.status_at(1000.0);
            runs += 1;
            assert_eq!(rig.runs(), runs, "{rel} changed");
            rig.status_at(1000.0);
            assert_eq!(rig.runs(), runs, "{rel} settled");
        }
        rig.t.write("repo/.git/packed-refs", "# pack-refs\n");
        rig.status_at(1000.0);
        assert_eq!(rig.runs(), runs + 1, "packed-refs appeared");
    }

    #[test]
    fn corrupt_cache_recomputes_and_is_repaired() {
        let rig = Rig::ok("git-corrupt");
        rig.status_at(1000.0);
        let cache_file = fs::read_dir(rig.t.join("cache"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        for junk in [
            "{broken",
            "",
            "null",
            "[]",
            r#"{"v":1}"#,
            r#"{"v":99,"root":"x"}"#,
        ] {
            fs::write(&cache_file, junk).unwrap();
            let before = rig.runs();
            assert_eq!(rig.status_at(1000.0), Some(COUNTS), "{junk:?}");
            assert_eq!(rig.runs(), before + 1, "{junk:?} must recompute");
            let again = rig.runs();
            rig.status_at(1000.0);
            assert_eq!(rig.runs(), again, "{junk:?} repaired");
        }
    }

    #[test]
    fn well_formed_entry_with_wrong_shape_is_a_miss() {
        let rig = Rig::ok("git-shape");
        rig.status_at(1000.0);
        let cache_file = fs::read_dir(rig.t.join("cache"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let good: Value = serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
        for (field, bad) in [
            ("c", json!([1, 2, 3])),
            ("c", json!(["a", "b", "c", "d", "e"])),
            ("c", json!([1, 2, 3, 4, -5])),
            ("c", json!({"staged": 1})),
            ("key", json!("x")),
            ("key", json!([1, 2])),
            ("at", json!("soon")),
            ("up", json!(5)),
            ("root", json!("/elsewhere")),
        ] {
            let mut doc = good.clone();
            doc[field] = bad.clone();
            fs::write(&cache_file, doc.to_string()).unwrap();
            let before = rig.runs();
            assert_eq!(rig.status_at(1000.0), Some(COUNTS), "{field}={bad}");
            assert_eq!(rig.runs(), before + 1, "{field}={bad} must recompute");
        }
    }

    #[test]
    fn failed_git_is_remembered_for_the_ttl() {
        let rig = Rig::new("git-fail", "exit 128");
        assert_eq!(rig.status_at(1000.0), None);
        assert_eq!(rig.status_at(1005.0), None);
        assert_eq!(
            rig.runs(),
            1,
            "a failure is cached, not retried every render"
        );
        assert_eq!(rig.status_at(1031.0), None);
        assert_eq!(rig.runs(), 2);
    }

    #[test]
    fn slow_git_is_cut_off_and_remembered() {
        let rig = Rig::new("git-slow", "exec sleep 5");
        let start = std::time::Instant::now();
        assert_eq!(rig.status_timeout(1000.0, Duration::from_millis(200)), None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let again = std::time::Instant::now();
        assert_eq!(rig.status_timeout(1001.0, Duration::from_millis(200)), None);
        assert!(
            again.elapsed() < Duration::from_millis(150),
            "second call is a cache hit"
        );
        assert_eq!(rig.runs(), 1);
    }

    #[test]
    fn missing_git_binary_is_none() {
        let t = Tmp::new("git-missing");
        t.write("repo/.git/HEAD", "ref: refs/heads/main\n");
        let repo = find(&t.0.join("repo")).unwrap();
        let got = status_via(
            &["/nonexistent/git"],
            GIT_TIMEOUT,
            &repo,
            repo.head().as_ref(),
            &t.join("cache"),
            1000.0,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn no_cache_dir_means_no_counts_and_no_fork() {
        let rig = Rig::ok("git-nocache");
        let head = rig.repo.head();
        assert_eq!(status(&rig.repo, head.as_ref(), None, 1000.0), None);
        assert_eq!(rig.runs(), 0);
    }

    #[test]
    fn unwritable_cache_means_no_counts_and_no_fork() {
        let rig = Rig::ok("git-unwritable");
        rig.t.write("blocker", "a file");
        let head = rig.repo.head();
        let script = rig.script.to_str().unwrap();
        let got = status_via(
            &["sh", script],
            GIT_TIMEOUT,
            &rig.repo,
            head.as_ref(),
            &rig.t.join("blocker"),
            1000.0,
        );
        assert_eq!(got, None);
        assert_eq!(rig.runs(), 0, "nowhere to remember the answer: no fork");
    }

    #[test]
    fn upstream_ref_is_watched_only_once_known() {
        // No upstream in the first run: the remote ref is not part of the key.
        let rig = Rig::new("git-noup", "printf '## main\\n?? a\\n'");
        rig.status_at(1000.0);
        rig.touch("repo/.git/refs/remotes/origin/main", 1_700_000_001);
        rig.status_at(1000.0);
        assert_eq!(rig.runs(), 1);
    }

    #[test]
    fn linked_worktree_watches_its_own_head_and_the_shared_refs() {
        let t = Tmp::new("git-wt");
        t.write("main/.git/refs/heads/feat", "a\n");
        t.write("main/.git/worktrees/wt/HEAD", "ref: refs/heads/feat\n");
        t.write("main/.git/worktrees/wt/index", "i");
        t.write("main/.git/worktrees/wt/commondir", "../..\n");
        t.write("wt/.git", "gitdir: ../main/.git/worktrees/wt\n");
        let log = t.0.join("runs.log");
        let script = t.write(
            "git.sh",
            &format!("echo run >> '{}'\nprintf '## feat\\n'", log.display()),
        );
        let repo = find(&t.0.join("wt")).unwrap();
        let head = repo.head();
        let run = |now: f64| {
            status_via(
                &["sh", script.to_str().unwrap()],
                GIT_TIMEOUT,
                &repo,
                head.as_ref(),
                &t.join("cache"),
                now,
            )
        };
        run(1000.0);
        run(1000.0);
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        let f = File::options()
            .write(true)
            .open(t.join("main/.git/refs/heads/feat"))
            .unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
        run(1000.0);
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().count(),
            2,
            "shared branch ref moved"
        );
    }

    #[test]
    fn concurrent_renders_agree_and_leave_a_valid_cache() {
        let rig = Arc::new(Rig::ok("git-concurrent"));
        let start = Arc::new(Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (rig, start) = (rig.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    (0..5).map(|_| rig.status_at(1000.0)).collect::<Vec<_>>()
                })
            })
            .collect();
        // A render that finds git running with nothing cached yet has no
        // counts to show; none shows wrong ones.
        let mut shown = 0;
        for h in handles {
            for got in h.join().unwrap() {
                assert!(got.is_none() || got == Some(COUNTS), "{got:?}");
                shown += usize::from(got.is_some());
            }
        }
        assert!(shown > 0);
        let before = rig.runs();
        assert_eq!(rig.status_at(1000.0), Some(COUNTS));
        assert_eq!(
            rig.runs(),
            before,
            "the cache the race left behind is a hit"
        );
    }

    // ---- what git is allowed to do ----

    /// Run a real `git` in `dir`, as far from the caller's environment as the
    /// tests can put it. `false` when there is none or it failed.
    fn real_git(dir: &Path, args: &[&str]) -> bool {
        let mut cmd = Command::new("git");
        cmd.args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for var in REPO_ENV {
            cmd.env_remove(var);
        }
        cmd.status().is_ok_and(|s| s.success())
    }

    #[test]
    fn a_repository_fsmonitor_program_is_not_run() {
        let t = Tmp::new("git-fsmonitor");
        let root = t.0.join("repo");
        fs::create_dir_all(&root).unwrap();
        let marker = t.0.join("fsmonitor.ran");
        let hook = t.write(
            "fsmonitor.sh",
            &format!(
                "#!/bin/sh\necho ran >> '{}'\nprintf '\\0'\n",
                marker.display()
            ),
        );
        fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
        let set_up = real_git(&root, &["init", "-q"])
            && fs::write(root.join("a"), "1").is_ok()
            && real_git(&root, &["add", "a"])
            && real_git(&root, &["commit", "-q", "-m", "one"])
            && fs::write(root.join("a"), "22").is_ok()
            && fs::write(root.join("b"), "x").is_ok()
            && real_git(&root, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        if !set_up {
            return;
        }
        // The setup has to make a plain `git status` run the program, or the
        // test proves nothing about ours.
        assert!(real_git(
            &root,
            &["--no-optional-locks", "status", "--porcelain"]
        ));
        if !marker.exists() {
            return;
        }
        fs::remove_file(&marker).unwrap();
        let (counts, _) = run_git(&["git"], &root, Duration::from_secs(10)).expect("git status");
        assert!(!marker.exists(), "the repository's fsmonitor program ran");
        assert_eq!((counts.modified, counts.untracked), (1, 1));
    }

    #[test]
    fn git_that_closes_stdout_but_lingers_is_cut_off_at_the_timeout() {
        // EOF on the pipe is not the end of the process: waiting for it
        // without a limit would hang the render.
        let rig = Rig::new("git-lingers", "exec >&-\nexec sleep 5");
        let started = std::time::Instant::now();
        assert_eq!(rig.status_timeout(1000.0, Duration::from_millis(300)), None);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    // ---- one git at a time ----

    #[test]
    fn a_fresh_marker_keeps_a_second_git_from_starting() {
        let rig = Rig::ok("git-marker");
        let name = format!("git-{}.run", cache::path_key(&rig.repo.root));
        let marker = rig.t.join("cache").join(&name);
        // Nothing cached and a render already running git: no counts, no fork.
        fs::create_dir_all(rig.t.join("cache")).unwrap();
        fs::write(&marker, "1").unwrap();
        assert_eq!(rig.status_at(1000.0), None);
        assert_eq!(rig.runs(), 0);
        // Taken over once it is old enough to be a cancelled render's.
        let age = |secs: u64| {
            File::options()
                .write(true)
                .open(&marker)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
        };
        age(10);
        assert_eq!(rig.status_at(1000.0), Some(COUNTS));
        assert_eq!(rig.runs(), 1);
        assert!(!marker.exists(), "released when the answer is in");
        // Entry expired, marker fresh: the old counts, still no fork.
        fs::write(&marker, "1").unwrap();
        assert_eq!(rig.status_at(1040.0), Some(COUNTS));
        assert_eq!(rig.runs(), 1);
        age(10);
        rig.status_at(1040.0);
        assert_eq!(rig.runs(), 2);
        assert!(!marker.exists());
    }

    #[test]
    fn a_failed_git_releases_its_marker_too() {
        let rig = Rig::new("git-marker-fail", "exit 128");
        assert_eq!(rig.status_at(1000.0), None);
        let leftovers: Vec<_> = fs::read_dir(rig.t.join("cache"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".run"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn renders_that_find_git_running_use_the_old_counts_and_do_not_fork() {
        let rig = Arc::new(Rig::new(
            "git-single-flight",
            "sleep 0.5\nprintf '## main...origin/main [ahead 2, behind 1]\\n M a\\nM  b\\n?? c\\n'",
        ));
        let patient = Duration::from_secs(10);
        assert_eq!(rig.status_timeout(1000.0, patient), Some(COUNTS));
        assert_eq!(rig.runs(), 1);
        // The entry has expired and six renders arrive at once.
        let start = Arc::new(Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (rig, start) = (rig.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    rig.status_timeout(1040.0, patient)
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), Some(COUNTS));
        }
        assert_eq!(rig.runs(), 2, "one git for the six of them");
    }
}
