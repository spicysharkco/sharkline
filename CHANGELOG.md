# Changelog

All notable changes to sharkline are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-10-20

### Added

- A `cache:` segment after `ctx:`, from the payload's `prompt_cache`: the time left before a
  warm prompt cache expires (`cache:4m`, yellow seconds in the last minute), or `cache:cold`
  with the tokens the next request writes to the cache again (`cache:cold(45k)`); then
  `miss:N` with the causes Claude Code gave for the last miss, yellow for five minutes and
  dim after. It is left out until a response reports cache tokens, and reads `cold` once
  `expires_at` has passed even if the payload still says warm.
- A `spend:` segment after the `5h:` and `wk:` quotas, from `rate_limits.spend_limit` behind a
  Claude apps gateway: bar, percentage (past 100 once the limit is exceeded), reset countdown,
  and `$used/$limit` once the gateway reports both amounts. A narrow terminal wraps before
  it rather than cutting the amounts short.
- The README explains `refreshInterval`: without it an idle session's countdowns stand still.

### Changed

- When the git and main lines wrap to fill all four lines, the context notice and update
  hints are left out. Before, the `... (+N lines)` note took the fourth line and hid the
  end of the main line, `ctx:` included.
- The sample fixture carries a `prompt_cache` object and a current `version`.
- Documentation links point to `code.claude.com`.

## [0.2.1] - 2026-10-19

### Changed

- `sharkline --help` and the README's environment table list `HOME`: it sets what `~` means
  in `CLAUDE_CONFIG_DIR` and the base of the `~/.claude` and `~/.cache` defaults.
- The README and `sharkline --help` name `~/.cargo/bin/sharkline` for `cargo binstall` as
  well as `cargo install`: both install into cargo's bin directory.

### Fixed

- Unit tests that run a stub `git` allow it 10 s instead of 1 s, so a loaded machine no
  longer fails them. The cache-hit test checks that the stub did not run again instead of
  timing the call.

## [0.2.0] - 2026-10-17

### Added

- `sharkline --version` (or `-V`) prints the version, and `sharkline --help` (or `-h`)
  prints the usage and the environment variables it reads. Both exit 0 before stdin is
  read, so they no longer wait for input in a terminal. Any other argument is still
  ignored.
- `SECURITY.md`: report a vulnerability through GitHub private vulnerability reporting.

### Changed

- The release workflow's smoke test also checks that `--version` prints the version
  being released.
- The README's `settings.json` example points at `~/.local/bin/sharkline`, where the
  release archive and the source build install, and names `~/.cargo/bin/sharkline` for
  `cargo install`.

### Fixed

- The README's source build creates `~/.local/bin` before installing into it.

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

[Unreleased]: https://github.com/spicysharkco/sharkline/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/spicysharkco/sharkline/releases/tag/v0.3.0
[0.2.1]: https://github.com/spicysharkco/sharkline/releases/tag/v0.2.1
[0.2.0]: https://github.com/spicysharkco/sharkline/releases/tag/v0.2.0
[0.1.0]: https://github.com/spicysharkco/sharkline/releases/tag/v0.1.0
