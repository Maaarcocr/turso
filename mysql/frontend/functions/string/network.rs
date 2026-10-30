use std::fmt::Write as _;

use turso_core::{LimboError, Numeric, Result, Value};

use super::convert::{double, integer, string};

pub(super) fn inet_aton(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let mut address: u64 = 0;
    let mut group: u64 = 0;
    let mut dots = 0;
    let mut last = b'.';
    for &byte in text.as_bytes() {
        last = byte;
        match byte {
            b'0'..=b'9' => {
                group = group * 10 + u64::from(byte - b'0');
                if group > 255 {
                    return Ok(Value::Null);
                }
            }
            b'.' => {
                dots += 1;
                address = (address << 8) + group;
                group = 0;
            }
            _ => return Ok(Value::Null),
        }
    }
    if last == b'.' {
        return Ok(Value::Null);
    }
    match dots {
        1 => address <<= 16,
        2 => address <<= 8,
        _ => {}
    }
    Ok(Value::from_i64(((address << 8) + group) as i64))
}

pub(super) fn inet_ntoa(args: &[Value]) -> Result<Value> {
    let Some(address) = integer(&args[0]) else {
        return Ok(Value::Null);
    };
    let address = address.as_unsigned();
    if address > u64::from(u32::MAX) {
        return Ok(Value::Null);
    }
    Ok(Value::build_text(ipv4_text(
        &(address as u32).to_be_bytes(),
    )))
}

pub(super) fn inet6_aton(args: &[Value]) -> Result<Value> {
    let Some(text) = string_argument(&args[0]) else {
        return Ok(Value::Null);
    };
    let bytes = text.as_bytes();
    if let Some(address) = parse_ipv4(bytes) {
        return Ok(Value::from_slice(&address)?);
    }
    match parse_ipv6(bytes) {
        Some(address) => Ok(Value::from_slice(&address)?),
        None => Ok(Value::Null),
    }
}

pub(super) fn inet6_ntoa(args: &[Value]) -> Result<Value> {
    let Value::Blob(address) = &args[0] else {
        return Ok(Value::Null);
    };
    Ok(match address.len() {
        4 => Value::build_text(ipv4_text(address)),
        16 => Value::build_text(ipv6_text(address.as_slice().try_into().expect("16 bytes"))),
        _ => Value::Null,
    })
}

pub(super) fn is_ipv4(args: &[Value]) -> Result<Value> {
    Ok(address_check(&args[0], |value| {
        string_argument(value).is_some_and(|text| parse_ipv4(text.as_bytes()).is_some())
    }))
}

pub(super) fn is_ipv6(args: &[Value]) -> Result<Value> {
    Ok(address_check(&args[0], |value| {
        string_argument(value).is_some_and(|text| parse_ipv6(text.as_bytes()).is_some())
    }))
}

pub(super) fn is_ipv4_compat(args: &[Value]) -> Result<Value> {
    Ok(address_check(&args[0], |value| {
        ipv6_bytes(value).is_some_and(|address| {
            address[..12].iter().all(|byte| *byte == 0)
                && u32::from_be_bytes(address[12..].try_into().expect("4 bytes")) > 1
        })
    }))
}

pub(super) fn is_ipv4_mapped(args: &[Value]) -> Result<Value> {
    Ok(address_check(&args[0], |value| {
        ipv6_bytes(value).is_some_and(|address| {
            address[..10].iter().all(|byte| *byte == 0) && address[10..12] == [0xFF, 0xFF]
        })
    }))
}

