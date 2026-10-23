use std::borrow::Cow;

use turso_core::{LimboError, Result, Value};

use super::collation::compare_bytes;

use super::convert::{
    bytes_value, integer, string, string_converted_to_text, Str, MAX_ALLOWED_PACKET,
};

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

pub(super) fn left(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(length)) = (string(&args[0]), integer(&args[1])) else {
        return Ok(Value::Null);
    };
    if !length.is_positive() {
        return text.empty().into_value();
    }
    let length = length.as_unsigned();
    if text.len() as u64 <= length {
        return text.into_value();
    }
    let end = text.char_length(0, length as usize);
    if text.len() <= end {
        return text.into_value();
    }
    text.slice(0, end).into_value()
}

pub(super) fn right(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(length)) = (string(&args[0]), integer(&args[1])) else {
        return Ok(Value::Null);
    };
    if !length.is_positive() {
        return text.empty().into_value();
    }
    let length = length.as_unsigned();
    if text.len() as u64 <= length {
        return text.into_value();
    }
    let char_count = text.char_count() as u64;
    if char_count <= length {
        return text.into_value();
    }
    let start = text.char_length(0, (char_count - length) as usize);
    text.slice(start, text.len()).into_value()
}

pub(super) fn substring(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(start)) = (string(&args[0]), integer(&args[1])) else {
        return Ok(Value::Null);
    };
    let length = match args.get(2) {
        None => i32::MAX as i64,
        Some(length) => {
            let Some(length) = integer(length) else {
                return Ok(Value::Null);
            };
            if !length.is_positive() {
                return text.empty().into_value();
            }
            length.value
        }
    };
    let length = if length <= 0 || length > i32::MAX as i64 {
        i32::MAX as i64
    } else {
        length
    };
    if start.value < i32::MIN as i64 || start.value > i32::MAX as i64 {
        return text.empty().into_value();
    }
    let start = if start.value < 0 {
        text.char_count() as i64 + start.value
    } else {
        start.value - 1
    };
    if start < 0 {
        return text.empty().into_value();
    }
    let start = text.char_length(0, start as usize);
    if start + 1 > text.len() {
        return text.empty().into_value();
    }
    let length = text
        .char_length(start, length as usize)
        .min(text.len() - start);
    text.slice(start, start + length).into_value()
}

pub(super) fn insert(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(start), Some(length), Some(replacement)) = (
        string(&args[0]),
        integer(&args[1]),
        integer(&args[2]),
        string_converted_to_text(&args[3], matches!(args[0], Value::Text(_)))?,
    ) else {
        return Ok(Value::Null);
    };
    let text_length = text.len() as i64;
    if start.value < 1 || start.value > text_length {
        return text.into_value();
    }
    let (text, replacement) = same_charset(text, replacement);
    let length = if length.value < 0 || length.value > text_length {
        text_length
    } else {
        length.value
    };
    let start = text.char_length(0, (start.value - 1) as usize);
    if start > text.len() {
        return text.into_value();
    }
    let length = text
        .char_length(start, length as usize)
        .min(text.len() - start);
    if (text.len() - length + replacement.len()) as u64 > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    let mut bytes = Vec::with_capacity(text.len() - length + replacement.len());
    bytes.extend_from_slice(&text.as_bytes()[..start]);
    bytes.extend_from_slice(replacement.as_bytes());
    bytes.extend_from_slice(&text.as_bytes()[start + length..]);
    bytes_value(bytes, text.is_binary())
}

pub(super) fn locate(args: &[Value]) -> Result<Value> {
    find(&args[1], &args[0], args.get(2))
}

pub(super) fn instr(args: &[Value]) -> Result<Value> {
    find(&args[0], &args[1], None)
}

