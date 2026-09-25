//! Share-text grammar for per-game macros (see `notes/macro-format.md`).

use std::collections::BTreeMap;
use std::fmt;

pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024;
pub const MAX_MACROS_PER_GAME: usize = 24;
pub const MAX_STEPS_PER_MACRO: usize = 48;
pub const MAX_WAIT_MS: u32 = 2000;
pub const MAX_WAIT_SUM_MS: u32 = 8000;

/// Parsed share document (one or more macros).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDocument {
    pub headers: BTreeMap<String, String>,
    pub macros: Vec<ParsedMacro>,
}

impl MacroDocument {
    pub fn game_header(&self) -> Option<&str> {
        self.headers.get("game").map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMacro {
    pub name: String,
    pub action: String,
    pub meta: BTreeMap<String, String>,
    pub steps: Vec<Step>,
}

/// One step inside an `action:` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Focus,
    Wait(u32),
    Press(KeyChord),
    Type(String),
    KeyDown(KeyChord),
    KeyUp(KeyChord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: Key,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Esc,
    Tab,
    Space,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    Char(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

/// Split `game: <appid>|<name>` on the first `|`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRef {
    pub appid: Option<u32>,
    pub name: String,
}

impl GameRef {
    pub fn parse(value: &str) -> Self {
        let (left, right) = match value.split_once('|') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (value.trim(), ""),
        };
        let appid = if left.is_empty() {
            None
        } else {
            left.parse::<u32>().ok()
        };
        Self {
            appid,
            name: right.to_string(),
        }
    }

    pub fn format(appid: Option<u32>, name: &str) -> String {
        match appid {
            Some(id) => format!("{id}|{name}"),
            None => format!("|{name}"),
        }
    }
}

/// Parse a share document. Rejects oversized / invalid input with a line number when possible.
pub fn parse(text: &str) -> Result<MacroDocument, ParseError> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(ParseError {
            line: None,
            message: format!("document exceeds {MAX_DOCUMENT_BYTES} bytes"),
        });
    }

    let mut headers = BTreeMap::new();
    let mut macros: Vec<ParsedMacro> = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_action: Option<String> = None;
    let mut current_meta: BTreeMap<String, String> = BTreeMap::new();
    let mut seen_names: Vec<String> = Vec::new();

    let flush = |name: Option<String>,
                 action: Option<String>,
                 meta: BTreeMap<String, String>,
                 line: usize,
                 macros: &mut Vec<ParsedMacro>|
     -> Result<(), ParseError> {
        let Some(name) = name else {
            return Ok(());
        };
        let Some(action) = action else {
            return Err(ParseError {
                line: Some(line),
                message: format!("macro `{name}` is missing action:"),
            });
        };
        if name.trim().is_empty() {
            return Err(ParseError {
                line: Some(line),
                message: "name must be non-empty".into(),
            });
        }
        let steps = parse_action(&action).map_err(|message| ParseError {
            line: Some(line),
            message,
        })?;
        if steps.is_empty() {
            return Err(ParseError {
                line: Some(line),
                message: "action must contain at least one step".into(),
            });
        }
        if steps.len() > MAX_STEPS_PER_MACRO {
            return Err(ParseError {
                line: Some(line),
                message: format!("action exceeds {MAX_STEPS_PER_MACRO} steps"),
            });
        }
        let wait_sum: u32 = steps
            .iter()
            .filter_map(|s| match s {
                Step::Wait(ms) => Some(*ms),
                _ => None,
            })
            .try_fold(0u32, |acc, ms| acc.checked_add(ms))
            .ok_or_else(|| ParseError {
                line: Some(line),
                message: "wait sum overflow".into(),
            })?;
        if wait_sum > MAX_WAIT_SUM_MS {
            return Err(ParseError {
                line: Some(line),
                message: format!("wait sum exceeds {MAX_WAIT_SUM_MS}ms"),
            });
        }
        macros.push(ParsedMacro {
            name,
            action,
            meta,
            steps,
        });
        Ok(())
    };

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        let Some((key, value)) = split_kv(line) else {
            return Err(ParseError {
                line: Some(line_no),
                message: format!("expected key: value, got `{line}`"),
            });
        };
        let key_l = key.to_ascii_lowercase();

