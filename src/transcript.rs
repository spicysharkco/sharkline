//! Call counters from the session transcript (a JSONL file Claude Code appends to).
//!
//! The HUD counted `tool_use` blocks in the last 4 MiB of the transcript on
//! every render. This reads the same window once, then only the bytes appended
//! since: the offset and the running counts live in a small per-session cache
//! file. Counted exactly as the HUD did:
//!
//! - tools: every `tool_use` block (it needs an `id` and a `name`)
//! - agents: those named `Task` or `Agent` (also with the `proxy_` prefix)
//! - skills: those named `Skill`; the last one's `skill` and the first
//!   `ARGS_KEPT_CHARS` characters of its `args` are kept
//! - thinking: the timestamp of the newest `thinking` or `reasoning` block

use crate::cache;
use memchr::memmem;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// How much of a transcript the first look reads: the HUD's window. A session
/// is hundreds of MiB after a few days; the counters are for the recent past.
const TAIL_BYTES: u64 = 4 << 20;
/// A render that finds more than this appended since the last one (a resumed
/// session, a copied file) starts over from the tail instead.
const MAX_CATCH_UP_BYTES: u64 = 64 << 20;
const CACHE_VERSION: u64 = 1;
/// The status line shows a few columns of a skill's arguments, so no more is
/// kept: the cache file would otherwise hold whatever was typed into them.
const ARGS_KEPT_CHARS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calls {
    pub tools: u64,
    pub agents: u64,
    pub skills: u64,
    pub last_skill: Option<Skill>,
    /// Unix seconds of the newest thinking block; `None` when there is none or
    /// its timestamp did not parse.
    pub thinking_at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub args: Option<String>,
}

/// What a cached scan knows about one transcript file.
struct State {
    path: String,
    dev: u64,
    ino: u64,
    /// Bytes consumed: always just past a newline.
    offset: u64,
    calls: Calls,
}

impl State {
    fn to_json(&self) -> Value {
        json!({
            "v": CACHE_VERSION,
            "path": self.path,
            "dev": self.dev,
            "ino": self.ino,
            "off": self.offset,
            "tools": self.calls.tools,
            "agents": self.calls.agents,
            "skills": self.calls.skills,
            "skill": self.calls.last_skill.as_ref().map(|s| json!({"n": s.name, "a": s.args})),
            "think": self.calls.thinking_at,
        })
    }

    fn from_json(doc: &Value) -> Option<State> {
        if doc.get("v")?.as_u64()? != CACHE_VERSION {
            return None;
        }
        let last_skill = match doc.get("skill")? {
            Value::Null => None,
            s => Some(Skill {
                name: s.get("n")?.as_str()?.to_string(),
                args: match s.get("a")? {
                    Value::Null => None,
                    a => Some(a.as_str()?.to_string()),
                },
            }),
        };
        let thinking_at = match doc.get("think")? {
            Value::Null => None,
            t => Some(t.as_f64()?),
        };
        Some(State {
            path: doc.get("path")?.as_str()?.to_string(),
            dev: doc.get("dev")?.as_u64()?,
            ino: doc.get("ino")?.as_u64()?,
            offset: doc.get("off")?.as_u64()?,
            calls: Calls {
                tools: doc.get("tools")?.as_u64()?,
                agents: doc.get("agents")?.as_u64()?,
                skills: doc.get("skills")?.as_u64()?,
                last_skill,
                thinking_at,
            },
        })
    }
}

/// Cache file for a session: its id when that is filename-safe, else a hash of
/// the transcript path.
fn cache_name(session_id: Option<&str>, path: &Path) -> String {
    match session_id {
        Some(id)
            if !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            format!("tr-{id}.json")
        }
        _ => format!("tr-{}.json", cache::path_key(path)),
    }
}

