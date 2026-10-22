use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};
use turso_core::{Result, Value};

use super::convert::{integer, is_space, string, MAX_ALLOWED_PACKET};

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_LINE_LENGTH: usize = 76;

pub(super) fn md5(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        Value::build_text(format!("{:x}", md5::compute(text.as_bytes())))
    }))
}

pub(super) fn sha1(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        Value::build_text(hex::encode(Sha1::digest(text.as_bytes())))
    }))
}

pub(super) fn sha2(args: &[Value]) -> Result<Value> {
    let (Some(text), Some(bits)) = (string(&args[0]), integer(&args[1])) else {
        return Ok(Value::Null);
    };
    let bytes = text.as_bytes();
    let digest = match bits.value {
        0 | 256 => hex::encode(Sha256::digest(bytes)),
        224 => hex::encode(Sha224::digest(bytes)),
        384 => hex::encode(Sha384::digest(bytes)),
        512 => hex::encode(Sha512::digest(bytes)),
        _ => return Ok(Value::Null),
    };
    Ok(Value::build_text(digest))
}

pub(super) fn crc32(args: &[Value]) -> Result<Value> {
    Ok(string(&args[0]).map_or(Value::Null, |text| {
        Value::from_i64(i64::from(crc32fast::hash(text.as_bytes())))
    }))
}

pub(super) fn to_base64(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    let bytes = text.as_bytes();
    let groups = bytes.len().div_ceil(3);
    if (groups * 4 + groups * 4 / BASE64_LINE_LENGTH) as u64 > MAX_ALLOWED_PACKET {
        return Ok(Value::Null);
    }
    let mut encoded = String::with_capacity(groups * 4 + groups * 4 / BASE64_LINE_LENGTH);
    for (index, chunk) in bytes.chunks(3).enumerate() {
        if index > 0 && index * 4 % BASE64_LINE_LENGTH == 0 {
            encoded.push('\n');
        }
        let word = chunk.iter().enumerate().fold(0u32, |word, (offset, byte)| {
            word | u32::from(*byte) << (16 - 8 * offset)
        });
        for position in 0..4 {
            if position <= chunk.len() {
                let sextet = (word >> (18 - 6 * position)) & 0x3F;
                encoded.push(BASE64_ALPHABET[sextet as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    Ok(Value::build_text(encoded))
}

pub(super) fn from_base64(args: &[Value]) -> Result<Value> {
    let Some(text) = string(&args[0]) else {
        return Ok(Value::Null);
    };
    match decode_base64(text.as_bytes()) {
        Some(bytes) => Ok(Value::from_slice(&bytes)?),
        None => Ok(Value::Null),
    }
}

fn decode_base64(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut symbols = encoded.iter().copied().filter(|byte| !is_space(*byte));
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    loop {
        let Some(first) = symbols.next() else {
            return Some(decoded);
        };
        let group = [first, symbols.next()?, symbols.next()?, symbols.next()?];
        let padding = match group {
            [_, _, b'=', b'='] => 2,
            [_, _, _, b'='] => 1,
            _ => 0,
        };
        let mut word = 0u32;
        for symbol in &group[..4 - padding] {
            word = word << 6 | base64_value(*symbol)?;
        }
        word <<= 6 * padding as u32;
        decoded.extend_from_slice(&word.to_be_bytes()[1..4 - padding]);
        if padding > 0 {
            return symbols.next().is_none().then_some(decoded);
        }
    }
}

fn base64_value(symbol: u8) -> Option<u32> {
    BASE64_ALPHABET
        .iter()
        .position(|candidate| *candidate == symbol)
        .map(|value| value as u32)
}