        if key_l == "name" {
            flush(
                current_name.take(),
                current_action.take(),
                std::mem::take(&mut current_meta),
                line_no,
                &mut macros,
            )?;
            let name = value.trim().to_string();
            if name.is_empty() {
                return Err(ParseError {
                    line: Some(line_no),
                    message: "name must be non-empty".into(),
                });
            }
            let lower = name.to_ascii_lowercase();
            if seen_names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                return Err(ParseError {
                    line: Some(line_no),
                    message: format!("duplicate macro name `{name}`"),
                });
            }
            let _ = lower;
            seen_names.push(name.clone());
            current_name = Some(name);
            current_action = None;
            current_meta = BTreeMap::new();
            continue;
        }

        if current_name.is_none() {
            headers.insert(key_l, value.trim().to_string());
            continue;
        }

        if key_l == "action" {
            if current_action.is_some() {
                return Err(ParseError {
                    line: Some(line_no),
                    message: "macro already has an action:".into(),
                });
            }
            current_action = Some(value.trim().to_string());
            continue;
        }

        current_meta.insert(key_l, value.trim().to_string());
    }

    flush(
        current_name.take(),
        current_action.take(),
        std::mem::take(&mut current_meta),
        text.lines().count().max(1),
        &mut macros,
    )?;

    if macros.is_empty() {
        return Err(ParseError {
            line: None,
            message: "document has no macros".into(),
        });
    }
    if macros.len() > MAX_MACROS_PER_GAME {
        return Err(ParseError {
            line: None,
            message: format!("document exceeds {MAX_MACROS_PER_GAME} macros"),
        });
    }

    Ok(MacroDocument { headers, macros })
}

/// Rebuild share text from headers + macros (export).
pub fn export(
    headers: &BTreeMap<String, String>,
    macros: &[(String, String, &BTreeMap<String, String>)],
) -> String {
    let mut out = String::new();
    // Prefer stable game header first when present.
    if let Some(game) = headers.get("game") {
        out.push_str("game: ");
        out.push_str(game);
        out.push('\n');
    }
    for (k, v) in headers {
        if k == "game" {
            continue;
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push('\n');
    }
    for (name, action, meta) in macros {
        out.push_str("name: ");
        out.push_str(name);
        out.push('\n');
        for (k, v) in *meta {
            if k == "action" || k == "name" {
                continue;
            }
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push('\n');
        }
        out.push_str("action: ");
        out.push_str(action);
        out.push('\n');
    }
    out
}

/// Export a single macro with optional catalog headers.
pub fn export_one(
    headers: &BTreeMap<String, String>,
    name: &str,
    action: &str,
    meta: &BTreeMap<String, String>,
) -> String {
    export(headers, &[(name.to_string(), action.to_string(), meta)])
}

fn strip_comment(line: &str) -> &str {
    // Only treat `#` as a comment when not inside quotes on this line.
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    let k = k.trim();
    if k.is_empty() || k.contains(char::is_whitespace) {
        return None;
    }
    Some((k, v))
}

pub fn parse_action(action: &str) -> Result<Vec<Step>, String> {
    let parts = split_action_chain(action)?;
    let mut steps = Vec::with_capacity(parts.len());
    for part in parts {
        steps.push(parse_step(part)?);
    }
    Ok(steps)
}

fn split_action_chain(action: &str) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let bytes = action.as_bytes();
    let mut i = 0usize;
    let mut in_quotes = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if in_quotes {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_quotes = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_quotes = true;
            i += 1;
            continue;
        }
        // Separator: " > "
        if b == b' ' && i + 2 < bytes.len() && bytes[i + 1] == b'>' && bytes[i + 2] == b' ' {
            let piece = action[start..i].trim();
            if piece.is_empty() {
                return Err("empty step in action chain".into());
            }
            parts.push(piece);
            i += 3;
            start = i;
            continue;
        }
        i += 1;
    }
    if in_quotes {
        return Err("unclosed quote in action".into());
    }
    let last = action[start..].trim();
    if last.is_empty() {
        if parts.is_empty() {
            return Err("empty action".into());
        }
        return Err("trailing separator in action".into());
    }
    parts.push(last);
    Ok(parts)
}