/// The counters for the transcript at `path`. `None` when it is not a regular
/// file (the segments then drop). `now` stands in for entries without a timestamp.
pub fn calls(
    path: &Path,
    session_id: Option<&str>,
    cache_dir: Option<&Path>,
    now: f64,
) -> Option<Calls> {
    let meta = fs::metadata(path).ok().filter(fs::Metadata::is_file)?;
    let size = meta.len();
    let name = cache_name(session_id, path);
    let path_text = path.to_string_lossy();

    let cached = cache_dir
        .and_then(|d| cache::read(d, &name))
        .and_then(|doc| State::from_json(&doc))
        .filter(|s| {
            s.path == path_text && s.dev == meta.dev() && s.ino == meta.ino() && s.offset <= size
        });
    if let Some(state) = &cached
        && state.offset == size
    {
        return Some(state.calls.clone());
    }

    let mut file = File::open(path).ok()?;
    // Resume where the last scan stopped, or take the tail on a first look
    // (and after a rewrite, or a gap too big to be worth reading). A file
    // rewritten in place and regrown past the offset is told by the byte
    // before it: the scans stop just past a newline.
    let resume = cached
        .filter(|s| size - s.offset <= MAX_CATCH_UP_BYTES && follows_newline(&mut file, s.offset));
    let (start, mut calls, from_tail) = match resume {
        Some(s) => (s.offset, s.calls, false),
        None => (size.saturating_sub(TAIL_BYTES), Calls::default(), true),
    };
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(start)).ok()?;
    file.take(size - start).read_to_end(&mut buf).ok()?;

    // A tail read starts mid-file: drop the partial line it lands in. A
    // resumed read starts just past a newline.
    let skipped = if from_tail && start > 0 {
        memchr::memchr(b'\n', &buf).map_or(buf.len(), |i| i + 1)
    } else {
        0
    };
    let body = &buf[skipped..];
    // Only whole lines count; a half-written last line waits for the next render.
    let used = memchr::memrchr(b'\n', body).map_or(0, |i| i + 1);
    let mut begin = 0;
    for end in memchr::memchr_iter(b'\n', &body[..used]) {
        scan(&body[begin..end], &mut calls, now);
        begin = end + 1;
    }

    if let Some(dir) = cache_dir {
        let state = State {
            path: path_text.into_owned(),
            dev: meta.dev(),
            ino: meta.ino(),
            offset: start + (skipped + used) as u64,
            calls: calls.clone(),
        };
        let _ = cache::write(dir, &name, &state.to_json());
    }
    Some(calls)
}

/// Whether the byte before `offset` is a newline (trivially so at 0).
fn follows_newline(file: &mut File, offset: u64) -> bool {
    let Some(before) = offset.checked_sub(1) else {
        return true;
    };
    let mut byte = [0u8];
    file.seek(SeekFrom::Start(before)).is_ok()
        && file.read_exact(&mut byte).is_ok()
        && byte[0] == b'\n'
}