fn find(haystack: &Value, needle: &Value, position: Option<&Value>) -> Result<Value> {
    let convert_binary = matches!(haystack, Value::Text(_)) || matches!(needle, Value::Text(_));
    let (Some(haystack), Some(needle)) = (
        string_converted_to_text(haystack, convert_binary)?,
        string_converted_to_text(needle, convert_binary)?,
    ) else {
        return Ok(Value::Null);
    };
    let (haystack, needle) = same_charset(haystack, needle);
    let mut skipped_chars = 0;
    let mut start = 0;
    if let Some(position) = position {
        let Some(position) = integer(position) else {
            return Ok(Value::Null);
        };
        if position.value <= 0 {
            return Ok(Value::from_i64(0));
        }
        skipped_chars = position.value - 1;
        if skipped_chars as u64 > haystack.len() as u64 {
            return Ok(Value::from_i64(0));
        }
        start = haystack.char_length(0, skipped_chars as usize);
        if start + needle.len() > haystack.len() {
            return Ok(Value::from_i64(0));
        }
    }
    if needle.len() == 0 {
        return Ok(Value::from_i64(start as i64 + 1));
    }
    Ok(Value::from_i64(
        match_position(&haystack, &needle, start).map_or(0, |chars| chars + skipped_chars + 1),
    ))
}

fn match_position(haystack: &Str, needle: &Str, start: usize) -> Option<i64> {
    let bytes = haystack.as_bytes();
    let needle = needle.as_bytes();
    let binary = haystack.is_binary();
    let last = bytes.len().checked_sub(needle.len())?;
    let mut offset = start;
    let mut chars = 0;
    while offset <= last {
        let window = &bytes[offset..offset + needle.len()];
        if compare_bytes(window, needle, binary).is_eq()
            && (binary || std::str::from_utf8(window).is_ok())
        {
            return Some(chars);
        }
        offset += if binary {
            1
        } else {
            utf8_char_width(bytes[offset])
        };
        chars += 1;
    }
    None
}

pub(super) fn space(args: &[Value]) -> Result<Value> {
    let Some(count) = integer(&args[0]) else {
        return Ok(Value::Null);
    };
    if !count.is_positive() {
        return Ok(Value::build_text(""));
    }
    let count = count.count_up_to_int32_max();
    if count > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    Ok(Value::build_text(" ".repeat(count as usize)))
}

pub(super) fn repeat(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(count)) = (string(&args[0]), integer(&args[1])) else {
        return Ok(Value::Null);
    };
    if !count.is_positive() {
        return text.empty().into_value();
    }
    let count = count.count_up_to_int32_max();
    if count == 1 {
        return text.into_value();
    }
    if text.len() as u64 > MAX_ALLOWED_PACKET / count {
        return Ok(Value::Null);
    }
    bytes_value(text.as_bytes().repeat(count as usize), text.is_binary())
}

pub(super) fn lpad(args: &[Value]) -> Result<Value> {
    pad(args, Side::Left)
}

pub(super) fn rpad(args: &[Value]) -> Result<Value> {
    pad(args, Side::Right)
}

fn pad(args: &[Value], side: Side) -> Result<Value> {
    let (Some(text), Some(count), Some(padding)) = (
        string(&args[0]),
        integer(&args[1]),
        string_converted_to_text(&args[2], matches!(args[0], Value::Text(_)))?,
    ) else {
        return Ok(Value::Null);
    };
    if count.is_negative() {
        return Ok(Value::Null);
    }
    let count = count.count_up_to_int32_max() as usize;
    let (text, padding) = same_charset(text, padding);
    let text_chars = text.char_count();
    if count <= text_chars {
        return text.slice(0, text.char_length(0, count)).into_value();
    }
    let max_char_bytes = if text.is_binary() { 1 } else { 4 };
    if (count * max_char_bytes) as u64 > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    let padding_chars = padding.char_count();
    if padding_chars == 0 {
        return text.empty().into_value();
    }
    let missing = count - text_chars;
    let whole = padding.as_bytes().repeat(missing / padding_chars);
    let partial = padding.char_length(0, missing % padding_chars);
    let mut bytes = Vec::with_capacity(text.len() + whole.len() + partial);
    match side {
        Side::Left => {
            bytes.extend_from_slice(&whole);
            bytes.extend_from_slice(&padding.as_bytes()[..partial]);
            bytes.extend_from_slice(text.as_bytes());
        }
        Side::Right => {
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(&whole);
            bytes.extend_from_slice(&padding.as_bytes()[..partial]);
        }
    }
    bytes_value(bytes, text.is_binary())
}