/// `UUID()`, lowered as `__mysql_uuid(randomblob(8))`: a version 1 UUID as
/// MySQL makes one, of the time in 100 ns intervals since 1582-10-15 (one
/// interval after the last UUID's when the clock has not moved on), with a
/// clock sequence and a random node drawn once per process from the bytes of
/// the first call.
pub(super) fn uuid(args: &[Value]) -> Result<Value> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};
    const INTERVALS_BEFORE_UNIX_EPOCH: u64 = 0x01B2_1DD2_1381_4000;
    static LAST_TIME: AtomicU64 = AtomicU64::new(0);
    static SEQUENCE_AND_NODE: OnceLock<u64> = OnceLock::new();
    let Value::Blob(random) = &args[0] else {
        return Err(LimboError::InternalError(
            "UUID draws from a value that is not randomblob(8)".to_string(),
        ));
    };
    let random: [u8; 8] = random.as_slice().try_into().map_err(|_| {
        LimboError::InternalError("UUID draws from a blob that is not 8 bytes".to_string())
    })?;
    let sequence_and_node = *SEQUENCE_AND_NODE.get_or_init(|| u64::from_be_bytes(random));
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let now = u64::try_from(elapsed.as_nanos() / 100)
        .unwrap_or(u64::MAX)
        .saturating_add(INTERVALS_BEFORE_UNIX_EPOCH);
    let mut last = LAST_TIME.load(Ordering::Relaxed);
    let time = loop {
        let next = now.max(last + 1);
        match LAST_TIME.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break next,
            Err(current) => last = current,
        }
    };
    let time_low = time & 0xFFFF_FFFF;
    let time_mid = (time >> 32) & 0xFFFF;
    let time_high_and_version = ((time >> 48) & 0x0FFF) | 0x1000;
    let clock_sequence = ((sequence_and_node >> 48) & 0x3FFF) | 0x8000;
    let node = (sequence_and_node & 0xFFFF_FFFF_FFFF) | 0x0100_0000_0000;
    Ok(Value::build_text(format!(
        "{time_low:08x}-{time_mid:04x}-{time_high_and_version:04x}-{clock_sequence:04x}-{node:012x}"
    )))
}

pub(super) fn is_uuid(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        boolean(parse_uuid(text.as_bytes()).is_some())
    }))
}

pub(super) fn uuid_to_bin(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let Some(uuid) = parse_uuid(text.as_bytes()) else {
        return Err(LimboError::InvalidArgument(format!(
            "Incorrect string value: '{}' for function uuid_to_bin",
            String::from_utf8_lossy(text.as_bytes())
        )));
    };
    let uuid = if swaps_time_fields(args) {
        [&uuid[6..8], &uuid[4..6], &uuid[0..4], &uuid[8..]].concat()
    } else {
        uuid.to_vec()
    };
    Ok(Value::from_slice(&uuid)?)
}

pub(super) fn bin_to_uuid(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let bytes = text.as_bytes();
    if bytes.len() != 16 {
        return Err(LimboError::InvalidArgument(format!(
            "Incorrect string value: '{}' for function bin_to_uuid",
            String::from_utf8_lossy(bytes)
        )));
    }
    let uuid = if swaps_time_fields(args) {
        [&bytes[4..8], &bytes[2..4], &bytes[0..2], &bytes[8..]].concat()
    } else {
        bytes.to_vec()
    };
    let digits = hex::encode(uuid);
    Ok(Value::build_text(format!(
        "{}-{}-{}-{}-{}",
        &digits[0..8],
        &digits[8..12],
        &digits[12..16],
        &digits[16..20],
        &digits[20..32]
    )))
}

fn address_check(value: &Value, check: impl Fn(&Value) -> bool) -> Value {
    match value {
        Value::Null => Value::Null,
        value => boolean(check(value)),
    }
}

fn string_argument(value: &Value) -> Option<super::convert::Str<'_>> {
    match value {
        Value::Text(_) | Value::Blob(_) => string(value),
        _ => None,
    }
}

fn ipv6_bytes(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Blob(address) if address.len() == 16 => Some(address),
        _ => None,
    }
}

fn boolean(value: bool) -> Value {
    Value::from_i64(i64::from(value))
}

fn parse_ipv6(text: &[u8]) -> Option<[u8; 16]> {
    if !(2..=39).contains(&text.len()) {
        return None;
    }
    let text = text
        .split(|byte| *byte == 0)
        .next()
        .expect("split yields a part");
    let mut address = [0u8; 16];
    let mut position = 0;
    if text.first() == Some(&b':') {
        if text.get(1) != Some(&b':') {
            return None;
        }
        position = 1;
    }
    let mut written = 0;
    let mut gap = None;
    let mut group_start = position;
    let mut group_digits = 0;
    let mut group: u16 = 0;
    while position < text.len() {
        let byte = text[position];
        position += 1;
        match byte {
            b':' => {
                group_start = position;
                if group_digits == 0 {
                    if gap.is_some() {
                        return None;
                    }
                    gap = Some(written);
                    continue;
                }
                if position >= text.len() || written + 2 > 16 {
                    return None;
                }
                address[written..written + 2].copy_from_slice(&group.to_be_bytes());
                written += 2;
                group_digits = 0;
                group = 0;
            }
            b'.' => {
                if written + 4 > 16 {
                    return None;
                }
                let ipv4 = parse_ipv4(&text[group_start..])?;
                address[written..written + 4].copy_from_slice(&ipv4);
                written += 4;
                group_digits = 0;
                break;
            }
            _ => {
                let digit = (byte as char).to_digit(16)?;
                if group_digits >= 4 {
                    return None;
                }
                group = group << 4 | digit as u16;
                group_digits += 1;
            }
        }
    }
    if group_digits > 0 {
        if written + 2 > 16 {
            return None;
        }
        address[written..written + 2].copy_from_slice(&group.to_be_bytes());
        written += 2;
    }
    if let Some(gap) = gap {
        if written == 16 {
            return None;
        }
        let moved = written - gap;
        address.copy_within(gap..written, 16 - moved);
        address[gap..16 - moved].fill(0);
        written = 16;
    }
    (written == 16).then_some(address)
}

