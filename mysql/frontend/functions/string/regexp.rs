use std::cell::RefCell;

use fancy_regex::{Captures, Regex};

use turso_core::types::TextSubtype;
use turso_core::{LimboError, Result, Value};

use super::convert::{integer, string, Str};

const LINE_TERMINATORS: &str = r"\n\x0B\x0C\r\x{85}\x{2028}\x{2029}";

thread_local! {
    static LAST_PATTERN: RefCell<Option<(String, Regex)>> = const { RefCell::new(None) };
}

struct Subject {
    text: String,
    binary: bool,
}

impl Subject {
    fn byte_offset(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(offset, _)| offset)
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn into_value(self, text: String) -> Result<Value> {
        if self.binary {
            let bytes: Vec<u8> = text.chars().map(|letter| letter as u8).collect();
            Ok(Value::from_slice(&bytes)?)
        } else {
            Ok(Value::build_text(text))
        }
    }
}

struct Flags {
    case_insensitive: bool,
    multiline: bool,
    dot_matches_all: bool,
    unix_lines: bool,
}

pub(super) fn regexp_operator(args: &[Value]) -> Result<Value> {
    regexp_like(&[args[1].clone(), args[0].clone()])
}

pub(super) fn regexp_like(args: &[Value]) -> Result<Value> {
    let Some((subject, regex)) = prepare(args, args.get(2), "regexp_like")? else {
        return Ok(Value::Null);
    };
    Ok(Value::from_i64(i64::from(
        regex.is_match(&subject.text).map_err(regex_error)?,
    )))
}

pub(super) fn regexp_instr(args: &[Value]) -> Result<Value> {
    let Some((subject, regex)) = prepare(args, args.get(5), "regexp_instr")? else {
        return Ok(Value::Null);
    };
    let Some(numbers) = optional_integers(&args[2..args.len().min(5)]) else {
        return Ok(Value::Null);
    };
    let position = numbers.first().copied().unwrap_or(1);
    if position < 1 || position as usize > subject.char_count().max(1) {
        return Err(index_out_of_bounds());
    }
    let occurrence = numbers.get(1).copied().unwrap_or(1).max(1);
    let after_match = match numbers.get(2).copied().unwrap_or(0) {
        0 => false,
        1 => true,
        _ => {
            return Err(LimboError::InvalidArgument(
                "Incorrect arguments to regexp_instr: return_option must be 1 or 0.".to_string(),
            ))
        }
    };
    let start = subject.byte_offset(position as usize - 1);
    let region = &subject.text[start..];
    let Some((match_start, match_end)) = nth_match(&regex, region, 0, occurrence)? else {
        return Ok(Value::from_i64(0));
    };
    let offset = if after_match { match_end } else { match_start };
    Ok(Value::from_i64(
        region[..offset].chars().count() as i64 + position,
    ))
}

pub(super) fn regexp_substr(args: &[Value]) -> Result<Value> {
    let Some((subject, regex)) = prepare(args, args.get(4), "regexp_substr")? else {
        return Ok(Value::Null);
    };
    let Some(numbers) = optional_integers(&args[2..args.len().min(4)]) else {
        return Ok(Value::Null);
    };
    let start = search_start(&subject, numbers.first().copied(), "regexp_substr")?;
    let occurrence = numbers.get(1).copied().unwrap_or(1).max(1);
    let Some((match_start, match_end)) = nth_match(&regex, &subject.text, start, occurrence)?
    else {
        return Ok(Value::Null);
    };
    let found = subject.text[match_start..match_end].to_string();
    subject.into_value(found)
}

pub(super) fn regexp_replace(args: &[Value]) -> Result<Value> {
    let Some((subject, regex)) = prepare(args, args.get(5), "regexp_replace")? else {
        return Ok(Value::Null);
    };
    let (Some(replacement), Some(numbers)) = (
        string(&args[2]),
        optional_integers(&args[3..args.len().min(5)]),
    ) else {
        return Ok(Value::Null);
    };
    let replacement = text_of(&replacement);
    let start = search_start(&subject, numbers.first().copied(), "regexp_replace")?;
    let occurrence = numbers.get(1).copied().unwrap_or(0).max(0);
    let mut replaced = String::with_capacity(subject.text.len());
    let mut copied_up_to = 0;
    let mut search_from = start;
    let mut seen = 0;
    while let Some(captures) = regex
        .captures_from_pos(&subject.text, search_from)
        .map_err(regex_error)?
    {
        let whole = captures.get(0).expect("a match has a whole group");
        seen += 1;
        if occurrence == 0 || seen == occurrence {
            replaced.push_str(&subject.text[copied_up_to..whole.start()]);
            expand_replacement(&replacement, &captures, &mut replaced)?;
            copied_up_to = whole.end();
            if seen == occurrence {
                break;
            }
        }
        search_from = next_search_start(&subject.text, whole.start(), whole.end());
        if search_from > subject.text.len() {
            break;
        }
    }
    replaced.push_str(&subject.text[copied_up_to..]);
    subject.into_value(replaced)
}