pub(super) fn concat(args: &[Value]) -> Result<Value> {
    let Some(parts) = args.iter().map(string).collect::<Option<Vec<_>>>() else {
        return Ok(Value::Null);
    };
    join(&parts, b"")
}

pub(super) fn concat_ws(args: &[Value]) -> Result<Value> {
    let Some(separator) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let parts: Vec<_> = args[1..].iter().filter_map(string).collect();
    let binary = separator.is_binary() || parts.iter().any(Str::is_binary);
    let joined = join(&parts, separator.as_bytes())?;
    match joined {
        Value::Text(text) if binary => Ok(Value::from_slice(text.as_str().as_bytes())?),
        other => Ok(other),
    }
}

pub(super) fn replace(args: &[Value]) -> Result<Value> {
    let subject_is_text = matches!(args[0], Value::Text(_));
    let (Some(text), Some(from), Some(to)) = (
        string(&args[0]),
        string_converted_to_text(&args[1], subject_is_text)?,
        string_converted_to_text(&args[2], subject_is_text)?,
    ) else {
        return Ok(Value::Null);
    };
    if from.len() == 0 {
        return text.into_value();
    }
    let binary = text.is_binary() || from.is_binary() || to.is_binary();
    let found = find_bytes(text.as_bytes(), from.as_bytes(), 0);
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = 0;
    let mut next = found;
    while let Some(offset) = next {
        bytes.extend_from_slice(&text.as_bytes()[rest..offset]);
        bytes.extend_from_slice(to.as_bytes());
        rest = offset + from.len();
        if bytes.len() as u64 > MAX_ALLOWED_PACKET {
            return Ok(Value::Null);
        }
        next = find_bytes(text.as_bytes(), from.as_bytes(), rest);
    }
    bytes.extend_from_slice(&text.as_bytes()[rest..]);
    bytes_value(bytes, binary)
}

pub(super) fn trim(args: &[Value]) -> Result<Value> {
    trim_ends(args, Ends::Both)
}

pub(super) fn ltrim(args: &[Value]) -> Result<Value> {
    trim_ends(args, Ends::Leading)
}

pub(super) fn rtrim(args: &[Value]) -> Result<Value> {
    trim_ends(args, Ends::Trailing)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ends {
    Both,
    Leading,
    Trailing,
}

fn trim_ends(args: &[Value], ends: Ends) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let Some(removed) = string_converted_to_text(&args[1], !text.is_binary())? else {
        return Ok(Value::Null);
    };
    let removed = removed.as_bytes();
    if removed.is_empty() {
        return text.into_value();
    }
    let bytes = text.as_bytes();
    let (mut start, mut end) = (0, bytes.len());
    if ends != Ends::Trailing {
        while bytes[start..end].starts_with(removed) {
            start += removed.len();
        }
    }
    if ends != Ends::Leading {
        while bytes[start..end].ends_with(removed) {
            end -= removed.len();
        }
    }
    text.slice(start, end).into_value()
}

pub(super) fn cast_as_char(args: &[Value]) -> Result<Value> {
    cast_to_text(args, false)
}

pub(super) fn cast_as_nchar(args: &[Value]) -> Result<Value> {
    cast_to_text(args, true)
}

fn cast_to_text(args: &[Value], national: bool) -> Result<Value> {
    let Some(value) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let Ok(text) = std::str::from_utf8(value.as_bytes()) else {
        return Ok(Value::Null);
    };
    let length = match integer(&args[1]) {
        Some(length) if !length.is_negative() => length.as_unsigned() as usize,
        _ => {
            return Err(LimboError::InvalidArgument(
                "the length of a CHAR cast must be a non-negative integer".to_string(),
            ))
        }
    };
    let converted: String = text
        .chars()
        .take(length)
        .map(|ch| {
            if national && ch.len_utf8() == 4 {
                '?'
            } else {
                ch
            }
        })
        .collect();
    Ok(Value::build_text(converted))
}