fn parse_step(step: &str) -> Result<Step, String> {
    let step = step.trim();
    if step.eq_ignore_ascii_case("focus") {
        return Ok(Step::Focus);
    }
    if let Some(rest) = strip_verb(step, "wait") {
        let ms: u32 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid wait ms `{rest}`"))?;
        if ms > MAX_WAIT_MS {
            return Err(format!("wait {ms} exceeds {MAX_WAIT_MS}ms"));
        }
        return Ok(Step::Wait(ms));
    }
    if let Some(rest) = strip_verb(step, "press") {
        return Ok(Step::Press(parse_chord(rest.trim())?));
    }
    if let Some(rest) = strip_verb(step, "keydown") {
        return Ok(Step::KeyDown(parse_chord(rest.trim())?));
    }
    if let Some(rest) = strip_verb(step, "keyup") {
        return Ok(Step::KeyUp(parse_chord(rest.trim())?));
    }
    if let Some(rest) = strip_verb(step, "type") {
        return Ok(Step::Type(parse_quoted(rest.trim())?));
    }
    Err(format!("unknown step `{step}`"))
}

fn strip_verb<'a>(step: &'a str, verb: &str) -> Option<&'a str> {
    if step.len() < verb.len() {
        return None;
    }
    if !step[..verb.len()].eq_ignore_ascii_case(verb) {
        return None;
    }
    let rest = &step[verb.len()..];
    if rest.is_empty() {
        return Some("");
    }
    if rest.starts_with(char::is_whitespace) {
        return Some(rest.trim_start());
    }
    None
}

fn parse_quoted(s: &str) -> Result<String, String> {
    let s = s.trim();
    if !s.starts_with('"') {
        return Err(format!("type expects a quoted string, got `{s}`"));
    }
    let mut out = String::new();
    let mut chars = s[1..].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => return Err("type string ends with backslash".into()),
            },
            '"' => {
                if chars.peek().is_some() {
                    return Err("unexpected text after type string".into());
                }
                return Ok(out);
            }
            c => out.push(c),
        }
    }
    Err("unclosed type string".into())
}