fn scan(line: &[u8], calls: &mut Calls, now: f64) {
    let has_tool = memmem::find(line, b"\"tool_use\"").is_some();
    let has_thinking = memmem::find(line, b"\"thinking\"").is_some()
        || memmem::find(line, b"\"reasoning\"").is_some();
    if !has_tool && !has_thinking {
        return;
    }
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let Some(blocks) = entry.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    // The HUD stamps an entry without a timestamp with the time it was read.
    let stamp = match entry.get("timestamp").filter(|t| !t.is_null()) {
        Some(t) => t.as_str().and_then(parse_timestamp),
        None => Some(now),
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking" | "reasoning") => calls.thinking_at = stamp,
            Some("tool_use") => {
                let named = |key: &str| {
                    block
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                };
                let (Some(_), Some(tool)) = (named("id"), named("name")) else {
                    continue;
                };
                calls.tools += 1;
                match tool {
                    "Task" | "proxy_Task" | "Agent" | "proxy_Agent" => calls.agents += 1,
                    "Skill" | "proxy_Skill" => {
                        calls.skills += 1;
                        let input = block.get("input");
                        let skill = input.and_then(|i| i.get("skill")).and_then(Value::as_str);
                        if let Some(skill) = skill.filter(|s| !s.is_empty()) {
                            let args = input
                                .and_then(|i| i.get("args"))
                                .and_then(Value::as_str)
                                .filter(|a| !a.is_empty());
                            calls.last_skill = Some(Skill {
                                name: skill.to_string(),
                                args: args.map(|a| a.chars().take(ARGS_KEPT_CHARS).collect()),
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// `2025-06-15T12:57:01.123Z` (or with a `+hh:mm` offset) as unix seconds.
fn parse_timestamp(text: &str) -> Option<f64> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let digits = b.get(from..from + len)?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut at = 19;
    let mut fraction = 0.0;
    if b.get(at) == Some(&b'.') {
        let digits = b[at + 1..]
            .iter()
            .take_while(|d| d.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        fraction = format!("0.{}", &text[at + 1..at + 1 + digits])
            .parse()
            .ok()?;
        at += 1 + digits;
    }
    let offset = match &b[at..] {
        b"Z" | b"z" | b"" => 0,
        [sign @ (b'+' | b'-'), ..] if b.len() == at + 6 && b[at + 3] == b':' => {
            let secs = num(at + 1, 2)? * 3600 + num(at + 4, 2)? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64 + fraction)
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::testing::Tmp;
    use std::io::Write;

    fn tool(id: &str, name: &str) -> String {
        format!(r#"{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}"#)
    }

    fn entry(ts: &str, blocks: &[String]) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{}]}}}}"#,
            blocks.join(",")
        )
    }

    fn lines(entries: &[String]) -> String {
        entries.iter().map(|e| format!("{e}\n")).collect()
    }

    fn scanned(text: &str) -> Calls {
        let mut c = Calls::default();
        for line in text.lines() {
            scan(line.as_bytes(), &mut c, 0.0);
        }
        c
    }

    const TS: &str = "2025-06-15T12:57:01.500Z";

    #[test]
    fn counts_tool_agent_and_skill_blocks_like_the_hud() {
        let text = lines(&[
            entry(TS, &[tool("1", "Bash"), tool("2", "Read")]),
            entry(TS, &[tool("3", "Task"), tool("4", "Agent")]),
            entry(
                TS,
                &[
                    tool("5", "proxy_Task"),
                    tool("6", "proxy_Agent"),
                    tool("7", "Skill"),
                ],
            ),
            entry(TS, &[tool("8", "proxy_Skill"), tool("9", "TodoWrite")]),
        ]);
        let c = scanned(&text);
        assert_eq!((c.tools, c.agents, c.skills), (9, 4, 2));
    }

    #[test]
    fn a_tool_use_needs_an_id_and_a_name() {
        let text = lines(&[entry(
            TS,
            &[
                r#"{"type":"tool_use","name":"Bash"}"#.into(),
                r#"{"type":"tool_use","id":"x"}"#.into(),
                r#"{"type":"tool_use","id":"","name":"Bash"}"#.into(),
                r#"{"type":"tool_use","id":"x","name":5}"#.into(),
                tool("ok", "Bash"),
            ],
        )]);
        assert_eq!(scanned(&text).tools, 1);
    }

    #[test]
    fn only_assistant_content_arrays_count() {
        let text = [
            // String content, no content, content of the wrong type: ignored.
            r#"{"type":"user","message":{"content":"tool_use \"tool_use\" talk"}}"#,
            r#"{"type":"user","message":{}}"#,
            r#"{"type":"user","message":{"content":{"type":"tool_use","id":"1","name":"Bash"}}}"#,
            // A tool_result quoting the words is not a call.
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":"1","content":"\"type\":\"tool_use\""}]}}"#,
            "not json at all tool_use",
            "",
        ]
        .join("\n");
        assert_eq!(scanned(&text), Calls::default());
    }

    #[test]
    fn last_skill_keeps_name_and_args() {
        let skill = |s: &str, args: Option<&str>| {
            let args = args.map_or(String::new(), |a| format!(r#","args":"{a}""#));
            format!(
                r#"{{"type":"tool_use","id":"s","name":"Skill","input":{{"skill":"{s}"{args}}}}}"#
            )
        };
        let c = scanned(&lines(&[
            entry(TS, &[skill("first", Some("a"))]),
            entry(
                TS,
                &[skill("oh-my-claudecode:plan", Some("Detailed handoff"))],
            ),
        ]));
        assert_eq!(c.skills, 2);
        assert_eq!(
            c.last_skill,
            Some(Skill {
                name: "oh-my-claudecode:plan".into(),
                args: Some("Detailed handoff".into())
            })
        );
        // A later Skill call without a usable name counts but does not replace it.
        let c = scanned(&lines(&[
            entry(TS, &[skill("keep", None)]),
            entry(
                TS,
                &[r#"{"type":"tool_use","id":"s","name":"Skill","input":{}}"#.into()],
            ),
            entry(
                TS,
                &[r#"{"type":"tool_use","id":"s","name":"Skill","input":{"skill":7}}"#.into()],
            ),
        ]));
        assert_eq!(c.skills, 3);
        assert_eq!(
            c.last_skill,
            Some(Skill {
                name: "keep".into(),
                args: None
            })
        );
        // Non-string args are not shown.
        let c = scanned(&lines(&[entry(
            TS,
            &[r#"{"type":"tool_use","id":"s","name":"Skill","input":{"skill":"x","args":{"a":1}}}"#.into()],
        )]));
        assert_eq!(
            c.last_skill,
            Some(Skill {
                name: "x".into(),
                args: None
            })
        );
    }

    #[test]
    fn thinking_tracks_the_newest_block_and_its_timestamp() {
        let think = |ty: &str| format!(r#"{{"type":"{ty}","thinking":"..."}}"#);
        let c = scanned(&lines(&[
            entry("2025-06-15T12:00:00Z", &[think("thinking")]),
            entry("2025-06-15T12:30:00Z", &[tool("1", "Bash")]),
        ]));
        assert_eq!(
            c.thinking_at,
            Some(parse_timestamp("2025-06-15T12:00:00Z").unwrap())
        );
        let c = scanned(&lines(&[
            entry("2025-06-15T12:00:00Z", &[think("thinking")]),
            entry("2025-06-15T12:30:00.250Z", &[think("reasoning")]),
        ]));
        assert_eq!(
            c.thinking_at,
            Some(parse_timestamp("2025-06-15T12:30:00.250Z").unwrap())
        );
        // An unparsable timestamp replaces an older good one, as in the HUD.
        let c = scanned(&lines(&[
            entry("2025-06-15T12:00:00Z", &[think("thinking")]),
            entry("yesterday", &[think("thinking")]),
        ]));
        assert_eq!(c.thinking_at, None);
    }

    #[test]
    fn missing_timestamp_means_now() {
        let mut c = Calls::default();
        let line = r#"{"message":{"content":[{"type":"thinking","thinking":"x"}]}}"#;
        scan(line.as_bytes(), &mut c, 1234.5);
        assert_eq!(c.thinking_at, Some(1234.5));
    }

    #[test]
    fn timestamps_parse_to_unix_seconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_timestamp("2000-03-01T00:00:00Z"), Some(951_868_800.0));
        assert_eq!(
            parse_timestamp("2025-06-15T12:57:01.500Z"),
            Some(1_749_992_221.5)
        );
        assert_eq!(
            parse_timestamp("2025-06-15T14:57:01+02:00"),
            Some(1_749_992_221.0)
        );
        assert_eq!(
            parse_timestamp("2025-06-15T10:27:01-02:30"),
            Some(1_749_992_221.0)
        );
        assert_eq!(
            parse_timestamp("2024-02-29T23:59:59Z"),
            Some(1_709_251_199.0)
        );
        for bad in [
            "",
            "2025-06-15",
            "2025-13-06T00:00:00Z",
            "2025-06-15T25:00:00Z",
            "2025-06-15T00:00:00.Z",
            "2025-06-15T00:00:00+0200",
            "garbage garbage garbage",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad:?}");
        }
    }

    // ---- the incremental reader ----

    struct Rig {
        t: Tmp,
    }

    impl Rig {
        fn new(name: &str) -> Rig {
            Rig { t: Tmp::new(name) }
        }

        fn transcript(&self) -> std::path::PathBuf {
            self.t.join("session.jsonl")
        }

        fn append(&self, text: &str) {
            let mut f = File::options()
                .create(true)
                .append(true)
                .open(self.transcript())
                .unwrap();
            f.write_all(text.as_bytes()).unwrap();
        }

        fn read(&self) -> Option<Calls> {
            calls(
                &self.transcript(),
                Some("sess-1"),
                Some(&self.t.join("cache")),
                0.0,
            )
        }

        fn cached_offset(&self) -> u64 {
            let doc = cache::read(&self.t.join("cache"), "tr-sess-1.json").unwrap();
            State::from_json(&doc).unwrap().offset
        }
    }

    #[test]
    fn reads_only_what_was_appended() {
        let rig = Rig::new("tr-incremental");
        rig.append(&lines(&[entry(
            TS,
            &[tool("1", "Bash"), tool("2", "Task")],
        )]));
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents), (2, 1));
        let size1 = fs::metadata(rig.transcript()).unwrap().len();
        assert_eq!(rig.cached_offset(), size1);

        // Corrupt what was already consumed (all but its last newline, which
        // marks where the scan stopped): a re-read from 0 would now find
        // nothing, so unchanged counts prove the old bytes are not read again.
        let mut bytes = fs::read(rig.transcript()).unwrap();
        bytes.fill(b'x');
        *bytes.last_mut().unwrap() = b'\n';
        fs::write(rig.transcript(), &bytes).unwrap();
        assert_eq!(
            rig.read(),
            Some(c.clone()),
            "no growth, answered from the cache"
        );
        rig.append(&lines(&[entry(TS, &[tool("3", "Skill")])]));
        let c2 = rig.read().unwrap();
        assert_eq!((c2.tools, c2.agents, c2.skills), (3, 1, 1));
    }

    #[test]
    fn a_half_written_line_waits_for_its_newline() {
        let rig = Rig::new("tr-partial");
        let whole = entry(TS, &[tool("1", "Bash")]);
        let (head, tail) = whole.split_at(whole.len() / 2);
        rig.append(&format!("{whole}\n{head}"));
        assert_eq!(rig.read().unwrap().tools, 1);
        assert_eq!(
            rig.cached_offset(),
            (whole.len() + 1) as u64,
            "stops before the partial line"
        );
        rig.append(&format!("{tail}\n"));
        assert_eq!(rig.read().unwrap().tools, 2);
    }

    #[test]
    fn empty_crlf_and_long_lines_keep_calls_and_offsets() {
        let rig = Rig::new("tr-line-boundaries");
        let ignored = format!(
            r#"{{"message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(8192)
        );
        let first = entry(TS, &[tool("1", "Bash"), tool("2", "Agent")]);
        let last = entry(TS, &[tool("3", "Skill")]);
        rig.append(&format!(
            "\n\r\n{ignored}\r\ninvalid \"tool_use\"\n{first}\r\n\n{last}"
        ));
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents, c.skills), (2, 1, 0));
        assert_eq!(
            rig.cached_offset(),
            fs::metadata(rig.transcript()).unwrap().len() - last.len() as u64
        );
        rig.append("\r\n\n");
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents, c.skills), (3, 1, 1));
        assert_eq!(
            rig.cached_offset(),
            fs::metadata(rig.transcript()).unwrap().len()
        );
    }

    #[test]
    fn first_look_reads_only_the_tail_window() {
        let rig = Rig::new("tr-tail");
        let old = entry(TS, &[tool("old", "Bash")]);
        let filler = format!("{{\"pad\":\"{}\"}}\n", "p".repeat(1000));
        // Old calls first, then well over the window of filler, then two new calls.
        rig.append(&format!("{old}\n"));
        for _ in 0..(TAIL_BYTES as usize / filler.len() + 10) {
            rig.append(&filler);
        }
        rig.append(&lines(&[
            entry(TS, &[tool("n1", "Bash")]),
            entry(TS, &[tool("n2", "Task")]),
        ]));
        let c = rig.read().unwrap();
        assert_eq!(
            (c.tools, c.agents),
            (2, 1),
            "the call before the window is not counted"
        );
    }

    #[test]
    fn the_partial_first_line_of_a_tail_read_is_dropped() {
        let rig = Rig::new("tr-tail-partial");
        let call = entry(TS, &[tool("n", "Bash")]);
        // Put a call line so the window start lands inside it: it must not be
        // half-parsed. Pad so the file is just over the window.
        let pad_len = TAIL_BYTES as usize - call.len() / 2;
        rig.append(&format!("{call}\n"));
        rig.append(&format!("{{\"pad\":\"{}\"}}\n", "p".repeat(pad_len)));
        rig.append(&lines(&[entry(TS, &[tool("last", "Bash")])]));
        assert_eq!(rig.read().unwrap().tools, 1);
    }

    #[test]
    fn shrunk_replaced_or_other_path_starts_over() {
        let rig = Rig::new("tr-reset");
        rig.append(&lines(&[entry(
            TS,
            &[tool("1", "Bash"), tool("2", "Bash"), tool("3", "Bash")],
        )]));
        assert_eq!(rig.read().unwrap().tools, 3);
        // Rewritten shorter (offset beyond the new size).
        fs::write(rig.transcript(), lines(&[entry(TS, &[tool("1", "Bash")])])).unwrap();
        assert_eq!(rig.read().unwrap().tools, 1);
        // Replaced by a different file of at least the old size: new inode.
        let bigger = lines(&[entry(
            TS,
            &[
                tool("a", "Task"),
                tool("b", "Task"),
                tool("c", "Task"),
                tool("d", "Task"),
            ],
        )]);
        let other = rig.t.join("other.jsonl");
        fs::write(&other, &bigger).unwrap();
        fs::rename(&other, rig.transcript()).unwrap();
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents), (4, 4));
        // Same session id, different transcript path.
        let second = rig.t.join("second.jsonl");
        fs::write(&second, lines(&[entry(TS, &[tool("z", "Skill")])])).unwrap();
        let c = calls(&second, Some("sess-1"), Some(&rig.t.join("cache")), 0.0).unwrap();
        assert_eq!((c.tools, c.skills), (1, 1));
    }

    #[test]
    fn skill_args_are_kept_only_as_far_as_the_line_shows_them() {
        let rig = Rig::new("tr-args");
        let args = format!("{}TAIL-SECRET", "\u{e9}".repeat(150));
        rig.append(&lines(&[entry(
            TS,
            &[format!(
                r#"{{"type":"tool_use","id":"s","name":"Skill","input":{{"skill":"x","args":"{args}"}}}}"#
            )],
        )]));
        let kept = rig.read().unwrap().last_skill.unwrap().args.unwrap();
        assert_eq!(kept.chars().count(), 64);
        assert!(args.starts_with(&kept));
        let stored = fs::read_to_string(rig.t.join("cache/tr-sess-1.json")).unwrap();
        assert!(!stored.contains("TAIL-SECRET"), "{stored}");
        // Resumed from the cache, the same.
        rig.append(&lines(&[entry(TS, &[tool("2", "Bash")])]));
        assert_eq!(rig.read().unwrap().last_skill.unwrap().args.unwrap(), kept);
    }

    #[test]
    fn a_transcript_rewritten_in_place_and_regrown_is_counted_afresh() {
        let rig = Rig::new("tr-rewrite");
        rig.append(&lines(&[
            entry(TS, &[tool("1", "Bash")]),
            entry(TS, &[tool("2", "Bash")]),
        ]));
        assert_eq!(rig.read().unwrap().tools, 2);
        let seen = fs::metadata(rig.transcript()).unwrap().len();
        // The same file (same inode) rewritten with other content that grew
        // past the cached offset, the old offset landing inside a line.
        let rewritten = lines(&[
            entry(TS, &[tool("a", "Task"), tool("b", "Task")]),
            entry(TS, &[tool("c", "Bash")]),
            entry(TS, &[tool("d", "Bash"), tool("e", "Skill")]),
        ]);
        assert!(rewritten.len() as u64 > seen);
        assert_ne!(rewritten.as_bytes()[seen as usize - 1], b'\n');
        fs::write(rig.transcript(), &rewritten).unwrap();
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents, c.skills), (5, 2, 1));
    }

    #[test]
    fn unusable_cache_is_recomputed() {
        let rig = Rig::new("tr-corrupt");
        rig.append(&lines(&[entry(
            TS,
            &[tool("1", "Bash"), tool("2", "Bash")],
        )]));
        rig.read();
        let cache_file = rig.t.join("cache/tr-sess-1.json");
        let good: Value = serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
        let broken = [
            "{nope".to_string(),
            "".to_string(),
            "[]".to_string(),
            {
                let mut d = good.clone();
                d["off"] = json!("far");
                d.to_string()
            },
            {
                let mut d = good.clone();
                d["tools"] = json!(-4);
                d.to_string()
            },
            {
                let mut d = good.clone();
                d["skill"] = json!({"n": 1});
                d.to_string()
            },
            {
                let mut d = good.clone();
                d["v"] = json!(2);
                d.to_string()
            },
        ];
        for text in broken {
            fs::write(&cache_file, &text).unwrap();
            assert_eq!(rig.read().unwrap().tools, 2, "{text}");
            let healed: Value = serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
            assert!(
                State::from_json(&healed).is_some(),
                "{text} was replaced by a good entry"
            );
        }
    }

    #[test]
    fn a_cache_that_claims_more_than_the_file_has_is_not_believed() {
        let rig = Rig::new("tr-liar");
        rig.append(&lines(&[entry(TS, &[tool("1", "Bash")])]));
        rig.read();
        let cache_file = rig.t.join("cache/tr-sess-1.json");
        let mut doc: Value = serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
        doc["off"] = json!(10_000_000);
        doc["tools"] = json!(999);
        fs::write(&cache_file, doc.to_string()).unwrap();
        assert_eq!(rig.read().unwrap().tools, 1);
    }

    #[test]
    fn missing_or_special_transcript_is_none() {
        let rig = Rig::new("tr-missing");
        assert_eq!(rig.read(), None);
        fs::create_dir_all(rig.transcript()).unwrap();
        assert_eq!(rig.read(), None, "a directory");
    }

    #[test]
    fn no_cache_dir_still_counts() {
        let rig = Rig::new("tr-nocache");
        rig.append(&lines(&[entry(TS, &[tool("1", "Bash")])]));
        let c = calls(&rig.transcript(), None, None, 0.0).unwrap();
        assert_eq!(c.tools, 1);
        assert!(!rig.t.join("cache").exists());
    }

    #[test]
    fn session_ids_that_are_not_filename_safe_fall_back_to_a_path_hash() {
        let p = Path::new("/a/b.jsonl");
        assert_eq!(cache_name(Some("abc-123_X"), p), "tr-abc-123_X.json");
        for bad in ["", "../../etc/passwd", "a/b", "a b", &"x".repeat(65)] {
            let name = cache_name(Some(bad), p);
            assert_eq!(name, cache_name(None, p), "{bad:?}");
            assert!(!name[3..name.len() - 5].contains(['/', ' ', '.']));
        }
    }

    #[test]
    fn concurrent_readers_of_one_session_agree() {
        let rig = std::sync::Arc::new(Rig::new("tr-concurrent"));
        rig.append(&lines(&[entry(
            TS,
            &[tool("1", "Bash"), tool("2", "Task")],
        )]));
        let handles: Vec<_> = (0..6)
            .map(|n| {
                let rig = rig.clone();
                std::thread::spawn(move || {
                    for i in 0..20 {
                        if n == 0 {
                            rig.append(&lines(&[entry(TS, &[tool(&format!("a{i}"), "Bash")])]));
                        }
                        let c = rig.read().unwrap();
                        assert!(c.tools >= 2);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Whatever the interleaving, the settled answer is the exact total.
        let c = rig.read().unwrap();
        assert_eq!((c.tools, c.agents), (22, 1));
        assert_eq!(rig.read().unwrap(), c);
    }
}