fn parse_ipv4(text: &[u8]) -> Option<[u8; 4]> {
    if !(7..=15).contains(&text.len()) {
        return None;
    }
    let text = text
        .split(|byte| *byte == 0)
        .next()
        .expect("split yields a part");
    let mut address = [0u8; 4];
    let mut group: u32 = 0;
    let mut digits = 0;
    let mut dots = 0;
    for &byte in text {
        match byte {
            b'0'..=b'9' => {
                digits += 1;
                group = group * 10 + u32::from(byte - b'0');
                if digits > 3 || group > 255 {
                    return None;
                }
            }
            b'.' => {
                if digits == 0 || dots == 3 {
                    return None;
                }
                address[dots] = group as u8;
                dots += 1;
                group = 0;
                digits = 0;
            }
            _ => return None,
        }
    }
    if text.last() == Some(&b'.') || dots != 3 {
        return None;
    }
    address[3] = group as u8;
    Some(address)
}

fn ipv6_text(address: &[u8; 16]) -> String {
    let words: Vec<u16> = address
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    let mut gap: Option<(usize, usize)> = None;
    let mut run: Option<(usize, usize)> = None;
    for (index, word) in words.iter().enumerate() {
        if *word == 0 {
            run = Some(run.map_or((index, 1), |(start, length)| (start, length + 1)));
        } else if let Some(current) = run.take() {
            if gap.is_none_or(|longest| current.1 > longest.1) {
                gap = Some(current);
            }
        }
    }
    if let Some(current) = run {
        if gap.is_none_or(|longest| current.1 > longest.1) {
            gap = Some(current);
        }
    }
    let mut text = String::new();
    let mut index = 0;
    while index < 8 {
        match gap {
            Some((start, length)) if index == start => {
                if index == 0 {
                    text.push(':');
                }
                text.push(':');
                index += length;
                continue;
            }
            Some((0, length))
                if index == 6 && (length == 6 || (length == 5 && words[5] == 0xFFFF)) =>
            {
                text.push_str(&ipv4_text(&address[12..]));
                return text;
            }
            _ => {
                write!(text, "{:x}", words[index]).expect("writing to a String succeeds");
                if index != 7 {
                    text.push(':');
                }
            }
        }
        index += 1;
    }
    text
}

fn ipv4_text(address: &[u8]) -> String {
    format!(
        "{}.{}.{}.{}",
        address[0], address[1], address[2], address[3]
    )
}

fn swaps_time_fields(args: &[Value]) -> bool {
    match args.get(1) {
        None | Some(Value::Null) => false,
        Some(Value::Numeric(Numeric::Integer(flag))) => *flag != 0,
        Some(flag) => double(flag).is_some_and(|flag| flag != 0.0),
    }
}

fn parse_uuid(text: &[u8]) -> Option<[u8; 16]> {
    let digits: Vec<u8> = match text.len() {
        32 => text.to_vec(),
        36 => dashed_uuid_digits(text)?,
        38 if text[0] == b'{' && text[37] == b'}' => dashed_uuid_digits(&text[1..37])?,
        _ => return None,
    };
    let mut uuid = [0u8; 16];
    hex::decode_to_slice(digits, &mut uuid).ok()?;
    Some(uuid)
}

fn dashed_uuid_digits(text: &[u8]) -> Option<Vec<u8>> {
    let groups: Vec<&[u8]> = text.split(|byte| *byte == b'-').collect();
    let lengths: Vec<usize> = groups.iter().map(|group| group.len()).collect();
    (lengths == [8, 4, 4, 4, 12]).then(|| groups.concat())
}