fn prepare(
    args: &[Value],
    match_type: Option<&Value>,
    function: &str,
) -> Result<Option<(Subject, Regex)>> {
    let Some(pattern) = string(&args[1]) else {
        return Ok(None);
    };
    let subject_binary = matches!(args[0], Value::Blob(_));
    let pattern_binary = pattern.is_binary();
    let json = args[..2]
        .iter()
        .any(|arg| matches!(arg, Value::Text(text) if text.subtype == TextSubtype::Json));
    let mixes_binary_and_text = !json
        && ((subject_binary && matches!(args[1], Value::Text(_)))
            || (pattern_binary && matches!(args[0], Value::Text(_))));
    if mixes_binary_and_text {
        return Err(LimboError::InvalidArgument(format!(
            "Character set 'binary' cannot be used in conjunction with 'utf8mb4_0900_ai_ci' in call to {function}."
        )));
    }
    let pattern = text_of(&pattern);
    if pattern.is_empty() {
        return Err(LimboError::InvalidArgument(
            "Illegal argument to a regular expression.".to_string(),
        ));
    }
    let binary = subject_binary || pattern_binary;
    let mut flags = Flags {
        case_insensitive: !binary && !json,
        multiline: false,
        dot_matches_all: false,
        unix_lines: false,
    };
    if let Some(match_type) = match_type {
        let Some(match_type) = string(match_type) else {
            return Ok(None);
        };
        for letter in match_type.as_bytes() {
            match letter {
                b'c' => flags.case_insensitive = false,
                b'i' => flags.case_insensitive = true,
                b'm' => flags.multiline = true,
                b'n' => flags.dot_matches_all = true,
                b'u' => flags.unix_lines = true,
                _ => {
                    return Err(LimboError::InvalidArgument(format!(
                        "Incorrect arguments to {function}"
                    )))
                }
            }
        }
    }
    let regex = compile(&translate_pattern(&pattern, &flags)?)?;
    let Some(subject) = string(&args[0]) else {
        return Ok(None);
    };
    Ok(Some((
        Subject {
            text: text_of(&subject),
            binary,
        },
        regex,
    )))
}

fn text_of(value: &Str) -> String {
    match value {
        Str::Text(text) => text.to_string(),
        Str::Binary(bytes) => bytes.iter().map(|byte| char::from(*byte)).collect(),
    }
}

fn translate_pattern(pattern: &str, flags: &Flags) -> Result<String> {
    let mut translated = String::with_capacity(pattern.len() + 16);
    if flags.case_insensitive {
        translated.push_str("(?i)");
    }
    if flags.multiline {
        translated.push_str("(?m)");
    }
    if flags.dot_matches_all {
        translated.push_str("(?s)");
    }
    let any_but_line_end = if flags.unix_lines {
        r"[^\n]".to_string()
    } else {
        format!("[^{LINE_TERMINATORS}]")
    };
    let end_of_text = if flags.unix_lines {
        r"(?=\n?\z)".to_string()
    } else {
        format!(r"(?=(?:\r\n|[{LINE_TERMINATORS}])?\z)")
    };
    let mut chars = pattern.chars().peekable();
    let mut class_depth = 0;
    let mut can_repeat = false;
    while let Some(letter) = chars.next() {
        match letter {
            '\\' => match chars.next() {
                Some('Q') => {
                    let mut literal = String::new();
                    while let Some(next) = chars.next() {
                        if next == '\\' && chars.peek() == Some(&'E') {
                            chars.next();
                            break;
                        }
                        literal.push(next);
                    }
                    translated.push_str(&fancy_regex::escape(&literal));
                }
                Some(name @ ('p' | 'P' | 'N' | 'x')) if chars.peek() == Some(&'{') => {
                    translated.push('\\');
                    translated.push(name);
                    for letter in chars.by_ref() {
                        translated.push(letter);
                        if letter == '}' {
                            break;
                        }
                    }
                }
                Some(next) => {
                    translated.push('\\');
                    translated.push(next);
                }
                None => translated.push('\\'),
            },
            '[' => {
                class_depth += 1;
                translated.push('[');
                if chars.peek() == Some(&'^') {
                    chars.next();
                    translated.push('^');
                }
                if chars.peek() == Some(&']') {
                    chars.next();
                    translated.push_str(r"\]");
                }
            }
            ']' if class_depth > 0 => {
                class_depth -= 1;
                translated.push(']');
            }
            '.' if class_depth == 0 && !flags.dot_matches_all => {
                translated.push_str(&any_but_line_end);
            }
            '$' if class_depth == 0 && !flags.multiline => translated.push_str(&end_of_text),
            '(' if class_depth == 0 && pattern_continues_with(&chars, "?P") => {
                return Err(LimboError::InvalidArgument(
                    "Syntax error in regular expression".to_string(),
                ))
            }
            '{' if class_depth == 0 => {
                let position = pattern.chars().count() - chars.clone().count();
                let interval = take_interval(&mut chars, can_repeat, position)?;
                translated.push_str(&interval);
            }
            '}' if class_depth == 0 => {
                let position = pattern.chars().count() - chars.clone().count();
                return Err(regexp_syntax_error(position));
            }
            other => translated.push(other),
        }
        if class_depth == 0 {
            can_repeat = !matches!(letter, '(' | '|' | '*' | '+' | '?' | '{');
        }
    }
    Ok(translated)
}

