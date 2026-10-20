//! The status line: the OMC HUD's "focused" layout, rebuilt from the JSON Claude
//! Code writes on stdin plus a few facts gathered outside it (git, the
//! transcript, OMC's files).
//!
//! ```text
//! branch:main (wt:name) | !30 ?103 ⇡5 ⇣9              <- git line (only in a repository)
//! [OMC#5.6.1L] | Model: Opus 5.5 xhigh | ... | ctx:[##--------]24% | 🔧251 🤖4
//! [!] claude 2.1.291 - paste: ! claude update          <- detail lines
//! ```
//!
//! Every lookup is optional: a missing or wrong-typed field drops its own
//! segment and never fails the line. The escape sequences, the order and the
//! thresholds follow the HUD's; see the README for the few places this
//! deliberately differs.

use crate::git::Counts;
use crate::layout;
use crate::omc::{self, Plugin, Updates};
use crate::transcript::Calls;
use serde_json::Value;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

const QUOTA_BAR_WIDTH: usize = 8;
const CTX_BAR_WIDTH: usize = 10;
/// The HUD's context thresholds (percent): colored, then " COMPRESS?", then red " CRITICAL".
const CTX_WARNING: u8 = 70;
const CTX_COMPACT: u8 = 80;
const CTX_CRITICAL: u8 = 85;
/// `[!] ctx N% >= 80% threshold` line starts here; two bangs from 90.
const CTX_NOTICE: u8 = 80;
const CTX_NOTICE_CRITICAL: u8 = 90;
/// A thinking block counts as live for this long after it was written.
const THINKING_WINDOW_SECS: f64 = 30.0;
/// More lines than this are cut and replaced by a count.
const MAX_LINES: usize = 4;
/// Cap on any string copied out of the input, so one bad field cannot flood the line.
const TEXT_MAX_CHARS: usize = 64;
/// A reset further away than this is a bad value, not a countdown.
const MAX_RESET_AHEAD_SECS: f64 = 366.0 * 86_400.0;
/// The longest prompt cache lifetime is an hour; an expiry further out than
/// that, plus a minute of slack, is not a real one and gets no countdown.
const CACHE_MAX_AHEAD_SECS: f64 = 3600.0 + 60.0;
/// A cache miss longer ago than this is shown dim instead of yellow.
const MISS_FRESH_SECS: f64 = 300.0;
/// A miss count past this is shown as `999+`, to bound the width.
const MISSES_MAX: u64 = 999;
/// Cause names shown for the last cache miss; the rest are counted (`+2`).
const MISS_CAUSES_SHOWN: usize = 2;
/// A token count or dollar amount from here up is a bad value and is left out.
const AMOUNT_MAX: f64 = 1e9;
/// A spend percentage past this is shown as this, to bound the width.
const SPEND_MAX_PCT: f64 = 999.0;
const SKILL_ARGS_WIDTH: usize = 15;
const MODEL_FALLBACK_WIDTH: usize = 20;

/// Rendering switches, resolved once by the caller from the environment.
pub struct Opts {
    pub color: bool,
    /// Unix seconds; the reset countdowns count down to `resets_at` from here.
    pub now: f64,
    /// Terminal width from `COLUMNS`; 0 means unknown, so lines are not fitted.
    pub columns: usize,
}

/// What the status line shows that is not in the payload.
#[derive(Default)]
pub struct Facts {
    /// Text after `branch:`.
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub status: Option<Counts>,
    /// Config-dir name when `CLAUDE_CONFIG_DIR` is set.
    pub profile: Option<String>,
    pub plugin: Option<Plugin>,
    pub updates: Option<Updates>,
    pub calls: Option<Calls>,
}

/// Working directory named by the input, for the repository lookup.
pub fn work_dir(input: &Value) -> Option<&str> {
    ["/workspace/current_dir", "/cwd"]
        .iter()
        .find_map(|p| input.pointer(p)?.as_str().filter(|s| !s.is_empty()))
}