pub(super) fn char(args: &[Value]) -> Result<Value> {
    let mut bytes = Vec::new();
    for code in args.iter().filter_map(integer) {
        let code = code.value as i32 as u32;
        let width = 4 - (code.leading_zeros() / 8).min(3) as usize;
        bytes.extend_from_slice(&code.to_be_bytes()[4 - width..]);
    }
    Ok(Value::from_slice(&bytes)?)
}

pub(super) fn lower(args: &[Value]) -> Result<Value> {
    change_case(&args[0], char::to_lowercase)
}

pub(super) fn upper(args: &[Value]) -> Result<Value> {
    change_case(&args[0], char::to_uppercase)
}

fn change_case<I: Iterator<Item = char> + ExactSizeIterator>(
    value: &Value,
    convert: fn(char) -> I,
) -> Result<Value> {
    match string(value) {
        None => Ok(Value::Null),
        Some(Str::Text(text)) => Ok(Value::build_text(
            text.chars()
                .map(|letter| {
                    let mut converted = convert(letter);
                    match converted.len() {
                        1 => converted.next().expect("one character"),
                        _ => letter,
                    }
                })
                .collect::<String>(),
        )),
        Some(binary) => binary.into_owned().into_value(),
    }
}

pub(super) fn ascii(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        Value::from_i64(text.as_bytes().first().map_or(0, |byte| i64::from(*byte)))
    }))
}

pub(super) fn ord(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        let bytes = text.as_bytes();
        let Some(first) = bytes.first() else {
            return Value::from_i64(0);
        };
        let width = if text.is_binary() {
            1
        } else {
            utf8_char_width(*first).min(bytes.len())
        };
        Value::from_i64(
            bytes[..width]
                .iter()
                .fold(0, |code, byte| (code << 8) | i64::from(*byte)),
        )
    }))
}

fn utf8_char_width(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

pub(super) fn bit_length(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| Value::from_i64(text.len() as i64 * 8)))
}

pub(super) fn length(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| Value::from_i64(text.len() as i64)))
}

pub(super) fn char_length(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(
        Value::Null,
        |text| Value::from_i64(text.char_count() as i64),
    ))
}

pub(super) fn hex(args: &[Value]) -> Result<Value> {
    Ok(match &args[0] {
        Value::Null => Value::Null,
        number @ Value::Numeric(_) => {
            let number = integer(number).expect("a number is not NULL");
            Value::build_text(format!("{:X}", number.as_unsigned()))
        }
        other => {
            let text = string(other).expect("a string is not NULL");
            Value::build_text(hex::encode_upper(text.as_bytes()))
        }
    })
}

pub(super) fn unhex(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let digits = text.as_bytes();
    let padded: Cow<[u8]> = if digits.len() % 2 == 1 {
        Cow::Owned([b"0", digits].concat())
    } else {
        Cow::Borrowed(digits)
    };
    match hex::decode(padded) {
        Ok(bytes) => Ok(Value::from_slice(&bytes)?),
        Err(_) => Ok(Value::Null),
    }
}

pub(super) fn find_in_set(args: &[Value]) -> Result<Value> {
    let (Some(needle), Some(list)) = (string(&args[0]), string(&args[1])) else {
        return Ok(Value::Null);
    };
    if list.len() < needle.len() || list.len() == 0 {
        return Ok(Value::from_i64(0));
    }
    let binary = needle.is_binary() || list.is_binary();
    let position = list
        .as_bytes()
        .split(|byte| *byte == b',')
        .position(|item| compare_bytes(item, needle.as_bytes(), binary).is_eq());
    Ok(Value::from_i64(
        position.map_or(0, |index| index as i64 + 1),
    ))
}

