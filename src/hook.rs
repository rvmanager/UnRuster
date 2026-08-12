//! `gate --hook` — the pre-write gate, wired to a Claude Code `PreToolUse` event.
//!
//! # Why the tool reads the event itself
//!
//! The obvious shape for this is a shell script that pulls `file_path` and the
//! new text out of the event with `jq` and calls `unruster gate`. That makes the
//! gate depend on a program the user may not have, in a script the user has to
//! keep in step with this tool's flags — two failure modes that both present as
//! "the hook silently stopped working", which is the worst way for a gate to
//! fail. Reading the event here makes the hook configuration one word:
//!
//! ```json
//! { "hooks": { "PreToolUse": [ {
//!     "matcher": "Write|Edit",
//!     "hooks": [ { "type": "command", "command": "unruster gate --hook" } ]
//! } ] } }
//! ```
//!
//! # The contract this implements
//!
//! stdin carries one JSON object with `tool_name` and `tool_input`. A non-zero
//! exit blocks the call and hands stderr back to the model; exit 0 lets it
//! through. That asymmetry is why the default mode is **warn-once** rather than
//! block (see [`crate::gate`]): the only way to *tell* the model something is to
//! stop it, so the gate stops it exactly once per distinct proposal and then
//! gets out of the way.
//!
//! # Modes (`UNRUSTER_GATE`)
//!
//! | value       | behaviour                                              |
//! |-------------|--------------------------------------------------------|
//! | `warn-once` | default — block the first time a proposal collides, allow the retry |
//! | `block`     | block every time; for a repo that wants the rule absolute |
//! | `warn`      | never block; findings go to the transcript only        |
//! | `off`       | do nothing                                             |
//!
//! Every failure here — unreadable stdin, malformed JSON, an unparseable
//! fragment, a missing cache — exits 0. A gate that blocks an edit because it
//! could not read its own input is worse than no gate.

use std::io::Read;

/// What the gate should do about a colliding proposal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    WarnOnce,
    Block,
    Warn,
    Off,
}

impl Mode {
    pub fn from_env() -> Mode {
        match std::env::var("UNRUSTER_GATE").as_deref().map(str::trim) {
            Ok("block") => Mode::Block,
            Ok("warn") => Mode::Warn,
            Ok("off") => Mode::Off,
            // Anything else, including unset and a typo: the default. A typo'd
            // mode must not silently disable a gate somebody meant to enable.
            _ => Mode::WarnOnce,
        }
    }
}

/// The fields of a `PreToolUse` event this command uses.
#[derive(Debug, Default)]
pub struct Event {
    pub tool_name: String,
    pub file_path: String,
    /// `Write`'s whole-file content, or `Edit`'s replacement text.
    pub text: String,
}

/// Read the event from stdin. `None` when there is nothing usable — which is
/// not an error, it is a hook that has nothing to say.
pub fn read_event() -> Option<Event> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    parse_event(&raw)
}

pub fn parse_event(raw: &str) -> Option<Event> {
    let tool_name = json_string(raw, "tool_name").unwrap_or_default();
    let file_path = json_string(raw, "file_path")?;
    // `content` is Write's key, `new_string` is Edit's. A `MultiEdit`-shaped
    // payload carries several of the latter; the first is enough to decide
    // whether a *declaration* is being introduced, which is the only question
    // this gate asks.
    let text = json_string(raw, "content")
        .or_else(|| json_string(raw, "new_string"))
        .unwrap_or_default();
    Some(Event {
        tool_name,
        file_path,
        text,
    })
}

/// The string value of the first `"key":` in `raw`, unescaped.
///
/// A deliberately small reader rather than a JSON dependency. The tool has four
/// dependencies and this needs three fields out of a document whose shape is
/// fixed by another program; a parser for the whole grammar would be more code
/// than the feature. It is string-literal aware, so a `"content"` appearing
/// *inside* a value cannot be mistaken for the key — which matters here more
/// than usual, since the values are Rust source and may well contain the word.
fn json_string(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            // At the start of a string: is it our key?
            if raw[i..].starts_with(&needle) {
                let after = &raw[i + needle.len()..];
                let rest = after.trim_start();
                if let Some(v) = rest.strip_prefix(':') {
                    let v = v.trim_start();
                    if v.starts_with('"') {
                        return read_json_string(v);
                    }
                }
            }
            in_str = true;
        }
        i += 1;
    }
    None
}