fn parse_chord(s: &str) -> Result<KeyChord, String> {
    if s.is_empty() {
        return Err("missing key".into());
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut win = false;
    let mut remaining = s;
    loop {
        let lower = remaining.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("ctrl+") {
            ctrl = true;
            remaining = &remaining[remaining.len() - rest.len()..];
            continue;
        }
        if let Some(rest) = lower.strip_prefix("alt+") {
            alt = true;
            remaining = &remaining[remaining.len() - rest.len()..];
            continue;
        }
        if let Some(rest) = lower.strip_prefix("shift+") {
            shift = true;
            remaining = &remaining[remaining.len() - rest.len()..];
            continue;
        }
        if let Some(rest) = lower.strip_prefix("win+") {
            win = true;
            remaining = &remaining[remaining.len() - rest.len()..];
            continue;
        }
        break;
    }
    let key = parse_key(remaining)?;
    Ok(KeyChord {
        ctrl,
        alt,
        shift,
        win,
        key,
    })
}

fn parse_key(s: &str) -> Result<Key, String> {
    let lower = s.trim().to_ascii_lowercase();
    Ok(match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        f if f.starts_with('f') && f.len() <= 3 => {
            let n: u8 = f[1..].parse().map_err(|_| format!("unknown key `{s}`"))?;
            if (1..=12).contains(&n) {
                Key::F(n)
            } else {
                return Err(format!("unknown key `{s}`"));
            }
        }
        one if one.chars().count() == 1 => {
            let ch = one.chars().next().unwrap();
            if ch.is_ascii_alphanumeric() {
                Key::Char(ch.to_ascii_lowercase())
            } else {
                return Err(format!("unknown key `{s}`"));
            }
        }
        _ => return Err(format!("unknown key `{s}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIDEOUT: &str = r#"game: 238960|Path of Exile
name: Hideout
action: focus > wait 100 > press enter > type "/hideout" > press enter
"#;

    #[test]
    fn parses_hideout_single() {
        let doc = parse(HIDEOUT).unwrap();
        assert_eq!(doc.game_header(), Some("238960|Path of Exile"));
        assert!(!doc.macros.is_empty());
        assert_eq!(doc.macros.len(), 1);
        assert_eq!(doc.macros[0].name, "Hideout");
        assert_eq!(
            doc.macros[0].steps,
            vec![
                Step::Focus,
                Step::Wait(100),
                Step::Press(KeyChord {
                    ctrl: false,
                    alt: false,
                    shift: false,
                    win: false,
                    key: Key::Enter,
                }),
                Step::Type("/hideout".into()),
                Step::Press(KeyChord {
                    ctrl: false,
                    alt: false,
                    shift: false,
                    win: false,
                    key: Key::Enter,
                }),
            ]
        );
    }

    #[test]
    fn parses_catalog() {
        let text = r#"game: 238960|Path of Exile
name: Hideout
action: focus > press enter
name: Remaining
action: focus > type "/remaining"
"#;
        let doc = parse(text).unwrap();
        assert!(doc.macros.len() >= 2);
        assert_eq!(doc.macros.len(), 2);
        assert_eq!(doc.macros[1].name, "Remaining");
    }

    #[test]
    fn type_keeps_gt_inside_quotes() {
        let steps = parse_action(r#"type "a > b""#).unwrap();
        assert_eq!(steps, vec![Step::Type("a > b".into())]);
    }

    #[test]
    fn type_escapes() {
        let steps = parse_action(r#"type "say \"hi\" \\ ok""#).unwrap();
        assert_eq!(steps, vec![Step::Type(r#"say "hi" \ ok"#.into())]);
    }

    #[test]
    fn game_ref_split() {
        assert_eq!(
            GameRef::parse("238960|Path of Exile"),
            GameRef {
                appid: Some(238960),
                name: "Path of Exile".into(),
            }
        );
        assert_eq!(
            GameRef::parse("|Standalone"),
            GameRef {
                appid: None,
                name: "Standalone".into(),
            }
        );
        assert_eq!(
            GameRef::parse("42|"),
            GameRef {
                appid: Some(42),
                name: String::new(),
            }
        );
    }

    #[test]
    fn rejects_unknown_step() {
        let err = parse_action("focus > explode").unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn rejects_wait_too_long() {
        assert!(parse_action("wait 2001").is_err());
    }

    #[test]
    fn export_round_trip_headers() {
        let doc = parse(HIDEOUT).unwrap();
        let macros: Vec<_> = doc
            .macros
            .iter()
            .map(|m| (m.name.clone(), m.action.clone(), &m.meta))
            .collect();
        let text = export(&doc.headers, &macros);
        let again = parse(&text).unwrap();
        assert_eq!(again.macros[0].name, "Hideout");
        assert_eq!(again.macros[0].steps, doc.macros[0].steps);
    }

    #[test]
    fn chord_modifiers() {
        let steps = parse_action("press ctrl+shift+v").unwrap();
        match &steps[0] {
            Step::Press(c) => {
                assert!(c.ctrl && c.shift && !c.alt);
                assert_eq!(c.key, Key::Char('v'));
            }
            _ => panic!("expected press"),
        }
    }

    #[test]
    fn comments_and_blank_lines() {
        let text = r#"
# Path of Exile hideout
game: 238960|Path of Exile

name: Hideout
# ignore
action: focus > wait 50 > press enter
"#;
        let doc = parse(text).unwrap();
        assert_eq!(doc.macros[0].steps.len(), 3);
    }
}