pub(super) fn substring_index(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(delimiter), Some(count)) = (
        string(&args[0]),
        string_converted_to_text(&args[1], matches!(args[0], Value::Text(_)))?,
        integer(&args[2]),
    ) else {
        return Ok(Value::Null);
    };
    let count = count.value as i32;
    if text.len() == 0 || delimiter.len() == 0 || count == 0 {
        return text.empty().into_value();
    }
    let bytes = text.as_bytes();
    let delimiter = delimiter.as_bytes();
    let mut offsets = Vec::new();
    let mut next = find_bytes(bytes, delimiter, 0);
    while let Some(offset) = next {
        offsets.push(offset);
        next = find_bytes(bytes, delimiter, offset + delimiter.len());
    }
    let wanted = count.unsigned_abs() as usize;
    if offsets.len() < wanted {
        return text.into_value();
    }
    if count > 0 {
        text.slice(0, offsets[wanted - 1]).into_value()
    } else {
        let start = offsets[offsets.len() - wanted] + delimiter.len();
        text.slice(start, text.len()).into_value()
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

pub(super) fn elt(args: &[Value]) -> Result<Value> {
    let Some(index) = integer(&args[0]) else {
        return Ok(Value::Null);
    };
    if index.value < 1 || index.value >= args.len() as i64 {
        return Ok(Value::Null);
    }
    let binary = args[1..]
        .iter()
        .any(|value| matches!(value, Value::Blob(_)));
    match string(&args[index.value as usize]) {
        None => Ok(Value::Null),
        Some(chosen) if binary => chosen.into_binary().into_value(),
        Some(chosen) => chosen.into_owned().into_value(),
    }
}

pub(super) fn make_set(args: &[Value]) -> Result<Value> {
    let Some(bits) = integer(&args[0]) else {
        return Ok(Value::Null);
    };
    let names = &args[1..];
    let mut bits = bits.as_unsigned();
    if names.len() < 64 {
        bits &= (1 << names.len()) - 1;
    }
    let chosen: Vec<_> = names
        .iter()
        .enumerate()
        .filter(|(index, _)| bits & (1 << index) != 0)
        .filter_map(|(_, name)| string(name))
        .collect();
    let joined = join(&chosen, b",")?;
    let binary = names.iter().any(|value| matches!(value, Value::Blob(_)));
    match joined {
        Value::Text(text) if binary => Ok(Value::from_slice(text.as_str().as_bytes())?),
        other => Ok(other),
    }
}

pub(super) fn export_set(args: &[Value]) -> Result<Value> {
    let (Some(bits), Some(on), Some(off)) = (integer(&args[0]), string(&args[1]), string(&args[2]))
    else {
        return Ok(Value::Null);
    };
    let bit_count = match args.get(4) {
        None => 64,
        Some(count) => match integer(count) {
            None => return Ok(Value::Null),
            Some(count) => count.as_unsigned().min(64),
        },
    };
    let separator = match args.get(3) {
        None => Str::Text(Cow::Borrowed(",")),
        Some(separator) => match string(separator) {
            None => return Ok(Value::Null),
            Some(separator) => separator,
        },
    };
    let separators = bit_count.saturating_sub(1);
    let longest = on.len().max(off.len()) as u64;
    if bit_count * longest + separators * separator.len() as u64 > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    let bits = bits.as_unsigned();
    let parts: Vec<Str> = (0..bit_count)
        .map(|bit| {
            if bits & (1 << bit) != 0 {
                on.clone()
            } else {
                off.clone()
            }
        })
        .collect();
    let binary = on.is_binary() || off.is_binary() || separator.is_binary();
    let joined = join(&parts, separator.as_bytes())?;
    match joined {
        Value::Text(text) if binary => Ok(Value::from_slice(text.as_str().as_bytes())?),
        other => Ok(other),
    }
}

fn join(parts: &[Str], separator: &[u8]) -> Result<Value> {
    let length: usize =
        parts.iter().map(Str::len).sum::<usize>() + separator.len() * parts.len().saturating_sub(1);
    if length as u64 > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    let mut bytes = Vec::with_capacity(length);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            bytes.extend_from_slice(separator);
        }
        bytes.extend_from_slice(part.as_bytes());
    }
    bytes_value(bytes, parts.iter().any(Str::is_binary))
}

fn same_charset<'a, 'b>(left: Str<'a>, right: Str<'b>) -> (Str<'a>, Str<'b>) {
    if left.is_binary() == right.is_binary() {
        (left, right)
    } else {
        (left.into_binary(), right.into_binary())
    }
}