fn take_interval(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    can_repeat: bool,
    position: usize,
) -> Result<String> {
    let mut interval = String::from("{");
    let mut bounds = vec![String::new()];
    for letter in chars.by_ref() {
        interval.push(letter);
        match letter {
            '}' => break,
            ',' if bounds.len() == 1 && !bounds[0].is_empty() => bounds.push(String::new()),
            digit if digit.is_ascii_digit() => bounds
                .last_mut()
                .expect("bounds start with one")
                .push(digit),
            _ => return Err(interval_error(can_repeat, position)),
        }
    }
    if !interval.ends_with('}') || bounds[0].is_empty() {
        return Err(interval_error(can_repeat, position));
    }
    if !can_repeat {
        return Err(regexp_syntax_error(position));
    }
    if bounds
        .iter()
        .filter(|bound| !bound.is_empty())
        .any(|bound| bound.parse::<i32>().is_err())
    {
        return Err(LimboError::InvalidArgument(
            "Decimal number in regular expression is too large.".to_string(),
        ));
    }
    Ok(interval)
}

fn interval_error(can_repeat: bool, position: usize) -> LimboError {
    if can_repeat {
        LimboError::InvalidArgument("Incorrect description of a {min,max} interval.".to_string())
    } else {
        regexp_syntax_error(position)
    }
}

fn regexp_syntax_error(position: usize) -> LimboError {
    LimboError::InvalidArgument(format!(
        "Syntax error in regular expression on line 1, character {position}."
    ))
}

fn pattern_continues_with(chars: &std::iter::Peekable<std::str::Chars>, expected: &str) -> bool {
    chars.clone().take(expected.len()).eq(expected.chars())
}

fn compile(pattern: &str) -> Result<Regex> {
    LAST_PATTERN.with(|last| {
        let mut last = last.borrow_mut();
        if let Some((cached, regex)) = last.as_ref() {
            if cached == pattern {
                return Ok(regex.clone());
            }
        }
        let regex = Regex::new(pattern).map_err(regex_error)?;
        *last = Some((pattern.to_string(), regex.clone()));
        Ok(regex)
    })
}

fn optional_integers(values: &[Value]) -> Option<Vec<i64>> {
    values
        .iter()
        .map(|value| integer(value).map(|value| value.value))
        .collect()
}

fn search_start(subject: &Subject, position: Option<i64>, function: &str) -> Result<usize> {
    let position = position.unwrap_or(1);
    if position < 1 {
        return Err(LimboError::InvalidArgument(format!(
            "Incorrect parameters in the call to native function '{function}'"
        )));
    }
    if position as u64 - 1 > subject.char_count() as u64 {
        return Err(index_out_of_bounds());
    }
    Ok(subject.byte_offset(position as usize - 1))
}

fn nth_match(
    regex: &Regex,
    text: &str,
    start: usize,
    occurrence: i64,
) -> Result<Option<(usize, usize)>> {
    let mut search_from = start;
    let mut seen = 0;
    while search_from <= text.len() {
        let Some(found) = regex
            .find_from_pos(text, search_from)
            .map_err(regex_error)?
        else {
            return Ok(None);
        };
        seen += 1;
        if seen == occurrence {
            return Ok(Some((found.start(), found.end())));
        }
        search_from = next_search_start(text, found.start(), found.end());
    }
    Ok(None)
}

fn next_search_start(text: &str, start: usize, end: usize) -> usize {
    if end > start {
        return end;
    }
    text[end..]
        .chars()
        .next()
        .map_or(text.len() + 1, |letter| end + letter.len_utf8())
}

fn expand_replacement(replacement: &str, captures: &Captures, out: &mut String) -> Result<()> {
    let mut chars = replacement.chars().peekable();
    while let Some(letter) = chars.next() {
        match letter {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '$' => {
                let mut group = None;
                while let Some(digit) = chars.peek().and_then(|next| next.to_digit(10)) {
                    let candidate = group.unwrap_or(0) * 10 + digit as usize;
                    if candidate >= captures.len() {
                        break;
                    }
                    group = Some(candidate);
                    chars.next();
                }
                let Some(group) = group else {
                    return Err(index_out_of_bounds());
                };
                if let Some(text) = captures.get(group) {
                    out.push_str(text.as_str());
                }
            }
            other => out.push(other),
        }
    }
    Ok(())
}

fn regex_error(error: fancy_regex::Error) -> LimboError {
    LimboError::InvalidArgument(format!("Illegal argument to a regular expression: {error}"))
}

fn index_out_of_bounds() -> LimboError {
    LimboError::InvalidArgument("Index out of bounds in regular expression search.".to_string())
}
