# sharkline

[![CI](https://github.com/spicysharkco/sharkline/actions/workflows/ci.yml/badge.svg)](https://github.com/spicysharkco/sharkline/actions/workflows/ci.yml)
[![License: PolyForm Noncommercial 1.0.0](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue)](LICENSE)

A fast Claude Code statusline: a status line for
[Claude Code](https://docs.claude.com/en/docs/claude-code/overview), written in
Rust. Claude Code pipes a JSON payload to the command on stdin; this
binary prints a compact HUD: git branch and counts, model, rate-limit bars,
context usage, session time and tool/agent counts.

A warm render takes about 1.3 ms, starts no other process, and touches no
network.

Unofficial; not affiliated with Anthropic or the oh-my-claudecode project.

## Why

Claude Code runs the status line command again as a session goes on; the
[status line documentation](https://docs.claude.com/en/docs/claude-code/statusline)
says when. A status line written for Node, Python or a shell script with `jq`
and `git` pays for a process start (or several) on every one of those renders. On the
machine used for the numbers below, a Node process that does nothing
(`node -e 0`) takes about 30 ms; this binary renders a full line in about 1.3 ms.

How it gets there:

- The branch is read from `.git/HEAD`. `git status` runs only when its cached
  answer is stale (30 s or a changed file time), and never twice at once for the
  same repository.
- The transcript is read once (the last 4 MiB). Later renders read only the
  bytes appended since.
- Versions of Claude Code and oh-my-claudecode (OMC) come from a cache file OMC
  already keeps. The binary never makes a network request.

The layout follows the "focused" preset of the
[oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode) HUD, which
is a Node program. This is a separate implementation of the parts that can be
computed without running state; see "Differences from the OMC HUD".

## Sample output

```sh
SHARKLINE_NOW=1750000000 sharkline < tests/fixtures/stdin.json
```

```text
Model: Opus 5.5 xhigh | 5h:[####----]55%(2h20m) wk:[#######-]88%(6d6h) | session:95m | ctx:[###-------]30%
```

(No OMC installed, so there are no OMC segments or update hints. The fixture's
directory does not exist, so there is no git line. The real output is
colored; `NO_COLOR=1` removes the escapes.)

At most four lines are printed, wrapped at ` | ` to `COLUMNS` (Claude Code sets
it for the command). Anything beyond four lines becomes `... (+N lines)`.

## Install

### Prebuilt binary

No Rust toolchain is needed. From the
[Releases page](https://github.com/spicysharkco/sharkline/releases/latest), download
the archive for your system and the `SHA256SUMS` file:

| System | Archive |
|---|---|
| macOS, Apple silicon | `sharkline-VERSION-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `sharkline-VERSION-x86_64-apple-darwin.tar.gz` |
| Linux, x86-64 | `sharkline-VERSION-x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM64 | `sharkline-VERSION-aarch64-unknown-linux-musl.tar.gz` |

The Linux binaries are statically linked, so they run on any distribution. Check the
archive, unpack it and put the binary on your `PATH` (the example is version 0.1.0 for
Apple silicon):

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
gh attestation verify sharkline-0.1.0-aarch64-apple-darwin.tar.gz --repo spicysharkco/sharkline
tar -xzf sharkline-0.1.0-aarch64-apple-darwin.tar.gz
install -m 755 sharkline-0.1.0-aarch64-apple-darwin/sharkline ~/.local/bin/
```

`shasum` checks that the download is intact. `gh attestation verify` (GitHub CLI)
checks that GitHub Actions built the archive from this repository. Use the path you
installed to (here `~/.local/bin/sharkline`) as the `command` in "Configure Claude
Code" below. If you use `cargo-binstall`,
`cargo binstall --git https://github.com/spicysharkco/sharkline sharkline` fetches the
same archive.

The macOS binaries are not notarised by Apple. If a browser download is blocked ("cannot
be opened because the developer cannot be verified"), clear the quarantine flag on the
unpacked directory with `xattr -dr com.apple.quarantine sharkline-0.1.0-aarch64-apple-darwin`.
A download made with `curl` or `gh release download` is not flagged.

### With cargo

You need Rust 1.89 or newer (the crate uses edition 2024).

From Git:

```sh
cargo install --git https://github.com/spicysharkco/sharkline
# or, to use the committed Cargo.lock:
cargo install --locked --git https://github.com/spicysharkco/sharkline
```

From source:

```sh
git clone https://github.com/spicysharkco/sharkline
cd sharkline
cargo build --locked --release
install -m 755 target/release/sharkline ~/.local/bin/
```

`cargo install` puts the binary in `~/.cargo/bin/sharkline`. The crate
is not published to crates.io.

## Configure Claude Code

Add a `statusLine` entry to `~/.claude/settings.json` (or a project's
`.claude/settings.json`). The `command` is run in a shell and may start with
`~`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.cargo/bin/sharkline"
  }
}
```

Claude Code reloads settings on save. `padding` and `refreshInterval` are
optional Claude Code settings; see the
[status line documentation](https://docs.claude.com/en/docs/claude-code/statusline).
To go back, delete the `statusLine` entry.

To try it without touching settings, pipe a payload in by hand:

```sh
sharkline < tests/fixtures/stdin.json
```

## Environment variables

| Variable | Effect |
|---|---|
| `NO_COLOR` | Set and non-empty: no ANSI escapes. Set but empty: color stays on. |
| `COLUMNS` | Claude Code sets it for the command. A positive number wraps lines at separators to that width and cuts a line that still does not fit with `...`. Absent, zero or not a number: no fitting. |
| `CLAUDE_CONFIG_DIR` | Where OMC's files are looked up (default `~/.claude`), and the source of the `profile:` segment. |
| `OMC_PLUGIN_ROOT` | OMC plugin root to read the OMC version from. |
| `XDG_CACHE_HOME` | Cache location (used only when absolute). Default `~/.cache`. |
| `SHARKLINE_NOW` | Unix seconds (fractions allowed) that replace the clock: countdowns, the 30 s thinking window and the git cache age. For reproducible output and tests. An unparsable value is ignored. |

## Segments

A segment whose data is missing is left out; a line with no segments is left
out. `session:` and `ctx:` always print.

| Segment | Source | Shown when |
|---|---|---|
| `branch:` `(wt:name)` | `.git/HEAD`, found from `workspace.current_dir`, else `cwd`, else the process directory. `(wt:name)` marks a linked worktree. A detached HEAD shows `(abc1234)`. | in a git repository |
| `+` `!` `?` `⇡` `⇣` | `git status`, cached: staged, modified, untracked, ahead, behind | the count is not zero |
| `profile:` | name of `CLAUDE_CONFIG_DIR`, leading dot dropped (`~/.claude-work` is `claude-work`) | the variable is set |
| `[OMC#v]` | `package.json` of the installed OMC plugin; `L` marks a development checkout; `-> <latest> omc update` when the update cache holds a newer version | OMC is installed |
| `Model:` | `model.display_name` (else `model.id`) as `Opus 5.5`, then `effort.level` | a model or effort is present |
| `[Claude#v] -> <latest> claude update` | `version` against `claudeCodeLatestVersion` in `<config>/.omc/update-check.json` | the cached latest is newer |
| `5h:` `wk:` | `rate_limits.five_hour` and `.seven_day`: `used_percentage`, and `resets_at` as a countdown | the window is in the payload |
| `thinking` | a thinking block in the transcript written within the last 30 s | yes |
| `session:` | `cost.total_duration_ms`, in whole minutes | always |
| `skill:` | the last `Skill` call in the transcript, with up to 15 columns of its arguments | a skill was called |
| `ctx:` | `context_window.used_percentage`, else tokens against the window size | always |
| `🔧` `🤖` `⚡` | transcript counts: tool calls, agent spawns (`Task`/`Agent`), skill calls | the count is not zero |
| `[!] ctx N% >= 80% threshold - run /compact` | context fill from 80 (`[!!]` from 90) | yes |
| `[!] claude v - paste: ! claude update`, `[!] omc v available` (OMC installed from the plugin marketplace) or `[!] omc v - paste: ! npm i -g oh-my-claude-sisyphus@latest` | the update cache | a newer version is cached |

Details:

- Percentages are rounded and clamped to 0-100. A bar is 8 columns for rate
  limits and 10 for context.
- Rate-limit color: green under 70, yellow 70-89, red from 90. Context: green,
  yellow from 70, yellow with ` COMPRESS?` from 80, red with ` CRITICAL` from 85.
  `session:` is yellow past 60 minutes or 70% context, red past 120 minutes or
  85%. The number always prints, so color is never the only signal.
- A countdown reads `0h18m`, `1h41m` or `6d5h`. It is omitted when `resets_at`
  is missing, not a number, not in the future, or more than 366 days ahead.
- Strings copied out of the input (model, effort, branch, profile, skill) lose
  control characters and zero-width or bidirectional marks, so a field cannot
  inject an escape sequence or reorder the line. They are cut at 64 characters.
- Widths count CJK as two columns and zero-width marks as none.
- The OMC segments need oh-my-claudecode. Without it there is no `[OMC...]`
  segment, and no update hints either, because the update cache is OMC's.

### Update hints

The newest versions are read from `<config>/.omc/update-check.json`, which
OMC's own update check keeps fresh. Fields used: `latestVersion` (OMC),
`source` (`marketplace` gives the plain `[!] omc v available` hint, because
updating a plugin install needs OMC's own steps, including re-running
`/omc-setup`; anything else gives `npm i -g oh-my-claude-sisyphus@latest`) and
`claudeCodeLatestVersion`. This binary only reads the file.

## Cost and behavior

Rules the code keeps, each with a test:

- **No network, ever.**
- **A warm render starts no process.** Branch, OMC version, update cache and
  transcript counts are file reads and `stat` calls.
- **`git` runs only when its cache is stale**: the 30 s TTL ran out, or the
  modification time of `HEAD`, the index, the branch ref, the upstream ref or
  `packed-refs` changed. A failed or timed-out `git` is cached for the TTL too,
  so a broken repository costs one attempt per 30 s, not one per render. If the
  cache directory cannot be found or written, the counts are left out rather
  than running `git` on every render.
- **One `git` per repository at a time.** A marker file stops concurrent
  renders (several sessions, or a render Claude Code cancelled) from starting a
  second one. The marker expires after 5 s.
- **A render never waits on `git` for more than a second.** After that `git` is
  killed and the render continues without the counts.
- **Transcripts are read once.** The first render reads the last 4 MiB; later
  renders read only what was appended, from an offset kept in the cache. A
  replaced or truncated transcript is read from its last 4 MiB again.

### Cache

`${XDG_CACHE_HOME:-~/.cache}/sharkline/`

| File | Holds |
|---|---|
| `git-<hash of repo root>.json` | status counts, the upstream ref name, when they were taken, and the file times they were taken against |
| `git-<hash of repo root>.run` | a marker that exists only while a `git status` runs (5 s at most) |
| `tr-<session id>.json` | per transcript: path, device and inode, byte offset, tool/agent/skill counts, the last skill, the time of the newest thinking block |

Files are written to a temporary name and renamed into place, so a reader sees
the old file or the new one. A missing, oversized, unparsable or wrong-shaped
cache file is a miss and is recomputed. Entries untouched for 7 days are
removed when a new one is written. The directory is created with mode 0700 and
its files 0600, because they hold paths and session data. Skill arguments are
cut to 64 characters before they are cached.

### What `git status` can run

The counts come from `git -c core.fsmonitor= --no-optional-locks status
--porcelain -b`. Because a repository's own config can influence git:

- `core.fsmonitor` is set empty on the command line, so a repository cannot
  make `status` run its monitor program.
- `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
  `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_NAMESPACE`
  and `GIT_PREFIX` are removed from git's environment, so a session started
  from inside a git hook does not report another repository's counts.
  `GIT_OPTIONAL_LOCKS=0` and `GIT_TERMINAL_PROMPT=0` are set.
- Hooks do not run.
- A clean or smudge filter still can. `status` has to read files whose stat data
  changed, and a `filter.<name>.clean` command in the repository's config with a
  matching `.gitattributes` line runs on them. This is the same exposure as any
  shell prompt that shows `git status`.
- `.git`, `HEAD` and the files read for the branch are opened only if they are
  regular files of a sane size, so a FIFO named `HEAD` cannot block a render.

## Never fails

A status line should not print an error, so every failure degrades:

- A missing or wrong-typed field drops only its own segment. Each outside
  lookup (git, OMC plugin, update cache, transcript) is isolated, and a panic in
  one drops its segments and nothing else.
- Invalid, truncated, empty or binary stdin renders as if the payload were
  `{}`: `session:0m | ctx:[----------]0%`, plus the git line when the process
  directory is in a repository.
- A lone surrogate escape (`"\ud800"`) or an overflowing number (`1e999`) in
  the payload is repaired instead of discarding the payload. Only the first JSON
  value on stdin is used, up to 1 MiB.
- A panic prints an empty line. stdout write errors are ignored. The exit status
  is always 0 and stderr is always empty.
- It does not block: a watchdog thread waits 1.5 s, and if nothing has been
  printed by then (a hung mount, or a stdin that never closes) it prints the
  line the payload alone gives and exits.

## Benchmark

Measured on a Linux desktop with [hyperfine](https://github.com/sharkdp/hyperfine)
1.20, release build (`cargo build --locked --release`), Rust 1.99. The machine
was under load from unrelated jobs during the runs, so these are not best-case numbers.

The input was a status payload like `tests/fixtures/stdin.json` pointing at a
git repository with 300 tracked files (2 commits ahead of its upstream, a few
modified, staged and untracked files), a synthetic 5 MB transcript (about
1,800 tool calls), a fake OMC plugin directory and an update cache. All data
was generated; the cache lived in a scratch `XDG_CACHE_HOME`.

```sh
# warm: 5 warm-up runs, then 1000 timed runs
hyperfine -N -w 5 -r 1000 --input payload.json sharkline
# cold cache: the cache directory is removed before every run (git runs,
# the transcript tail is read). reset.sh is: rm -rf "$XDG_CACHE_HOME/sharkline"
hyperfine -N -w 0 -r 1000 --prepare ./reset.sh --input payload.json sharkline
```

(`-N` makes hyperfine run `--prepare` without a shell too, so the reset has to
be a script file and not an inline `rm -rf $VAR`.)

| Render | Mean | Median (p50) | p99 | Runs |
|---|---|---|---|---|
| warm (git, transcript and plugin cached) | 1.35 ms | 1.26 ms | 2.79 ms | 1000 |
| cold cache (git status runs, transcript tail read) | 14.5 ms | 14.1 ms | 20.5 ms | 1000 |
| `/usr/bin/true`, for scale | 0.69 ms | 0.62 ms | 1.25 ms | 1000 |
| `node -e 0`, for scale | 29.9 ms | 27.6 ms | 69.3 ms | 200 |

The first render after installing, with nothing cached, is a cold-cache render
and took about 14 ms (one measurement, in a small Python timer that adds about
a millisecond of its own). A cold render is mostly `git status` plus reading
the transcript tail. It happens at most once per 30 s per repository, and only
when something the counts depend on changed or the TTL ran out. In a large
repository `git status` takes longer, and that cost lands on the cold render
only.

## Differences from the OMC HUD

Left out, because it is live state that this binary does not track:

- The running-agent segment and its detail lines, todos, background tasks,
  ralph/autopilot/PRD state, prompt time, the payload warning, the mission
  board and the session summary.
- The usage-API merge. The HUD asks Anthropic for usage; this binary shows
  `rate_limits` from the payload as is, so its percentages can differ by a few
  points, and it never prints the stale (`*`) and estimate (`~`) markers.

Different on purpose:

- The effort level follows the model name (`Model: Opus 5.5 xhigh`).
- A detached HEAD shows `branch:(abc1234)`.
- `thinking` means a thinking block in the transcript within the last 30 s, not
  the payload's `thinking.enabled`.
- `session:` is `cost.total_duration_ms`.
- Counts are incremental: they keep growing as the transcript does. They do not
  fall as old entries slide out of a re-read window.

## Build and test

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests are unit tests next to the code (JSON repair, cache, git, transcript,
layout, rendering) plus the fixture in `tests/fixtures/`. Dependencies are
`serde_json` and `memchr`.

## Platform

Unix only (Linux and macOS). The code uses Unix file metadata and permissions,
so it does not build on Windows. CI builds and tests on Ubuntu and macOS.

## License

[PolyForm Noncommercial License 1.0.0](LICENSE). You may use, change and share
this software for any noncommercial purpose: personal use, hobby projects, study,
research, and use by charities, schools and other noncommercial organizations.
Any commercial use needs a separate license from the copyright holder; ask
[spicysharkco](https://github.com/spicysharkco) on GitHub.

Contributions are welcome. Each one is assigned to spicysharkco under the
contributor agreement in [CONTRIBUTING.md](CONTRIBUTING.md), so spicysharkco owns
the whole project.