/// Decode the JSON string literal at the head of `s`.
fn read_json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = s.strip_prefix('"')?.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                '/' => out.push('/'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| it.next()).collect();
                    let n = u32::from_str_radix(&hex, 16).ok()?;
                    // Lone surrogates are legal in JSON and not in Rust chars;
                    // the replacement keeps the rest of the string readable
                    // rather than discarding the whole event.
                    out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Escape a string for the JSON this command writes back.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// The `systemMessage` form: shown in the transcript, does not block.
pub fn advisory_json(text: &str) -> String {
    format!("{{\"systemMessage\": \"{}\"}}", esc(text))
}

// ──────────────────────────────────────────────────────────────────────────
// warn-once

/// Has this exact proposal already been reported? Records it if not.
///
/// The acknowledgment store is the pre-hoc analogue of a waiver: post-hoc, a
/// judged-intentional site gets `// unruster: ok(…)` written beside it; here
/// there is no code yet to write beside, so the retry *is* the acknowledgment.
/// Keyed on the finding text rather than on the file, so changing the proposal
/// in response to the warning produces a new key and gets a fresh answer —
/// which is the behaviour that makes this a gate rather than a speed bump.
///
/// Entries expire, so a collision re-reported an hour later is worth saying
/// again. Any IO failure answers "not seen before", because a store that cannot
/// be read must not silently turn the gate off.
pub fn seen_before(root: &std::path::Path, key_text: &str) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let Some(dir) = crate::cache::cache_root().map(|d| {
        d.join(crate::cache::slug_for(root)).join("ack")
    }) else {
        return false;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let key = crate::cache::Cache::key(key_text.as_bytes());
    let path = dir.join(key);
    if let Ok(m) = std::fs::metadata(&path) {
        if let Ok(age) = m.modified().and_then(|t| t.elapsed().map_err(std::io::Error::other)) {
            if age < TTL {
                return true;
            }
        }
    }
    let _ = std::fs::write(&path, "");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITE_EVENT: &str = r#"{
      "session_id": "abc",
      "tool_name": "Write",
      "tool_input": {
        "file_path": "/tmp/x/src/ids.rs",
        "content": "pub struct AccountId(u64);\n"
      }
    }"#;

    #[test]
    fn a_write_event_yields_its_path_and_content() {
        let e = parse_event(WRITE_EVENT).expect("parses");
        assert_eq!(e.tool_name, "Write");
        assert_eq!(e.file_path, "/tmp/x/src/ids.rs");
        assert_eq!(e.text, "pub struct AccountId(u64);\n");
    }

    #[test]
    fn an_edit_event_uses_the_replacement_text() {
        let raw = r#"{"tool_name":"Edit","tool_input":{"file_path":"a.rs",
                     "old_string":"x","new_string":"pub struct B(u8);"}}"#;
        let e = parse_event(raw).expect("parses");
        assert_eq!(e.text, "pub struct B(u8);");
    }

    /// The failure mode a naive `find the key` reader has: the payload is Rust
    /// source, and Rust source talks about `content` all the time.
    #[test]
    fn a_key_name_inside_a_value_is_not_mistaken_for_the_key() {
        let raw = r#"{"tool_input":{"file_path":"a.rs",
                     "content":"fn f() { let \"file_path\" = 1; }"}}"#;
        let e = parse_event(raw).expect("parses");
        assert_eq!(e.file_path, "a.rs");
        assert!(e.text.contains("file_path"));
    }

    #[test]
    fn escapes_and_unicode_survive_the_round_trip() {
        let raw = r#"{"tool_input":{"file_path":"a.rs","content":"a\tb\n\"c\"\\d\u00e9"}}"#;
        let e = parse_event(raw).expect("parses");
        assert_eq!(e.text, "a\tb\n\"c\"\\dé");
    }

    #[test]
    fn an_event_without_a_path_is_not_an_event_for_this_hook() {
        assert!(parse_event(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#).is_none());
    }

    #[test]
    fn malformed_json_is_declined_rather_than_guessed_at() {
        assert!(parse_event("not json at all").is_none());
        assert!(parse_event("").is_none());
    }

    #[test]
    fn an_unknown_mode_falls_back_to_the_default_rather_than_off() {
        std::env::set_var("UNRUSTER_GATE", "definitely-not-a-mode");
        assert_eq!(Mode::from_env(), Mode::WarnOnce);
        std::env::remove_var("UNRUSTER_GATE");
    }

    #[test]
    fn the_advisory_payload_escapes_its_message() {
        let j = advisory_json("said \"no\"\nagain");
        assert!(j.contains("\\\"no\\\""), "{j}");
        assert!(!j.contains('\n'), "{j}");
    }
}