/// The whole status line: the git line, the main line, then detail lines,
/// fitted to the terminal width and cut to `MAX_LINES`. Never empty: the
/// session and context segments print whatever the payload holds.
pub fn render(input: &Value, f: &Facts, o: &Opts) -> String {
    let sep = paint(o, DIM, " | ");
    let ctx = context_percent(input);

    let git_line = [
        branch_segment(f, o),
        status_segment(f, o),
        profile_segment(f, o),
    ];
    let main_line = [
        omc_label(f, o),
        model(input, o),
        claude_label(input, f, o),
        quotas(input, o),
        spend(input, o),
        thinking(f, o),
        Some(session(input, ctx, o)),
        last_skill(f, o),
        Some(context_segment(ctx, o)),
        prompt_cache(input, o),
        call_counts(f),
    ];
    let detail = [context_notice(ctx, o)]
        .into_iter()
        .chain(update_hints(input, f, o));

    let join = |segments: Vec<String>| (!segments.is_empty()).then(|| segments.join(&sep));
    let separators = [sep.as_str(), " | "];
    let fit = |l: String| {
        if o.columns > 0 {
            layout::wrap_line(&l, o.columns, separators)
        } else {
            vec![l]
        }
    };
    let head: Vec<String> = [
        join(git_line.into_iter().flatten().collect()),
        join(main_line.into_iter().flatten().collect()),
    ]
    .into_iter()
    .flatten()
    .flat_map(fit)
    .collect();
    let tail: Vec<String> = detail.flatten().flat_map(fit).collect();
    let lines = layout::limit_lines(head, tail, MAX_LINES);
    lines
        .iter()
        .map(|l| {
            let l = if o.columns > 0 {
                layout::truncate_line(l, o.columns)
            } else {
                l.clone()
            };
            l.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn paint(o: &Opts, code: &str, text: &str) -> String {
    if o.color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Characters that never belong in a one-line label. Control characters (an
/// escape sequence in a field must not reach the terminal), plus the invisible
/// format characters that can reorder or hide text: zero-width and
/// directional marks (including the Arabic letter mark), line and paragraph
/// separators, the word joiner and the invisible math operators, directional
/// isolates, the byte order mark, and the tag characters.
fn is_unwanted(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// Drop unwanted characters, cap the length, trim.
fn clean(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| !is_unwanted(*c))
        .take(TEXT_MAX_CHARS)
        .collect();
    kept.trim().to_string()
}

fn text(v: Option<&Value>) -> Option<String> {
    let s = clean(v?.as_str()?);
    (!s.is_empty()).then_some(s)
}

/// Any JSON number as a percentage: rounded, clamped to 0..=100.
fn pct(v: Option<&Value>) -> Option<u8> {
    Some(v?.as_f64()?.round().clamp(0.0, 100.0) as u8)
}

// ---- git line ----

fn branch_segment(f: &Facts, o: &Opts) -> Option<String> {
    let name = clean(f.branch.as_deref()?);
    if name.is_empty() {
        return None;
    }
    let mut out = format!("{}{}", paint(o, DIM, "branch:"), paint(o, CYAN, &name));
    if let Some(wt) = f.worktree.as_deref().map(clean).filter(|w| !w.is_empty()) {
        out.push_str(&format!(
            " {}{}{}",
            paint(o, DIM, "(wt:"),
            paint(o, CYAN, &wt),
            paint(o, DIM, ")")
        ));
    }
    Some(out)
}

/// `!30 ?103 ⇡5 ⇣9`: staged `+`, modified `!`, untracked `?`, ahead, behind;
/// zero counts are left out and a clean tree shows nothing.
fn status_segment(f: &Facts, o: &Opts) -> Option<String> {
    let c = f.status.filter(|c| !c.is_clean())?;
    let part =
        |n: u32, code: &str, mark: &str| (n > 0).then(|| format!("{}{n}", paint(o, code, mark)));
    let parts: Vec<String> = [
        part(c.staged, GREEN, "+"),
        part(c.modified, RED, "!"),
        part(c.untracked, CYAN, "?"),
        part(c.ahead, GREEN, "\u{21e1}"),
        part(c.behind, RED, "\u{21e3}"),
    ]
    .into_iter()
    .flatten()
    .collect();
    Some(parts.join(" "))
}

fn profile_segment(f: &Facts, o: &Opts) -> Option<String> {
    let name = clean(f.profile.as_deref()?);
    (!name.is_empty()).then(|| paint(o, BOLD, &format!("profile:{name}")))
}

// ---- main line ----

fn omc_label(f: &Facts, o: &Opts) -> Option<String> {
    let plugin = f.plugin.as_ref()?;
    let version = plugin
        .version
        .as_deref()
        .map(clean)
        .filter(|v| !v.is_empty());
    let tag = match (&version, plugin.local) {
        (Some(v), true) => format!("#{v}L"),
        (Some(v), false) => format!("#{v}"),
        (None, true) => "#L".to_string(),
        (None, false) => String::new(),
    };
    Some(match newer_omc(f) {
        Some(latest) => paint(o, BOLD, &format!("[OMC{tag}] -> {latest} omc update")),
        None => paint(o, BOLD, &format!("[OMC{tag}]")),
    })
}

/// A newer OMC than the installed one, if the update check saw one.
fn newer_omc(f: &Facts) -> Option<String> {
    let installed = f.plugin.as_ref()?.version.as_deref()?;
    let latest = clean(f.updates.as_ref()?.omc.as_deref()?);
    (!latest.is_empty() && omc::is_older(installed, &latest)).then_some(latest)
}

/// A newer Claude Code than the one that sent this payload, if the update check saw one.
fn newer_claude(input: &Value, f: &Facts) -> Option<(String, String)> {
    let running = text(input.get("version"))?;
    let latest = clean(f.updates.as_ref()?.claude.as_deref()?);
    (!latest.is_empty() && omc::is_older(&running, &latest)).then_some((running, latest))
}

fn claude_label(input: &Value, f: &Facts, o: &Opts) -> Option<String> {
    let (running, latest) = newer_claude(input, f)?;
    Some(paint(
        o,
        BOLD,
        &format!("[Claude#{running}] -> {latest} claude update"),
    ))
}

/// `Model: Opus 5.5 xhigh`: the HUD's versioned short name from the display
/// name (the model id when that is blank), then the effort level.
fn model(input: &Value, o: &Opts) -> Option<String> {
    let source =
        text(input.pointer("/model/display_name")).or_else(|| text(input.pointer("/model/id")));
    let name = source.map(|s| format!("Model: {}", model_name(&s)));
    let effort = text(input.pointer("/effort/level"));
    let parts: Vec<String> = [name, effort].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| paint(o, CYAN, &parts.join(" ")))
}

fn tier_len(s: &str) -> Option<usize> {
    ["opus", "sonnet", "haiku"]
        .into_iter()
        .find(|t| s.starts_with(t))
        .map(str::len)
}

fn digits(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

/// The version inside a model id or display name, tried in the HUD's order:
/// `opus-5-5` / `opus-4-20250514`, `claude-opus-5`, `claude-3-5-sonnet`,
/// `Opus 5.5`.
fn model_version(id: &str) -> Option<String> {
    let at = |i: usize| &id[i..];
    // opus-<major>-<minor>; a minor of eight digits is a date, so it is dropped.
    for (i, _) in id.char_indices() {
        let Some(n) = tier_len(at(i)) else { continue };
        let Some(rest) = at(i + n).strip_prefix('-') else {
            continue;
        };
        let major = digits(rest);
        let Some(after) = rest[major..].strip_prefix('-').filter(|_| major > 0) else {
            continue;
        };
        let minor = digits(after);
        if minor > 0 {
            return Some(if minor == 8 {
                rest[..major].to_string()
            } else {
                format!("{}.{}", &rest[..major], &after[..minor])
            });
        }
    }
    // claude-opus-<major> at the very end, after the start or a `.` or `-`.
    for (i, _) in id.match_indices("claude-") {
        let boundary = id[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c == '.' || c == '-');
        let rest = at(i + "claude-".len());
        let Some(n) = tier_len(rest) else { continue };
        let Some(num) = rest[n..].strip_prefix('-') else {
            continue;
        };
        if boundary && !num.is_empty() && digits(num) == num.len() {
            return Some(num.to_string());
        }
    }
    // claude-<major>[-<minor>]-opus
    for (i, _) in id.match_indices("claude-") {
        let rest = at(i + "claude-".len());
        let major = digits(rest);
        if major == 0 {
            continue;
        }
        let tail = &rest[major..];
        if let Some(r) = tail.strip_prefix('-') {
            let minor = digits(r);
            if minor > 0
                && r[minor..]
                    .strip_prefix('-')
                    .is_some_and(|t| tier_len(t).is_some())
            {
                return Some(format!("{}.{}", &rest[..major], &r[..minor]));
            }
        }
        if tail
            .strip_prefix('-')
            .is_some_and(|t| tier_len(t).is_some())
        {
            return Some(rest[..major].to_string());
        }
    }
    // opus 5.5
    for (i, _) in id.char_indices() {
        let Some(n) = tier_len(at(i)) else { continue };
        let rest = at(i + n);
        let trimmed = rest.trim_start();
        let major = digits(trimmed);
        if trimmed.len() == rest.len() || major == 0 {
            continue;
        }
        let fraction = trimmed[major..].strip_prefix('.').map_or(0, digits);
        let end = if fraction > 0 {
            major + 1 + fraction
        } else {
            major
        };
        return Some(trimmed[..end].to_string());
    }
    None
}

/// `Opus 5.5`, `Sonnet 4.5`, `Haiku`; a name with no known tier is cut to 20 columns.
fn model_name(source: &str) -> String {
    let id = source.to_lowercase();
    let tier = if id.contains("opus") {
        "Opus"
    } else if id.contains("sonnet") {
        "Sonnet"
    } else if id.contains("haiku") {
        "Haiku"
    } else {
        return layout::truncate_to_width(source, MODEL_FALLBACK_WIDTH);
    };
    match model_version(&id) {
        Some(v) => format!("{tier} {v}"),
        None => tier.to_string(),
    }
}

/// Green under 70, yellow 70-89, red 90 and up (the quota windows).
fn quota_color(p: u8) -> &'static str {
    if p >= 90 {
        RED
    } else if p >= 70 {
        YELLOW
    } else {
        GREEN
    }
}

/// `[####----]`: filled part in the level color, the rest dim, one reset.
fn bar(p: u8, width: usize, color: &str, o: &Opts) -> String {
    let filled = (usize::from(p) * width + 50) / 100;
    let (full, empty) = ("#".repeat(filled), "-".repeat(width - filled));
    if o.color {
        format!("[{color}{full}{DIM}{empty}{RESET}]")
    } else {
        format!("[{full}{empty}]")
    }
}

/// A reset time as seconds since the epoch: a number (seconds, or milliseconds
/// when it is too big for seconds) or a string holding one.
fn reset_secs(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|_| !s.trim().is_empty())?,
        _ => return None,
    };
    n.is_finite()
        .then(|| if n.abs() < 1e12 { n } else { n / 1000.0 })
}

/// Time left until `resets_at`: `0h18m`, `1h41m`, `6d5h`. `None` when missing,
/// wrong-typed, not in the future, or more than a year away.
fn countdown(v: Option<&Value>, now: f64) -> Option<String> {
    let ahead = reset_secs(v?)? - now;
    if ahead.is_nan() || ahead <= 0.0 || ahead > MAX_RESET_AHEAD_SECS {
        return None;
    }
    let mins = (ahead / 60.0).floor() as u64;
    let (hours, days) = (mins / 60, mins / 1440);
    Some(if days > 0 {
        format!("{days}d{}h", hours % 24)
    } else {
        format!("{hours}h{}m", mins % 60)
    })
}

/// `5h:[####----]54%(3h42m)`; the HUD dimmed the `wk:` label but not `5h:`.
fn quota(input: &Value, key: &str, label: &str, dim_label: bool, o: &Opts) -> Option<String> {
    let window = input.pointer(&format!("/rate_limits/{key}"))?;
    let p = pct(window.get("used_percentage"))?;
    let color = quota_color(p);
    let reset = countdown(window.get("resets_at"), o.now)
        .map(|r| paint(o, DIM, &format!("({r})")))
        .unwrap_or_default();
    let label = if dim_label {
        paint(o, DIM, &format!("{label}:"))
    } else {
        format!("{label}:")
    };
    Some(format!(
        "{label}{}{}{reset}",
        bar(p, QUOTA_BAR_WIDTH, color, o),
        paint(o, color, &format!("{p}%")),
    ))
}

/// `spend:[#####---]63%(5d2h) $314.12/$500`: the spend limit a Claude apps
/// gateway sets. The percentage passes 100 once the limit is exceeded, while
/// the bar stops full. The dollars arrive later than the percentage and only
/// show when both amounts are present. A segment of its own, not part of
/// `quotas`, so a narrow terminal wraps before it instead of cutting the
/// dollars short.
fn spend(input: &Value, o: &Opts) -> Option<String> {
    let limit = input.pointer("/rate_limits/spend_limit")?;
    let used = limit.get("used_percentage")?.as_f64()?;
    let shown = used.round().clamp(0.0, SPEND_MAX_PCT) as u16;
    let p = shown.min(100) as u8;
    let color = quota_color(p);
    let reset = countdown(limit.get("resets_at"), o.now)
        .map(|r| paint(o, DIM, &format!("({r})")))
        .unwrap_or_default();
    let dollars = match (usd(limit.get("used_usd")), usd(limit.get("limit_usd"))) {
        (Some(used), Some(limit)) if limit > 0.0 => {
            paint(o, DIM, &format!(" ${}/${}", cents(used), cents(limit)))
        }
        _ => String::new(),
    };
    Some(format!(
        "{}{}{}{reset}{dollars}",
        paint(o, DIM, "spend:"),
        bar(p, QUOTA_BAR_WIDTH, color, o),
        paint(o, color, &format!("{shown}%")),
    ))
}

/// A dollar amount rounded to the cent. `None` when not a number, or when
/// it rounds to below zero or to a billion or more.
fn usd(v: Option<&Value>) -> Option<f64> {
    let n = (v?.as_f64()? * 100.0).round() / 100.0;
    // abs() turns -0 into 0, which would otherwise print as `-0`.
    (0.0..AMOUNT_MAX).contains(&n).then_some(n.abs())
}

/// `500` when whole, else two decimals (`314.12`).
fn cents(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n:.2}")
    }
}

