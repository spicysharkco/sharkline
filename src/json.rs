//! Lenient reading of the render JSON.
//!
//! serde_json is stricter than the `JSON.parse` the Node HUD used: it rejects a
//! lone surrogate escape such as `"\ud800"` and a number that overflows, such
//! as `1e999`. Either one anywhere in the payload would blank every segment, so
//! a first failure is retried on a repaired copy.

use serde_json::Value;

/// First JSON value in `buf`, or `Null`. Bytes after the value are ignored and
/// invalid UTF-8 becomes U+FFFD, as it did in the HUD.
pub fn parse(buf: &[u8]) -> Value {
    let text = String::from_utf8_lossy(buf);
    first_value(&text)
        .or_else(|| first_value(&repair(&text)))
        .unwrap_or(Value::Null)
}

fn first_value(text: &str) -> Option<Value> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// Rewrite what serde_json refuses and `JSON.parse` takes: a surrogate escape
/// that is not half of a valid pair becomes `�`; a number that overflows
/// becomes the largest finite one of the same sign (every reader clamps).
fn repair(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut in_string = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == b'\\' && b.get(i + 1) == Some(&b'u') {
                match unit(b, i) {
                    Some(0xD800..=0xDBFF) if unit(b, i + 6).is_some_and(is_low) => {
                        out.push_str(&text[i..i + 12]);
                        i += 12;
                    }
                    Some(0xD800..=0xDFFF) => {
                        out.push_str("\\ufffd");
                        i += 6;
                    }
                    Some(_) => {
                        out.push_str(&text[i..i + 6]);
                        i += 6;
                    }
                    None => {
                        out.push_str("\\u");
                        i += 2;
                    }
                }
                continue;
            }
            if c == b'\\' && i + 1 < b.len() {
                // Keep the escaped byte whole so an escaped quote does not end the string.
                let n = text[i + 1..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&text[i..i + 1 + n]);
                i += 1 + n;
                continue;
            }
            in_string = c != b'"';
        } else if c == b'"' {
            in_string = true;
        } else if c == b'-' || c.is_ascii_digit() {
            let end = i + b[i..]
                .iter()
                .take_while(|c| matches!(c, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E'))
                .count();
            let token = &text[i..end];
            match token.parse::<f64>() {
                Ok(v) if v.is_infinite() => {
                    out.push_str(if v < 0.0 { "-1e308" } else { "1e308" });
                }
                _ => out.push_str(token),
            }
            i = end;
            continue;
        }
        let n = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[i..i + n]);
        i += n;
    }
    out
}

/// The UTF-16 code unit of the `\uXXXX` escape starting at `at`.
fn unit(b: &[u8], at: usize) -> Option<u16> {
    let hex = b.get(at..at + 6)?;
    if hex[0] != b'\\' || hex[1] != b'u' {
        return None;
    }
    let digits = std::str::from_utf8(&hex[2..]).ok()?;
    if !digits.bytes().all(|d| d.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

fn is_low(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json_is_untouched() {
        assert_eq!(
            parse(br#"{"a":[1,2.5,"x"],"b":null}"#),
            json!({"a": [1, 2.5, "x"], "b": null})
        );
    }

    #[test]
    fn first_value_only_and_garbage_is_null() {
        assert_eq!(parse(b"{\"a\":1}\ntrailing"), json!({"a": 1}));
        for junk in [&b""[..], b"  ", b"nope", b"{\"a\":", b"\xff\xfe\x00"] {
            assert_eq!(parse(junk), Value::Null, "{junk:?}");
        }
    }

    #[test]
    fn lone_surrogate_escape_keeps_the_rest() {
        let got = parse(br#"{"model":{"display_name":"Op\ud800us"},"n":7}"#);
        assert_eq!(got["model"]["display_name"], "Op\u{fffd}us");
        assert_eq!(got["n"], 7);
        // A lone low surrogate, and one at the very end of a string.
        let got = parse(br#"{"a":"\udc00x","b":"y\ud83d"}"#);
        assert_eq!(got["a"], "\u{fffd}x");
        assert_eq!(got["b"], "y\u{fffd}");
    }

    #[test]
    fn valid_surrogate_pair_and_other_escapes_survive_repair() {
        // The 1e999 forces the repair path; the rest must come through intact.
        let got = parse(br#"{"e":"\ud83d\ude00 \u00e9 \" \\ \n","big":1e999}"#);
        assert_eq!(got["e"], "\u{1f600} \u{e9} \" \\ \n");
        assert_eq!(got["big"].as_f64(), Some(1e308));
    }

    #[test]
    fn overflowing_numbers_clamp_to_finite() {
        let got = parse(br#"{"used_percentage":1e999,"low":-1e999,"ok":1.5e3,"s":"1e999"}"#);
        assert_eq!(got["used_percentage"].as_f64(), Some(1e308));
        assert_eq!(got["low"].as_f64(), Some(-1e308));
        assert_eq!(got["ok"].as_f64(), Some(1500.0));
        assert_eq!(got["s"], "1e999", "a string is not a number");
    }

    #[test]
    fn escaped_quote_does_not_confuse_the_repair() {
        let got = parse(br#"{"a":"say \"1e999\" \ud800","b":2}"#);
        assert_eq!(got["a"], "say \"1e999\" \u{fffd}");
        assert_eq!(got["b"], 2);
    }

    #[test]
    fn invalid_utf8_becomes_a_replacement_character() {
        let got = parse(b"{\"a\":\"x\xffy\",\"b\":1}");
        assert_eq!(got["a"], "x\u{fffd}y");
        assert_eq!(got["b"], 1);
    }

    #[test]
    fn multibyte_text_around_a_repair_is_kept() {
        let got = parse("{\"a\":\"h\u{e9}llo \u{1f600}\",\"b\":1e999}".as_bytes());
        assert_eq!(got["a"], "h\u{e9}llo \u{1f600}");
    }
}
