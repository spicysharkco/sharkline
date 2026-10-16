# Changelog

All notable changes to sharkline are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-13

First public release. sharkline is a Claude Code status line written in Rust: Claude
Code pipes its status JSON to the command on stdin, and sharkline prints a compact HUD.

### Added

- The "focused" layout of the oh-my-claudecode HUD: a git line (branch, linked
  worktree mark, staged, modified, untracked, ahead and behind counts), then model and
  effort, 5-hour and weekly rate-limit bars with reset countdowns, a thinking mark,
  session time, the last skill, context usage with `/compact` warnings, and tool, agent
  and skill counts. Update hints for Claude Code and oh-my-claudecode are read from the
  cache file OMC already keeps.
- Colored output, with `NO_COLOR` to turn it off, and wrapping at `COLUMNS` to at most
  four lines.
- A render that never fails: bad or missing fields drop only their own segment, bad
  stdin renders as an empty payload, a panic prints an empty line, and the exit status is
  always 0 with nothing on stderr. A watchdog prints a fallback line after 1.5 s.
- A warm render of about 1.3 ms that starts no other process and makes no network
  request. The git branch is read from `.git/HEAD`; `git status` runs only when its
  cached answer is stale, once per repository at a time, and never for more than a
  second. The transcript is read once and then only the bytes added since.
- Prebuilt binaries on the releases page for macOS (Apple silicon and Intel) and Linux
  (x86-64 and ARM64, statically linked), with a `SHA256SUMS` file and build provenance
  attestations. `cargo binstall --git` can use the archives too. `cargo install
  --locked --git` still builds from source.

[0.1.0]: https://github.com/spicysharkco/sharkline/releases/tag/v0.1.0