/// 5-hour and weekly quotas, space-joined; either may be absent.
fn quotas(input: &Value, o: &Opts) -> Option<String> {
    let parts: Vec<String> = [
        quota(input, "five_hour", "5h", false, o),
        quota(input, "seven_day", "wk", true, o),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// True while the newest thinking block in the transcript is recent.
fn thinking(f: &Facts, o: &Opts) -> Option<String> {
    let at = f.calls.as_ref()?.thinking_at?;
    (o.now - at <= THINKING_WINDOW_SECS).then(|| paint(o, CYAN, "thinking"))
}

/// The context fill, from the first source that has a positive figure: the
/// percentage Claude Code reports, then the tokens in the current request
/// against the window, then the session's input tokens against the window,
/// else 0.
fn context_percent(input: &Value) -> u8 {
    let window = input.get("context_window");
    let num = |p: &str| window.and_then(|w| w.pointer(p)).and_then(Value::as_f64);
    let clamp = |p: f64| p.round().clamp(0.0, 100.0) as u8;
    let size = num("/context_window_size").filter(|s| *s > 0.0);
    let of_window = |tokens: f64| size.map(|s| clamp(tokens / s * 100.0));
    let native = num("/used_percentage").filter(|p| *p > 0.0).map(clamp);
    let current = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .map(|k| num(&format!("/current_usage/{k}")).unwrap_or(0.0))
    .sum::<f64>();
    native
        .or_else(|| of_window(current).filter(|p| *p > 0))
        .or_else(|| {
            num("/total_input_tokens")
                .filter(|t| *t > 0.0)
                .and_then(of_window)
        })
        .unwrap_or(0)
}

/// `ctx:[###-------]28%`, with a suffix once it is high.
fn context_segment(p: u8, o: &Opts) -> String {
    let (color, suffix) = if p >= CTX_CRITICAL {
        (RED, " CRITICAL")
    } else if p >= CTX_COMPACT {
        (YELLOW, " COMPRESS?")
    } else if p >= CTX_WARNING {
        (YELLOW, "")
    } else {
        (GREEN, "")
    };
    format!(
        "ctx:{}{}",
        bar(p, CTX_BAR_WIDTH, color, o),
        paint(o, color, &format!("{p}%{suffix}"))
    )
}

/// `cache:4m` while the prompt cache is warm, with the time left before it goes
/// cold (yellow seconds in the last minute, `warm` when no expiry is given);
/// `cache:cold(45k)` once it is cold, with the tokens the next request writes
/// to the cache again; then `miss:2(tools_changed)` when requests re-processed
/// content the cache held, with the cause Claude Code gave for the last one.
/// Left out until a response this session reported cache tokens.
fn prompt_cache(input: &Value, o: &Opts) -> Option<String> {
    let c = input.get("prompt_cache")?;
    if c.get("caching_observed") != Some(&Value::Bool(true)) {
        return None;
    }
    let ahead = c
        .get("expires_at")
        .and_then(reset_secs)
        .map(|at| at - o.now);
    let warm = c.get("warm") == Some(&Value::Bool(true)) && ahead.is_none_or(|a| a > 0.0);
    let state = if warm {
        match ahead.filter(|a| *a <= CACHE_MAX_AHEAD_SECS) {
            Some(a) if a < 60.0 => paint(o, YELLOW, &format!("{}s", (a as u64).max(1))),
            Some(a) => paint(o, GREEN, &format!("{}m", (a / 60.0) as u64)),
            None => paint(o, GREEN, "warm"),
        }
    } else {
        let recache = c
            .get("recache_tokens_if_cold")
            .and_then(Value::as_f64)
            .filter(|n| (1.0..AMOUNT_MAX).contains(n))
            .map(|n| paint(o, DIM, &format!("({})", tokens(n))))
            .unwrap_or_default();
        format!("{}{recache}", paint(o, YELLOW, "cold"))
    };
    let misses = c
        .get("misses")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| format!(" {}", cache_misses(c, n, o)))
        .unwrap_or_default();
    Some(format!("cache:{state}{misses}"))
}

/// `miss:2(tools_changed)`: the session's miss count, then the causes of the
/// last miss, whole names only (`a,b+1` past two). Yellow while the last miss
/// is under five minutes old or its time is unknown, dim after that.
fn cache_misses(c: &Value, n: u64, o: &Opts) -> String {
    let count = if n > MISSES_MAX {
        format!("{MISSES_MAX}+")
    } else {
        n.to_string()
    };
    let fresh = c
        .get("last_miss_at")
        .and_then(reset_secs)
        .is_none_or(|at| o.now - at <= MISS_FRESH_SECS);
    let causes: Vec<String> = c
        .pointer("/last_miss_cause/causes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| text(Some(v))).collect())
        .unwrap_or_default();
    let cause = match causes.len() {
        0 => String::new(),
        len => {
            let shown = len.min(MISS_CAUSES_SHOWN);
            let more = if len > shown {
                format!("+{}", len - shown)
            } else {
                String::new()
            };
            paint(o, DIM, &format!("({}{more})", causes[..shown].join(",")))
        }
    };
    let color = if fresh { YELLOW } else { DIM };
    format!("{}{cause}", paint(o, color, &format!("miss:{count}")))
}

/// A token count in a few columns: `950`, `45k`, `1.2M`.
fn tokens(n: f64) -> String {
    if n < 1000.0 {
        format!("{}", n as u64)
    } else if n < 999_500.0 {
        format!("{}k", (n / 1000.0).round() as u64)
    } else {
        format!("{}M", (n / 100_000.0).round() / 10.0)
    }
}

/// `session:1283m`, colored by health: yellow past 60 min or 70% context, red
/// past 120 min or 85%. Zero minutes when the payload has no duration.
fn session(input: &Value, ctx: u8, o: &Opts) -> String {
    let ms = input
        .pointer("/cost/total_duration_ms")
        .and_then(Value::as_f64)
        .filter(|ms| *ms > 0.0)
        .unwrap_or(0.0);
    let mins = (ms / 60_000.0) as u64;
    let color = if mins > 120 || ctx > CTX_CRITICAL {
        RED
    } else if mins > 60 || ctx > CTX_WARNING {
        YELLOW
    } else {
        GREEN
    };
    format!("session:{}", paint(o, color, &format!("{mins}m")))
}

/// `skill:session-handoff(Detailed han...)`: the last skill the session ran
/// without its plugin prefix, with up to 15 columns of its arguments.
fn last_skill(f: &Facts, o: &Opts) -> Option<String> {
    let skill = f.calls.as_ref()?.last_skill.as_ref()?;
    let name = clean(
        skill
            .name
            .rsplit(':')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(&skill.name),
    );
    if name.is_empty() {
        return None;
    }
    let args = skill
        .args
        .as_deref()
        .map(clean)
        .filter(|a| !a.is_empty())
        .map(|a| format!("({})", layout::truncate_to_width(&a, SKILL_ARGS_WIDTH)))
        .unwrap_or_default();
    Some(paint(o, CYAN, &format!("skill:{name}{args}")))
}

/// `🔧251 🤖4 ⚡2`: tool calls, agent spawns, skill uses; zeros are left out.
fn call_counts(f: &Facts) -> Option<String> {
    let c = f.calls.as_ref()?;
    let parts: Vec<String> = [
        (c.tools, "\u{1f527}"),
        (c.agents, "\u{1f916}"),
        (c.skills, "\u{26a1}"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, icon)| format!("{icon}{n}"))
    .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

// ---- detail lines ----

/// `[!] ctx 89% >= 80% threshold - run /compact`.
fn context_notice(p: u8, o: &Opts) -> Option<String> {
    if p < CTX_NOTICE {
        return None;
    }
    let (color, icon) = if p >= CTX_NOTICE_CRITICAL {
        (RED, "!!")
    } else {
        (YELLOW, "!")
    };
    Some(notice(
        o,
        color,
        &format!("[{icon}] ctx {p}% >= {CTX_NOTICE}% threshold - run /compact"),
    ))
}

/// A bold colored line with no dim parts, as the HUD's hints were.
fn notice(o: &Opts, color: &str, text: &str) -> String {
    if o.color {
        format!("{color}{BOLD}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// `[!] omc 5.7.0 available` for a marketplace install (updating it takes more
/// than a command, so none is offered), `[!] omc 5.7.0 - paste: ! npm i -g ...`
/// otherwise, and `[!] claude 2.1.291 - paste: ! claude update`.
fn update_hints(input: &Value, f: &Facts, o: &Opts) -> Vec<Option<String>> {
    let omc = newer_omc(f).map(|latest| {
        let marketplace = f.updates.as_ref().is_some_and(|u| u.marketplace);
        let text = if marketplace {
            format!("[!] omc {latest} available")
        } else {
            format!("[!] omc {latest} - paste: ! npm i -g oh-my-claude-sisyphus@latest")
        };
        notice(o, YELLOW, &text)
    });
    let claude = newer_claude(input, f).map(|(_, latest)| {
        notice(
            o,
            YELLOW,
            &format!("[!] claude {latest} - paste: ! claude update"),
        )
    });
    vec![omc, claude]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Skill;
    use serde_json::json;

    const NOW: f64 = 1_000_000.0;

    fn color() -> Opts {
        Opts {
            color: true,
            now: NOW,
            columns: 0,
        }
    }

    fn plain() -> Opts {
        Opts {
            color: false,
            now: NOW,
            columns: 0,
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn facts() -> Facts {
        Facts::default()
    }

    // ---- bars and colors ----

    #[test]
    fn bar_fill_rounds_like_the_hud() {
        let o = plain();
        assert_eq!(bar(0, 8, GREEN, &o), "[--------]");
        assert_eq!(bar(54, 8, GREEN, &o), "[####----]");
        assert_eq!(bar(55, 8, GREEN, &o), "[####----]");
        assert_eq!(bar(78, 8, GREEN, &o), "[######--]");
        assert_eq!(bar(88, 8, GREEN, &o), "[#######-]");
        assert_eq!(bar(100, 8, GREEN, &o), "[########]");
        assert_eq!(bar(28, 10, GREEN, &o), "[###-------]");
        assert_eq!(bar(25, 10, GREEN, &o), "[###-------]", "half rounds up");
    }

    #[test]
    fn bar_escapes_are_the_huds_even_when_a_half_is_empty() {
        // The HUD printed the color code before an empty fill, and one reset at the end.
        assert_eq!(
            bar(54, 8, GREEN, &color()),
            "[\x1b[32m####\x1b[2m----\x1b[0m]"
        );
        assert_eq!(
            bar(100, 8, RED, &color()),
            "[\x1b[31m########\x1b[2m\x1b[0m]"
        );
        assert_eq!(
            bar(0, 8, GREEN, &color()),
            "[\x1b[32m\x1b[2m--------\x1b[0m]"
        );
    }

    #[test]
    fn quota_levels() {
        assert_eq!(quota_color(0), GREEN);
        assert_eq!(quota_color(69), GREEN);
        assert_eq!(quota_color(70), YELLOW);
        assert_eq!(quota_color(89), YELLOW);
        assert_eq!(quota_color(90), RED);
        assert_eq!(quota_color(100), RED);
    }

    #[test]
    fn pct_rounds_clamps_and_rejects_non_numbers() {
        assert_eq!(pct(Some(&json!(28))), Some(28));
        assert_eq!(pct(Some(&json!(28.4))), Some(28));
        assert_eq!(pct(Some(&json!(28.5))), Some(29));
        assert_eq!(pct(Some(&json!(-5))), Some(0));
        assert_eq!(pct(Some(&json!(250))), Some(100));
        assert_eq!(pct(Some(&json!(1e30))), Some(100));
        assert_eq!(pct(Some(&json!("28"))), None);
        assert_eq!(pct(Some(&json!(null))), None);
        assert_eq!(pct(Some(&json!(true))), None);
        assert_eq!(pct(Some(&json!([28]))), None);
        assert_eq!(pct(None), None);
    }

    // ---- countdown ----

    #[test]
    fn countdown_formats_like_the_hud() {
        let at = |secs: f64| countdown(Some(&json!(NOW + secs)), NOW);
        assert_eq!(at(1.0).as_deref(), Some("0h0m"));
        assert_eq!(at(18.0 * 60.0).as_deref(), Some("0h18m"));
        assert_eq!(at(41.0 * 60.0 + 59.0).as_deref(), Some("0h41m"), "floors");
        assert_eq!(at(3600.0).as_deref(), Some("1h0m"));
        assert_eq!(at(3600.0 + 41.0 * 60.0).as_deref(), Some("1h41m"));
        assert_eq!(at(23.0 * 3600.0 + 59.0 * 60.0).as_deref(), Some("23h59m"));
        assert_eq!(at(86_400.0).as_deref(), Some("1d0h"));
        assert_eq!(
            at(6.0 * 86_400.0 + 5.0 * 3600.0 + 1799.0).as_deref(),
            Some("6d5h")
        );
    }

    #[test]
    fn countdown_takes_milliseconds_and_numeric_strings() {
        // A real epoch: a value this big is milliseconds, one below 1e12 is seconds.
        let now = 1_750_000_000.0;
        let secs = now + 6060.0;
        assert_eq!(countdown(Some(&json!(secs)), now).as_deref(), Some("1h41m"));
        assert_eq!(
            countdown(Some(&json!(secs * 1000.0)), now).as_deref(),
            Some("1h41m")
        );
        assert_eq!(
            countdown(Some(&json!(secs.to_string())), now).as_deref(),
            Some("1h41m")
        );
        assert_eq!(
            countdown(Some(&json!(format!(" {secs} "))), now).as_deref(),
            Some("1h41m")
        );
    }

    #[test]
    fn countdown_omitted_when_past_missing_or_wrong() {
        assert_eq!(countdown(Some(&json!(NOW)), NOW), None);
        assert_eq!(countdown(Some(&json!(NOW - 1.0)), NOW), None);
        for bad in [
            json!("soon"),
            json!(""),
            json!("  "),
            json!(null),
            json!(true),
            json!([1]),
            json!({}),
        ] {
            assert_eq!(countdown(Some(&bad), NOW), None, "{bad}");
        }
        assert_eq!(countdown(None, NOW), None);
    }

    #[test]
    fn countdown_omitted_when_more_than_a_year_away() {
        let year = 366.0 * 86_400.0;
        assert!(
            countdown(Some(&json!(NOW + year)), NOW).is_some(),
            "366 days is still a countdown"
        );
        assert_eq!(countdown(Some(&json!(NOW + year + 1.0)), NOW), None);
        assert_eq!(countdown(Some(&json!(NOW + 400.0 * 86_400.0)), NOW), None);
        assert_eq!(countdown(Some(&json!(1e300)), NOW), None);
        assert_eq!(countdown(Some(&json!(1e308)), NOW), None);
        assert_eq!(countdown(Some(&json!(-1e300)), NOW), None);
        // And a clock that is wildly wrong does not panic or invent a countdown.
        assert_eq!(countdown(Some(&json!(1.5)), f64::NAN), None);
        assert_eq!(countdown(Some(&json!(1.5)), f64::INFINITY), None);
    }

    // ---- quota ----

    #[test]
    fn quota_with_countdown() {
        let input = json!({"rate_limits": {"five_hour": {"used_percentage": 78, "resets_at": NOW + 6060.0}}});
        assert_eq!(
            quota(&input, "five_hour", "5h", false, &plain()).as_deref(),
            Some("5h:[######--]78%(1h41m)")
        );
    }

    #[test]
    fn quota_colors_match_the_hud_byte_for_byte() {
        let five = json!({"rate_limits": {"five_hour": {"used_percentage": 70, "resets_at": NOW + 6300.0}}});
        assert_eq!(
            quota(&five, "five_hour", "5h", false, &color()).as_deref(),
            Some("5h:[\x1b[33m######\x1b[2m--\x1b[0m]\x1b[33m70%\x1b[0m\x1b[2m(1h45m)\x1b[0m")
        );
        let week = json!({"rate_limits": {"seven_day": {"used_percentage": 18, "resets_at": NOW + 6.0 * 86_400.0 + 20.0 * 3600.0}}});
        assert_eq!(
            quota(&week, "seven_day", "wk", true, &color()).as_deref(),
            Some(
                "\x1b[2mwk:\x1b[0m[\x1b[32m#\x1b[2m-------\x1b[0m]\x1b[32m18%\x1b[0m\x1b[2m(6d20h)\x1b[0m"
            )
        );
    }

    #[test]
    fn quota_without_reset_or_with_past_reset_has_no_parens() {
        let none = json!({"rate_limits": {"five_hour": {"used_percentage": 10}}});
        assert_eq!(
            quota(&none, "five_hour", "5h", false, &plain()).as_deref(),
            Some("5h:[#-------]10%")
        );
        let past =
            json!({"rate_limits": {"five_hour": {"used_percentage": 10, "resets_at": NOW - 5.0}}});
        assert_eq!(
            quota(&past, "five_hour", "5h", false, &plain()).as_deref(),
            Some("5h:[#-------]10%")
        );
    }

    #[test]
    fn quota_needs_a_numeric_percentage() {
        for bad in [
            json!({"rate_limits": {"five_hour": {"resets_at": NOW + 60.0}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": "78"}}}),
            json!({"rate_limits": {"five_hour": {"used_percentage": null}}}),
            json!({"rate_limits": {"five_hour": 78}}),
            json!({"rate_limits": {"five_hour": null}}),
            json!({"rate_limits": []}),
            json!({"rate_limits": "x"}),
            json!({}),
        ] {
            assert_eq!(
                quota(&bad, "five_hour", "5h", false, &plain()),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn quotas_join_present_windows() {
        let both = json!({"rate_limits": {
            "five_hour": {"used_percentage": 78, "resets_at": NOW + 6060.0},
            "seven_day": {"used_percentage": 73, "resets_at": NOW + 6.0 * 86_400.0 + 5.0 * 3600.0}}});
        assert_eq!(
            quotas(&both, &plain()).as_deref(),
            Some("5h:[######--]78%(1h41m) wk:[######--]73%(6d5h)")
        );
        let only_wk = json!({"rate_limits": {"seven_day": {"used_percentage": 5}}});
        assert_eq!(
            quotas(&only_wk, &plain()).as_deref(),
            Some("wk:[--------]5%")
        );
        assert_eq!(quotas(&json!({}), &plain()), None);
    }

    #[test]
    fn spend_shows_the_percentage_past_the_limit_and_dollars_once_both_arrive() {
        let at = |s: Value| spend(&json!({"rate_limits": {"spend_limit": s}}), &plain());
        assert_eq!(
            at(json!({"used_percentage": 63, "resets_at": NOW + 6060.0})).as_deref(),
            Some("spend:[#####---]63%(1h41m)")
        );
        assert_eq!(
            at(json!({"used_percentage": 63, "used_usd": 314.12, "limit_usd": 500})).as_deref(),
            Some("spend:[#####---]63% $314.12/$500")
        );
        // The dollars come from a separate request; one alone is not shown.
        assert_eq!(
            at(json!({"used_percentage": 63, "used_usd": 314.12})).as_deref(),
            Some("spend:[#####---]63%")
        );
        // Bad amounts drop the dollars: negative, so large they would flood the
        // line (also once rounded up to a billion), or a limit of nothing.
        for (used, limit) in [
            (json!(-1), json!(500)),
            (json!(1e300), json!(500)),
            (json!(1), json!(1e9)),
            (json!(1), json!(999_999_999.999)),
            (json!(0), json!(0)),
            (json!(5), json!(0.001)),
        ] {
            assert_eq!(
                at(json!({"used_percentage": 63, "used_usd": used, "limit_usd": limit})).as_deref(),
                Some("spend:[#####---]63%"),
                "{used} {limit}"
            );
        }
        // Rounded to the cent before deciding whether it is whole; -0 prints as 0.
        assert_eq!(
            at(json!({"used_percentage": 63, "used_usd": 314.999, "limit_usd": 499.995}))
                .as_deref(),
            Some("spend:[#####---]63% $315/$500")
        );
        assert_eq!(
            at(json!({"used_percentage": 0, "used_usd": -0.0, "limit_usd": 500})).as_deref(),
            Some("spend:[--------]0% $0/$500")
        );
        // Over the limit: a clamped 100% would hide by how much.
        assert_eq!(
            at(json!({"used_percentage": 112.4})).as_deref(),
            Some("spend:[########]112%")
        );
        assert_eq!(
            at(json!({"used_percentage": 1e9})).as_deref(),
            Some("spend:[########]999%")
        );
        assert_eq!(
            at(json!({"used_percentage": -3})).as_deref(),
            Some("spend:[--------]0%")
        );
        for bad in [
            json!({"used_percentage": "63"}),
            json!({"resets_at": NOW + 60.0}),
            json!(63),
            json!(null),
        ] {
            assert_eq!(at(bad.clone()), None, "{bad}");
        }
    }

    #[test]
    fn spend_colors_follow_the_quota_levels() {
        let input = json!({"rate_limits": {"spend_limit": {
            "used_percentage": 120, "used_usd": 600, "limit_usd": 500}}});
        assert_eq!(
            spend(&input, &color()).as_deref(),
            Some(
                "\x1b[2mspend:\x1b[0m[\x1b[31m########\x1b[2m\x1b[0m]\x1b[31m120%\x1b[0m\x1b[2m $600/$500\x1b[0m"
            )
        );
        for (pct, want) in [(69, GREEN), (70, YELLOW), (89, YELLOW), (90, RED)] {
            let input = json!({"rate_limits": {"spend_limit": {"used_percentage": pct}}});
            let got = spend(&input, &color()).unwrap();
            assert!(got.contains(&format!("{want}{pct}%")), "{pct}: {got:?}");
        }
    }

    // ---- model ----

    #[test]
    fn model_names_follow_the_huds_versioned_format() {
        for (given, want) in [
            ("Opus 5.5", "Opus 5.5"),
            ("Opus 4.8", "Opus 4.8"),
            ("Haiku 4.5", "Haiku 4.5"),
            ("Sonnet 5.5 (1M context)", "Sonnet 5.5"),
            ("Opus", "Opus"),
            ("claude-opus-5-5", "Opus 5.5"),
            ("claude-sonnet-4-5-20250929", "Sonnet 4.5"),
            ("claude-opus-4-20250514", "Opus 4"),
            ("claude-opus-4-1-20250805", "Opus 4.1"),
            ("claude-haiku-4-5", "Haiku 4.5"),
            ("anthropic.claude-opus-5", "Opus 5"),
            ("claude-3-5-sonnet-20241022", "Sonnet 3.5"),
            ("claude-3-opus", "Opus 3"),
            ("claude-sonnet", "Sonnet"),
            ("OPUS 5.5", "Opus 5.5"),
            ("Opus  5", "Opus 5"),
            ("Opus 5.", "Opus 5"),
            ("Sonnet 5.5 (Opus plan)", "Opus 5.5"),
            ("Fable 5.1", "Fable 5.1"),
            ("gpt-6", "gpt-6"),
        ] {
            assert_eq!(model_name(given), want, "{given:?}");
        }
        // Names with no known tier are cut to 20 columns.
        assert_eq!(
            model_name("some-very-long-model-identifier"),
            "some-very-long-mo..."
        );
    }

    #[test]
    fn model_segment_label_name_and_effort() {
        let input = json!({"model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"}, "effort": {"level": "xhigh"}});
        assert_eq!(
            model(&input, &plain()).as_deref(),
            Some("Model: Opus 5.5 xhigh")
        );
        assert_eq!(
            model(&input, &color()).as_deref(),
            Some("\x1b[36mModel: Opus 5.5 xhigh\x1b[0m")
        );
    }

    #[test]
    fn model_partial_and_fallbacks() {
        let name_only = json!({"model": {"display_name": "Sonnet 5.5"}});
        assert_eq!(
            model(&name_only, &plain()).as_deref(),
            Some("Model: Sonnet 5.5")
        );
        let id_only = json!({"model": {"id": "claude-haiku-4-5"}});
        assert_eq!(
            model(&id_only, &plain()).as_deref(),
            Some("Model: Haiku 4.5")
        );
        let effort_only = json!({"effort": {"level": "high"}});
        assert_eq!(model(&effort_only, &plain()).as_deref(), Some("high"));
        let blank_name = json!({"model": {"display_name": "  ", "id": "claude-opus-5-5"}});
        assert_eq!(
            model(&blank_name, &plain()).as_deref(),
            Some("Model: Opus 5.5")
        );
    }

    #[test]
    fn model_wrong_types_are_dropped() {
        for bad in [
            json!({}),
            json!({"model": "Opus"}),
            json!({"model": {"display_name": 5}, "effort": {"level": ["x"]}}),
            json!({"model": null, "effort": null}),
            json!({"effort": "xhigh"}),
        ] {
            assert_eq!(model(&bad, &plain()), None, "{bad}");
        }
    }

    // ---- text hygiene ----

    #[test]
    fn text_fields_cannot_inject_escapes_or_flood() {
        let input = json!({"model": {"display_name": "Op\u{1b}[31mus\n\u{9b}x"}, "effort": {"level": "a".repeat(500)}});
        let got = model(&input, &plain()).unwrap();
        assert!(!got.chars().any(char::is_control), "{got:?}");
        assert!(got.chars().count() <= 2 * TEXT_MAX_CHARS + 8);
    }

    #[test]
    fn invisible_format_characters_are_dropped() {
        let hostile = [
            '\u{200b}',
            '\u{200c}',
            '\u{200d}',
            '\u{200e}',
            '\u{200f}',
            '\u{061c}',
            '\u{2028}',
            '\u{2029}',
            '\u{202a}',
            '\u{202b}',
            '\u{202c}',
            '\u{202d}',
            '\u{202e}',
            '\u{2060}',
            '\u{2061}',
            '\u{2062}',
            '\u{2063}',
            '\u{2064}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{feff}',
            '\u{e0000}',
            '\u{e0041}',
            '\u{e007f}',
        ];
        for c in hostile {
            assert_eq!(clean(&format!("a{c}b")), "ab", "U+{:04X}", c as u32);
        }
        // Their neighbours are ordinary text and stay.
        for c in [
            '\u{2010}',
            '\u{202f}',
            '\u{061b}',
            '\u{061d}',
            '\u{2065}',
            '\u{206a}',
            '\u{e0080}',
            '\u{00e9}',
            '\u{4e2d}',
            '\u{1f600}',
        ] {
            assert_eq!(
                clean(&format!("a{c}b")),
                format!("a{c}b"),
                "U+{:04X}",
                c as u32
            );
        }
        // Bidi controls cannot be used to flip the rest of the line.
        let flipped = branch_segment(
            &Facts {
                branch: Some("main\u{202e}gnp.exe".into()),
                ..facts()
            },
            &plain(),
        );
        assert_eq!(flipped.as_deref(), Some("branch:maingnp.exe"));
    }

    // ---- git line ----

    #[test]
    fn branch_segment_bytes_and_worktree() {
        let f = |b: &str, wt: Option<&str>| Facts {
            branch: Some(b.into()),
            worktree: wt.map(String::from),
            ..facts()
        };
        assert_eq!(
            branch_segment(&f("main", None), &plain()).as_deref(),
            Some("branch:main")
        );
        assert_eq!(
            branch_segment(&f("main", None), &color()).as_deref(),
            Some("\x1b[2mbranch:\x1b[0m\x1b[36mmain\x1b[0m")
        );
        assert_eq!(
            branch_segment(&f("feat", Some("wt-1")), &color()).as_deref(),
            Some(
                "\x1b[2mbranch:\x1b[0m\x1b[36mfeat\x1b[0m \x1b[2m(wt:\x1b[0m\x1b[36mwt-1\x1b[0m\x1b[2m)\x1b[0m"
            )
        );
        assert_eq!(
            branch_segment(&f("feat", Some("wt-1")), &plain()).as_deref(),
            Some("branch:feat (wt:wt-1)")
        );
        assert_eq!(
            branch_segment(&f("a\u{1b}b", None), &plain()).as_deref(),
            Some("branch:ab")
        );
        assert_eq!(branch_segment(&f(" \n", Some("wt")), &plain()), None);
        assert_eq!(branch_segment(&facts(), &plain()), None);
        // A blank worktree name adds nothing.
        assert_eq!(
            branch_segment(&f("m", Some(" ")), &plain()).as_deref(),
            Some("branch:m")
        );
    }

    #[test]
    fn status_segment_bytes() {
        let c = Counts {
            staged: 2,
            modified: 30,
            untracked: 103,
            ahead: 5,
            behind: 2,
        };
        let f = |c: Counts| Facts {
            status: Some(c),
            ..facts()
        };
        assert_eq!(
            status_segment(&f(c), &plain()).as_deref(),
            Some("+2 !30 ?103 \u{21e1}5 \u{21e3}2")
        );
        assert_eq!(
            status_segment(&f(Counts { staged: 0, ..c }), &color()).as_deref(),
            Some(
                "\x1b[31m!\x1b[0m30 \x1b[36m?\x1b[0m103 \x1b[32m\u{21e1}\x1b[0m5 \x1b[31m\u{21e3}\x1b[0m2"
            )
        );
        assert_eq!(
            status_segment(
                &f(Counts {
                    untracked: 3,
                    ..Counts::default()
                }),
                &plain()
            )
            .as_deref(),
            Some("?3")
        );
        assert_eq!(
            status_segment(&f(Counts::default()), &plain()),
            None,
            "clean shows nothing"
        );
        assert_eq!(status_segment(&facts(), &plain()), None);
    }

    #[test]
    fn profile_segment_is_bold() {
        let f = Facts {
            profile: Some("claude-work".into()),
            ..facts()
        };
        assert_eq!(
            profile_segment(&f, &color()).as_deref(),
            Some("\x1b[1mprofile:claude-work\x1b[0m")
        );
        assert_eq!(
            profile_segment(
                &Facts {
                    profile: Some("\u{1b}".into()),
                    ..facts()
                },
                &plain()
            ),
            None
        );
    }

    // ---- OMC and Claude notices ----

    fn plugin(version: &str, local: bool) -> Option<Plugin> {
        Some(Plugin {
            version: Some(version.into()),
            local,
        })
    }

    fn updates(omc: Option<&str>, marketplace: bool, claude: Option<&str>) -> Option<Updates> {
        Some(Updates {
            omc: omc.map(Into::into),
            marketplace,
            claude: claude.map(Into::into),
        })
    }

    #[test]
    fn omc_label_tag_and_update() {
        let f = |p: Option<Plugin>, u: Option<Updates>| Facts {
            plugin: p,
            updates: u,
            ..facts()
        };
        assert_eq!(
            omc_label(&f(plugin("5.6.1", true), None), &plain()).as_deref(),
            Some("[OMC#5.6.1L]")
        );
        assert_eq!(
            omc_label(&f(plugin("5.6.1", false), None), &plain()).as_deref(),
            Some("[OMC#5.6.1]")
        );
        assert_eq!(
            omc_label(
                &f(
                    Some(Plugin {
                        version: None,
                        local: true
                    }),
                    None
                ),
                &plain()
            )
            .as_deref(),
            Some("[OMC#L]")
        );
        assert_eq!(
            omc_label(
                &f(
                    Some(Plugin {
                        version: None,
                        local: false
                    }),
                    None
                ),
                &plain()
            )
            .as_deref(),
            Some("[OMC]")
        );
        assert_eq!(omc_label(&f(None, None), &plain()), None);
        assert_eq!(
            omc_label(&f(plugin("5.6.1", true), None), &color()).as_deref(),
            Some("\x1b[1m[OMC#5.6.1L]\x1b[0m")
        );
        let newer = f(plugin("5.6.1", true), updates(Some("5.7.0"), false, None));
        assert_eq!(
            omc_label(&newer, &plain()).as_deref(),
            Some("[OMC#5.6.1L] -> 5.7.0 omc update")
        );
        let same = f(plugin("5.6.1", true), updates(Some("5.6.1"), false, None));
        assert_eq!(omc_label(&same, &plain()).as_deref(), Some("[OMC#5.6.1L]"));
        let older = f(plugin("5.6.1", true), updates(Some("5.5.0"), false, None));
        assert_eq!(omc_label(&older, &plain()).as_deref(), Some("[OMC#5.6.1L]"));
    }

    #[test]
    fn claude_label_only_when_newer_is_known() {
        let input = json!({"version": "2.1.289"});
        let f = |claude: Option<&str>| Facts {
            updates: updates(None, false, claude),
            ..facts()
        };
        assert_eq!(
            claude_label(&input, &f(Some("2.1.291")), &plain()).as_deref(),
            Some("[Claude#2.1.289] -> 2.1.291 claude update")
        );
        assert_eq!(
            claude_label(&input, &f(Some("2.1.291")), &color()).as_deref(),
            Some("\x1b[1m[Claude#2.1.289] -> 2.1.291 claude update\x1b[0m")
        );
        assert_eq!(
            claude_label(&input, &f(Some("2.1.289")), &plain()),
            None,
            "up to date"
        );
        assert_eq!(
            claude_label(&input, &f(Some("2.1.200")), &plain()),
            None,
            "ahead of the check"
        );
        assert_eq!(claude_label(&input, &f(None), &plain()), None);
        assert_eq!(
            claude_label(&json!({}), &f(Some("2.1.291")), &plain()),
            None,
            "no running version"
        );
        assert_eq!(
            claude_label(&json!({"version": 5}), &f(Some("2.1.291")), &plain()),
            None
        );
        assert_eq!(claude_label(&input, &facts(), &plain()), None);
    }

    #[test]
    fn update_hint_lines() {
        let input = json!({"version": "2.1.289"});
        let f = |omc: Option<&str>, market: bool| Facts {
            plugin: plugin("5.6.1", true),
            updates: updates(omc, market, Some("2.1.291")),
            ..facts()
        };
        let lines = |f: &Facts, o: &Opts| {
            update_hints(&input, f, o)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(&f(None, false), &plain()),
            ["[!] claude 2.1.291 - paste: ! claude update"]
        );
        assert_eq!(
            lines(&f(None, false), &color()),
            ["\x1b[33m\x1b[1m[!] claude 2.1.291 - paste: ! claude update\x1b[0m"]
        );
        assert_eq!(
            lines(&f(Some("5.7.0"), true), &plain()),
            [
                "[!] omc 5.7.0 available",
                "[!] claude 2.1.291 - paste: ! claude update"
            ]
        );
        assert_eq!(
            lines(&f(Some("5.7.0"), false), &plain())[0],
            "[!] omc 5.7.0 - paste: ! npm i -g oh-my-claude-sisyphus@latest"
        );
        assert!(lines(&facts(), &plain()).is_empty());
    }

    // ---- context ----

    fn ctx_input(window: Value) -> Value {
        json!({"context_window": window})
    }

    #[test]
    fn context_percent_prefers_the_reported_figure() {
        assert_eq!(
            context_percent(&ctx_input(json!({"used_percentage": 28}))),
            28
        );
        assert_eq!(
            context_percent(&ctx_input(json!({"used_percentage": 28.5}))),
            29
        );
        assert_eq!(
            context_percent(&ctx_input(json!({"used_percentage": 250}))),
            100
        );
        assert_eq!(
            context_percent(&ctx_input(json!({"used_percentage": 1e308}))),
            100
        );
    }

    #[test]
    fn context_percent_falls_back_to_tokens_like_the_hud() {
        let usage = json!({"input_tokens": 100, "cache_creation_input_tokens": 400, "cache_read_input_tokens": 99_500});
        // A zero or missing figure falls through to the current request's tokens...
        for reported in [json!(0), json!(null), json!("x"), json!(-3)] {
            let w = json!({"used_percentage": reported, "context_window_size": 1000, "current_usage": usage});
            assert_eq!(context_percent(&ctx_input(w)), 100, "{reported}");
        }
        let w = json!({"context_window_size": 200_000, "current_usage": usage});
        assert_eq!(context_percent(&ctx_input(w)), 50);
        // ...then to the session's input tokens...
        let w = json!({"context_window_size": 1000, "total_input_tokens": 333});
        assert_eq!(context_percent(&ctx_input(w)), 33);
        // ...and an unusable window size means nothing to divide by.
        for size in [json!(0), json!(-5), json!(null), json!("big")] {
            let w = json!({"context_window_size": size, "current_usage": usage, "total_input_tokens": 5});
            assert_eq!(context_percent(&ctx_input(w)), 0, "{size}");
        }
        assert_eq!(context_percent(&json!({})), 0);
        assert_eq!(context_percent(&json!(null)), 0);
        assert_eq!(context_percent(&ctx_input(json!([1]))), 0);
    }

    #[test]
    fn context_segment_bytes_and_severity_suffixes() {
        let at = |p: u8, o: &Opts| context_segment(p, o);
        assert_eq!(at(28, &plain()), "ctx:[###-------]28%");
        assert_eq!(at(0, &plain()), "ctx:[----------]0%");
        assert_eq!(
            at(28, &color()),
            "ctx:[\x1b[32m###\x1b[2m-------\x1b[0m]\x1b[32m28%\x1b[0m"
        );
        assert_eq!(at(70, &plain()), "ctx:[#######---]70%");
        assert!(at(70, &color()).contains(YELLOW) && !at(70, &plain()).contains("COMPRESS"));
        assert_eq!(at(79, &plain()), "ctx:[########--]79%");
        assert_eq!(at(80, &plain()), "ctx:[########--]80% COMPRESS?");
        assert_eq!(at(84, &plain()), "ctx:[########--]84% COMPRESS?");
        assert_eq!(at(85, &plain()), "ctx:[#########-]85% CRITICAL");
        assert_eq!(
            at(88, &color()),
            "ctx:[\x1b[31m#########\x1b[2m-\x1b[0m]\x1b[31m88% CRITICAL\x1b[0m"
        );
        assert_eq!(at(100, &plain()), "ctx:[##########]100% CRITICAL");
    }

    #[test]
    fn context_notice_line() {
        assert_eq!(context_notice(79, &plain()), None);
        assert_eq!(
            context_notice(80, &plain()).as_deref(),
            Some("[!] ctx 80% >= 80% threshold - run /compact")
        );
        assert_eq!(
            context_notice(89, &color()).as_deref(),
            Some("\x1b[33m\x1b[1m[!] ctx 89% >= 80% threshold - run /compact\x1b[0m")
        );
        assert_eq!(
            context_notice(90, &color()).as_deref(),
            Some("\x1b[31m\x1b[1m[!!] ctx 90% >= 80% threshold - run /compact\x1b[0m")
        );
    }

    // ---- prompt cache ----

    fn cache(fields: Value) -> Option<String> {
        cache_with(fields, &plain())
    }

    fn cache_with(fields: Value, o: &Opts) -> Option<String> {
        let mut c = json!({"caching_observed": true, "warm": true});
        for (k, v) in fields.as_object().unwrap() {
            c[k] = v.clone();
        }
        prompt_cache(&json!({ "prompt_cache": c }), o)
    }

    #[test]
    fn prompt_cache_counts_down_while_warm() {
        for (ahead, want) in [
            (272.0, "cache:4m"),
            (60.0, "cache:1m"),
            (3599.0, "cache:59m"),
            (3600.0, "cache:60m"),
            (3660.0, "cache:61m"),
            (3661.0, "cache:warm"),
            (42.7, "cache:42s"),
            (0.3, "cache:1s"),
        ] {
            assert_eq!(
                cache(json!({"expires_at": NOW + ahead})).as_deref(),
                Some(want),
                "{ahead}"
            );
        }
        // A real epoch, so that the expiry in milliseconds is past 1e12.
        let now = 1_750_000_000.0;
        let o = Opts { now, ..plain() };
        assert_eq!(
            cache_with(json!({"expires_at": (now + 272.0) * 1000.0}), &o).as_deref(),
            Some("cache:4m")
        );
    }

    #[test]
    fn prompt_cache_without_a_usable_expiry_reads_warm() {
        for expires in [json!(null), json!("soon"), json!(NOW + 3.0 * 3600.0)] {
            assert_eq!(
                cache(json!({"expires_at": expires})).as_deref(),
                Some("cache:warm"),
                "{expires}"
            );
        }
        assert_eq!(cache(json!({})).as_deref(), Some("cache:warm"));
    }

    #[test]
    fn prompt_cache_reads_cold_once_expired_or_not_warm() {
        // The payload can be older than the expiry: an idle session's last
        // render still says warm.
        for expires in [NOW, NOW - 5.0] {
            assert_eq!(
                cache(json!({"expires_at": expires})).as_deref(),
                Some("cache:cold"),
                "{expires}"
            );
        }
        for warm in [json!(false), json!("true"), json!(null)] {
            assert_eq!(
                cache(json!({"warm": warm, "expires_at": NOW + 272.0})).as_deref(),
                Some("cache:cold"),
                "{warm}"
            );
        }
        assert_eq!(
            cache(json!({"warm": false, "recache_tokens_if_cold": 45000})).as_deref(),
            Some("cache:cold(45k)")
        );
        for recache in [json!(null), json!(0), json!("45000")] {
            assert_eq!(
                cache(json!({"warm": false, "recache_tokens_if_cold": recache})).as_deref(),
                Some("cache:cold"),
                "{recache}"
            );
        }
        // The re-cache cost is for a cold cache only.
        assert_eq!(
            cache(json!({"expires_at": NOW + 272.0, "recache_tokens_if_cold": 45000})).as_deref(),
            Some("cache:4m")
        );
    }

    #[test]
    fn prompt_cache_left_out_unless_caching_was_observed() {
        for observed in [json!(false), json!("true"), json!(1), json!(null)] {
            assert_eq!(
                cache(json!({"caching_observed": observed})),
                None,
                "{observed}"
            );
        }
        for input in [
            json!({}),
            json!({"prompt_cache": null}),
            json!({"prompt_cache": true}),
            json!({"prompt_cache": {"warm": true}}),
        ] {
            assert_eq!(prompt_cache(&input, &plain()), None, "{input}");
        }
    }

    #[test]
    fn prompt_cache_misses_name_the_last_cause() {
        let tools = json!({"causes": ["tools_changed"], "tools_added": 2, "tools_removed": 0});
        assert_eq!(
            cache(json!({"misses": 2, "last_miss_cause": tools, "expires_at": NOW + 272.0}))
                .as_deref(),
            Some("cache:4m miss:2(tools_changed)")
        );
        assert_eq!(
            cache(json!({"misses": 1, "last_miss_cause": null, "warm": false})).as_deref(),
            Some("cache:cold miss:1")
        );
        assert_eq!(
            cache(json!({"misses": 3, "last_miss_cause": {"causes": ["model_changed", 7, "\u{1b}[31mtools_changed"]}}))
                .as_deref(),
            Some("cache:warm miss:3(model_changed,[31mtools_changed)")
        );
        // Whole names only: two, then a count of the rest.
        let five = json!({"causes": ["tools_changed", "system_prompt_changed", "ttl_expired_5m", "a", "b"]});
        assert_eq!(
            cache(json!({"misses": 2, "last_miss_cause": five})).as_deref(),
            Some("cache:warm miss:2(tools_changed,system_prompt_changed+3)")
        );
        let long = "x".repeat(100);
        let got = cache(json!({"misses": 1, "last_miss_cause": {"causes": [long]}})).unwrap();
        assert_eq!(got, format!("cache:warm miss:1({})", "x".repeat(64)));
        for (misses, want) in [
            (999, "cache:warm miss:999"),
            (1000, "cache:warm miss:999+"),
            (u64::MAX, "cache:warm miss:999+"),
        ] {
            assert_eq!(
                cache(json!({ "misses": misses })).as_deref(),
                Some(want),
                "{misses}"
            );
        }
        for misses in [json!(0), json!(-1), json!("2"), json!(2.5), json!(null)] {
            assert_eq!(
                cache(json!({"misses": misses, "last_miss_cause": tools})).as_deref(),
                Some("cache:warm"),
                "{misses}"
            );
        }
    }

    #[test]
    fn prompt_cache_colors() {
        assert_eq!(
            cache_with(json!({"expires_at": NOW + 272.0}), &color()).as_deref(),
            Some("cache:\x1b[32m4m\x1b[0m")
        );
        assert_eq!(
            cache_with(json!({"expires_at": NOW + 42.0}), &color()).as_deref(),
            Some("cache:\x1b[33m42s\x1b[0m")
        );
        let cold = json!({"warm": false, "recache_tokens_if_cold": 45000, "misses": 2,
            "last_miss_cause": {"causes": ["ttl_expired_5m"]}});
        let got = cache_with(cold.clone(), &color()).unwrap();
        assert_eq!(
            got,
            "cache:\x1b[33mcold\x1b[0m\x1b[2m(45k)\x1b[0m \x1b[33mmiss:2\x1b[0m\x1b[2m(ttl_expired_5m)\x1b[0m"
        );
        assert_eq!(strip_ansi(&got), cache(cold).unwrap());
        // A miss more than five minutes old is dim; a recent one stays yellow.
        let at = |ago: f64| {
            cache_with(json!({"misses": 1, "last_miss_at": NOW - ago}), &color()).unwrap()
        };
        assert_eq!(at(300.0), "cache:\x1b[32mwarm\x1b[0m \x1b[33mmiss:1\x1b[0m");
        assert_eq!(at(301.0), "cache:\x1b[32mwarm\x1b[0m \x1b[2mmiss:1\x1b[0m");
    }

    #[test]
    fn token_counts_fit_a_few_columns() {
        for (n, want) in [
            (950.0, "950"),
            (999.6, "999"),
            (1000.0, "1k"),
            (45_000.0, "45k"),
            (999_499.0, "999k"),
            (999_500.0, "1M"),
            (1_234_567.0, "1.2M"),
        ] {
            assert_eq!(tokens(n), want, "{n}");
        }
        // A billion or more is a bad value, not a count to print in full.
        for n in [1e9, 1e300] {
            assert_eq!(
                cache(json!({"warm": false, "recache_tokens_if_cold": n})).as_deref(),
                Some("cache:cold"),
                "{n}"
            );
        }
    }

    // ---- session, thinking, skill, counts ----

    #[test]
    fn session_minutes_floor_and_default_to_zero() {
        let at = |ms: Value| session(&json!({"cost": {"total_duration_ms": ms}}), 0, &plain());
        assert_eq!(at(json!(0.0)), "session:0m");
        assert_eq!(at(json!(59_999.0)), "session:0m");
        assert_eq!(at(json!(82_072_574.0)), "session:1367m");
        for bad in [json!(-1), json!("5"), json!(null)] {
            assert_eq!(at(bad), "session:0m");
        }
        assert_eq!(session(&json!({}), 0, &plain()), "session:0m");
    }

    #[test]
    fn session_color_follows_health() {
        let at = |mins: u64, ctx: u8| {
            session(
                &json!({"cost": {"total_duration_ms": mins * 60_000}}),
                ctx,
                &color(),
            )
        };
        assert!(at(60, 70).contains(GREEN));
        assert!(at(61, 0).contains(YELLOW));
        assert!(at(10, 71).contains(YELLOW));
        assert!(at(120, 85).contains(YELLOW));
        assert!(at(121, 0).contains(RED));
        assert!(at(10, 86).contains(RED));
        assert_eq!(at(5840, 88), "session:\x1b[31m5840m\x1b[0m");
    }

    #[test]
    fn thinking_is_live_for_thirty_seconds_after_the_newest_block() {
        let f = |at: Option<f64>| Facts {
            calls: Some(Calls {
                thinking_at: at,
                ..Calls::default()
            }),
            ..facts()
        };
        assert_eq!(
            thinking(&f(Some(NOW - 5.0)), &plain()).as_deref(),
            Some("thinking")
        );
        assert_eq!(
            thinking(&f(Some(NOW - 30.0)), &color()).as_deref(),
            Some("\x1b[36mthinking\x1b[0m")
        );
        assert_eq!(thinking(&f(Some(NOW - 30.1)), &plain()), None);
        assert_eq!(
            thinking(&f(Some(NOW + 3600.0)), &plain()).as_deref(),
            Some("thinking"),
            "future stamp: the HUD agreed"
        );
        assert_eq!(thinking(&f(None), &plain()), None);
        assert_eq!(thinking(&facts(), &plain()), None);
    }

    #[test]
    fn last_skill_shows_the_bare_name_and_truncated_args() {
        let f = |name: &str, args: Option<&str>| Facts {
            calls: Some(Calls {
                last_skill: Some(Skill {
                    name: name.into(),
                    args: args.map(Into::into),
                }),
                ..Calls::default()
            }),
            ..facts()
        };
        assert_eq!(
            last_skill(
                &f(
                    "oh-my-claudecode:session-handoff",
                    Some("Detailed handoff for x")
                ),
                &plain()
            )
            .as_deref(),
            Some("skill:session-handoff(Detailed han...)")
        );
        assert_eq!(
            last_skill(&f("plan", None), &plain()).as_deref(),
            Some("skill:plan")
        );
        assert_eq!(
            last_skill(&f("plan", Some("short")), &plain()).as_deref(),
            Some("skill:plan(short)")
        );
        assert_eq!(
            last_skill(&f("plan", Some("exactly15chars!")), &plain()).as_deref(),
            Some("skill:plan(exactly15chars!)")
        );
        assert_eq!(
            last_skill(&f("plan", Some("")), &plain()).as_deref(),
            Some("skill:plan")
        );
        assert_eq!(
            last_skill(&f("a:b:c", None), &color()).as_deref(),
            Some("\x1b[36mskill:c\x1b[0m")
        );
        assert_eq!(
            last_skill(&f("trailing:", None), &plain()).as_deref(),
            Some("skill:trailing:")
        );
        assert_eq!(last_skill(&f("\u{1b}", None), &plain()), None);
        assert_eq!(last_skill(&facts(), &plain()), None);
    }

    #[test]
    fn call_counts_skip_zeros() {
        let f = |tools, agents, skills| Facts {
            calls: Some(Calls {
                tools,
                agents,
                skills,
                ..Calls::default()
            }),
            ..facts()
        };
        assert_eq!(
            call_counts(&f(251, 4, 2)).as_deref(),
            Some("\u{1f527}251 \u{1f916}4 \u{26a1}2")
        );
        assert_eq!(call_counts(&f(251, 0, 0)).as_deref(), Some("\u{1f527}251"));
        assert_eq!(call_counts(&f(0, 3, 0)).as_deref(), Some("\u{1f916}3"));
        assert_eq!(call_counts(&f(0, 0, 0)), None);
        assert_eq!(call_counts(&facts()), None);
    }

    // ---- the whole line ----

    fn full_input() -> Value {
        json!({
            "version": "2.1.289",
            "model": {"display_name": "Opus 5.5"},
            "effort": {"level": "xhigh"},
            "context_window": {"used_percentage": 28},
            "rate_limits": {
                "five_hour": {"used_percentage": 78, "resets_at": NOW + 6060.0},
                "seven_day": {"used_percentage": 73, "resets_at": NOW + 6.0 * 86_400.0 + 5.0 * 3600.0}},
            "cost": {"total_duration_ms": 1283 * 60_000},
        })
    }

    fn full_facts() -> Facts {
        Facts {
            branch: Some("main".into()),
            worktree: None,
            status: Some(Counts {
                modified: 30,
                untracked: 103,
                ahead: 5,
                behind: 2,
                ..Counts::default()
            }),
            profile: None,
            plugin: plugin("5.6.1", true),
            updates: updates(None, false, Some("2.1.291")),
            calls: Some(Calls {
                tools: 251,
                agents: 4,
                thinking_at: Some(NOW - 2.0),
                ..Calls::default()
            }),
        }
    }

    #[test]
    fn full_layout_matches_the_hud() {
        assert_eq!(
            render(&full_input(), &full_facts(), &plain()),
            "branch:main | !30 ?103 \u{21e1}5 \u{21e3}2\n\
             [OMC#5.6.1L] | Model: Opus 5.5 xhigh | [Claude#2.1.289] -> 2.1.291 claude update | 5h:[######--]78%(1h41m) wk:[######--]73%(6d5h) | thinking | session:1283m | ctx:[###-------]28% | \u{1f527}251 \u{1f916}4\n\
             [!] claude 2.1.291 - paste: ! claude update"
        );
    }

    #[test]
    fn no_color_output_has_no_escapes_and_matches_stripped_color() {
        let plain_line = render(&full_input(), &full_facts(), &plain());
        let color_line = render(&full_input(), &full_facts(), &color());
        assert!(!plain_line.contains('\x1b'));
        assert!(color_line.contains('\x1b'));
        assert_eq!(strip_ansi(&color_line), plain_line);
    }

    #[test]
    fn color_separators_are_dim() {
        let got = render(
            &json!({"model": {"display_name": "M"}, "context_window": {"used_percentage": 1}}),
            &facts(),
            &color(),
        );
        assert!(got.contains("\x1b[2m | \x1b[0m"), "{got:?}");
    }

    #[test]
    fn nothing_known_still_shows_the_unconditional_segments() {
        // The HUD always printed session time and context, even for an empty payload.
        for input in [json!({}), json!(null), json!([1, 2, 3]), json!("text")] {
            assert_eq!(
                render(&input, &facts(), &plain()),
                "session:0m | ctx:[----------]0%",
                "{input}"
            );
        }
    }

    #[test]
    fn garbage_fields_drop_only_their_own_segment() {
        let input = json!({
            "model": {"display_name": 42},
            "effort": {"level": null},
            "thinking": {"enabled": "yes"},
            "context_window": {"used_percentage": "lots"},
            "rate_limits": {"five_hour": {"used_percentage": "78"}, "seven_day": {"used_percentage": 73},
                "spend_limit": {"used_percentage": "63"}},
            "cost": {"total_duration_ms": "long"},
            "prompt_cache": {"caching_observed": "yes", "warm": true},
        });
        assert_eq!(
            render(&input, &facts(), &plain()),
            "wk:[######--]73% | session:0m | ctx:[----------]0%"
        );
    }

    #[test]
    fn cache_follows_ctx_and_spend_follows_the_quotas() {
        let mut input = full_input();
        input["rate_limits"]["spend_limit"] = json!({"used_percentage": 12});
        input["prompt_cache"] =
            json!({"caching_observed": true, "warm": true, "expires_at": NOW + 272.0});
        let got = render(&input, &full_facts(), &plain());
        assert!(
            got.contains("wk:[######--]73%(6d5h) | spend:[#-------]12% | thinking"),
            "{got}"
        );
        assert!(
            got.contains("ctx:[###-------]28% | cache:4m | \u{1f527}251"),
            "{got}"
        );
        assert_eq!(strip_ansi(&render(&input, &full_facts(), &color())), got);
    }

    #[test]
    fn git_line_alone_when_only_git_is_known() {
        let f = Facts {
            branch: Some("main".into()),
            ..facts()
        };
        let got = render(&json!({}), &f, &plain());
        assert_eq!(got, "branch:main\nsession:0m | ctx:[----------]0%");
    }

    #[test]
    fn long_context_adds_the_notice_and_cuts_to_four_lines() {
        let mut input = full_input();
        input["context_window"] = json!({"used_percentage": 89});
        let got = render(&input, &full_facts(), &plain());
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 4, "{got}");
        assert_eq!(lines[2], "[!] ctx 89% >= 80% threshold - run /compact");
        assert_eq!(lines[3], "[!] claude 2.1.291 - paste: ! claude update");
        // A fifth line becomes a count.
        let mut f = full_facts();
        f.updates = updates(Some("5.7.0"), false, Some("2.1.291"));
        let got = render(&input, &f, &plain());
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 4, "{got}");
        assert_eq!(lines[3], "... (+2 lines)");
    }

    #[test]
    fn a_wrapped_main_line_keeps_ctx_and_the_notice_gives_way() {
        // At 80 columns the git and main lines fill all four rows. The notice
        // used to push the last row, with `ctx:`, behind `... (+2 lines)`.
        let mut input = full_input();
        input["context_window"] = json!({"used_percentage": 89});
        let mut f = full_facts();
        f.updates = updates(None, false, None);
        let o = |columns| Opts {
            color: false,
            now: NOW,
            columns,
        };
        let got = render(&input, &f, &o(80));
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 4, "{got}");
        assert_eq!(lines[3], "ctx:[#########-]89% CRITICAL | 🔧251 🤖4");
        // With a row to spare, the notice is back.
        let got = render(&input, &f, &o(100));
        assert_eq!(
            got.lines().last(),
            Some("[!] ctx 89% >= 80% threshold - run /compact")
        );
    }

    #[test]
    fn narrow_terminal_wraps_at_separators_and_never_exceeds_the_width() {
        let o = |columns| Opts {
            color: false,
            now: NOW,
            columns,
        };
        let got = render(&full_input(), &full_facts(), &o(100));
        for line in got.lines() {
            assert!(layout::width(line) <= 100, "{line:?}");
        }
        assert!(got.lines().count() > 3);
        // Wide enough: the same text as with no width at all.
        assert_eq!(
            render(&full_input(), &full_facts(), &o(400)),
            render(&full_input(), &full_facts(), &o(0))
        );
        // Absurdly narrow: still no panic and every line within the width.
        for cols in [1, 2, 3, 4, 10] {
            for line in render(&full_input(), &full_facts(), &o(cols)).lines() {
                assert!(layout::width(line) <= cols.max(3), "{cols}: {line:?}");
            }
        }
    }

    #[test]
    fn spend_wraps_whole_instead_of_losing_its_dollars() {
        let input = json!({"rate_limits": {
            "five_hour": {"used_percentage": 40, "resets_at": NOW + 8400.0},
            "seven_day": {"used_percentage": 70, "resets_at": NOW + 500_000.0},
            "spend_limit": {"used_percentage": 63, "resets_at": NOW + 500_000.0,
                "used_usd": 314.12, "limit_usd": 500}}});
        let o = Opts {
            color: false,
            now: NOW,
            columns: 80,
        };
        let got = render(&input, &facts(), &o);
        assert!(
            got.lines()
                .any(|l| l.starts_with("spend:[#####---]63%(5d18h) $314.12/$500 | ")),
            "{got}"
        );
    }

    #[test]
    fn wrapped_color_lines_keep_balanced_escapes() {
        let o = Opts {
            color: true,
            now: NOW,
            columns: 60,
        };
        for line in render(&full_input(), &full_facts(), &o).lines() {
            // Every line that opens a color closes it, so nothing bleeds into the next.
            assert!(
                line.matches("\x1b[0m").count() >= 1 || !line.contains('\x1b'),
                "{line:?}"
            );
            assert!(!line.ends_with(' '), "{line:?}");
        }
    }

    #[test]
    fn work_dir_prefers_workspace_then_cwd() {
        let both = json!({"cwd": "/c", "workspace": {"current_dir": "/w"}});
        assert_eq!(work_dir(&both), Some("/w"));
        assert_eq!(work_dir(&json!({"cwd": "/c"})), Some("/c"));
        assert_eq!(
            work_dir(&json!({"cwd": "/c", "workspace": {"current_dir": ""}})),
            Some("/c")
        );
        assert_eq!(
            work_dir(&json!({"cwd": 5, "workspace": {"current_dir": null}})),
            None
        );
        assert_eq!(work_dir(&json!(null)), None);
    }
}
