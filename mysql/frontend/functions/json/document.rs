use std::cmp::Ordering;
use std::fmt::Write;

#[derive(Clone, Debug)]
pub(super) enum Json {
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Double(f64),
    String(String),
    Opaque(Vec<u8>),
    Array(Vec<Json>),
    Object(Object),
}

#[derive(Clone, Debug, Default)]
pub(super) struct Object {
    members: Vec<(String, Json)>,
}

impl Object {
    pub(super) fn get(&self, key: &str) -> Option<&Json> {
        self.find(key).ok().map(|index| &self.members[index].1)
    }

    pub(super) fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.find(key).ok().map(|index| &mut self.members[index].1)
    }

    pub(super) fn insert(&mut self, key: String, value: Json) {
        match self.find(&key) {
            Ok(index) => self.members[index].1 = value,
            Err(index) => self.members.insert(index, (key, value)),
        }
    }

    pub(super) fn remove(&mut self, key: &str) -> Option<Json> {
        self.find(key)
            .ok()
            .map(|index| self.members.remove(index).1)
    }

    pub(super) fn len(&self) -> usize {
        self.members.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &Json)> {
        self.members.iter().map(|(key, value)| (key, value))
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &Json> {
        self.members.iter().map(|(_, value)| value)
    }

    pub(super) fn into_members(self) -> impl Iterator<Item = (String, Json)> {
        self.members.into_iter()
    }

    fn find(&self, key: &str) -> Result<usize, usize> {
        self.members
            .binary_search_by(|(member, _)| compare_keys(member, key))
    }
}

fn compare_keys(left: &str, right: &str) -> Ordering {
    left.len()
        .cmp(&right.len())
        .then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

impl Json {
    pub(super) fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "NULL",
            Json::Boolean(_) => "BOOLEAN",
            Json::Integer(_) => "INTEGER",
            Json::Unsigned(_) => "UNSIGNED INTEGER",
            Json::Double(_) => "DOUBLE",
            Json::String(_) => "STRING",
            Json::Opaque(_) => "BLOB",
            Json::Array(_) => "ARRAY",
            Json::Object(_) => "OBJECT",
        }
    }

    pub(super) fn length(&self) -> usize {
        match self {
            Json::Array(items) => items.len(),
            Json::Object(object) => object.len(),
            _ => 1,
        }
    }

    pub(super) fn depth(&self) -> usize {
        let deepest_child = match self {
            Json::Array(items) => items.iter().map(Json::depth).max(),
            Json::Object(object) => object.values().map(Json::depth).max(),
            _ => None,
        };
        1 + deepest_child.unwrap_or(0)
    }

    pub(super) fn to_text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out);
        out
    }

    pub(super) fn to_pretty_text(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_text(&self, out: &mut String) {
        match self {
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    item.write_text(out);
                }
                out.push(']');
            }
            Json::Object(object) => {
                out.push('{');
                for (index, (key, value)) in object.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write_quoted(out, key);
                    out.push_str(": ");
                    value.write_text(out);
                }
                out.push('}');
            }
            scalar => scalar.write_scalar(out),
        }
    }

    fn write_pretty(&self, out: &mut String, level: usize) {
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_new_line(out, level + 1);
                    item.write_pretty(out, level + 1);
                }
                write_new_line(out, level);
                out.push(']');
            }
            Json::Object(object) if !object.is_empty() => {
                out.push('{');
                for (index, (key, value)) in object.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_new_line(out, level + 1);
                    write_quoted(out, key);
                    out.push_str(": ");
                    value.write_pretty(out, level + 1);
                }
                write_new_line(out, level);
                out.push('}');
            }
            other => other.write_text(out),
        }
    }

    fn write_scalar(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Boolean(true) => out.push_str("true"),
            Json::Boolean(false) => out.push_str("false"),
            Json::Integer(value) => write!(out, "{value}").expect("writing to a String"),
            Json::Unsigned(value) => write!(out, "{value}").expect("writing to a String"),
            Json::Double(value) => out.push_str(&format_json_double(*value)),
            Json::String(value) => write_quoted(out, value),
            Json::Opaque(bytes) => write_quoted(out, &format!("base64:type15:{}", base64(bytes))),
            Json::Array(_) | Json::Object(_) => unreachable!("containers are not scalars"),
        }
    }
}

fn write_new_line(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

pub(super) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    write_quoted(&mut out, value);
    out
}

fn write_quoted(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                write!(out, "\\u{:04x}", control as u32).expect("writing to a String")
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (index, byte)| {
            group | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (group >> (18 - 6 * index)) & 0x3f;
                out.push(ALPHABET[sextet as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn format_json_double(value: f64) -> String {
    let mut text = format_double(value);
    if !text.contains(['.', 'e']) {
        text.push_str(".0");
    }
    text
}

pub(super) fn format_double(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    let (digits, exponent) = shortest_digits(value.abs());
    let integer_digits = exponent + 1;
    let is_whole_number = digits.len() as i32 <= integer_digits;
    if exponent < -15 || (exponent >= 15 && is_whole_number) {
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{sign}{first}e{exponent}")
        } else {
            format!("{sign}{first}.{rest}e{exponent}")
        }
    } else if exponent < 0 {
        let zeros = "0".repeat((-integer_digits) as usize);
        format!("{sign}0.{zeros}{digits}")
    } else if is_whole_number {
        let zeros = "0".repeat(integer_digits as usize - digits.len());
        format!("{sign}{digits}{zeros}")
    } else {
        let (integer, fraction) = digits.split_at(integer_digits as usize);
        format!("{sign}{integer}.{fraction}")
    }
}

fn shortest_digits(value: f64) -> (String, i32) {
    let (digits, exponent) = scientific_digits(&format!("{value:e}"));
    let last = digits.as_bytes()[digits.len() - 1];
    for neighbour_last in [last.wrapping_sub(1), last + 1] {
        if !neighbour_last.is_ascii_digit() || neighbour_last % 2 != 0 {
            continue;
        }
        let mut neighbour = digits.clone().into_bytes();
        *neighbour.last_mut().expect("digits are not empty") = neighbour_last;
        let neighbour = String::from_utf8(neighbour).expect("digits are ASCII");
        if reads_back_as(&neighbour, exponent, value)
            && is_exact_midpoint(value, &digits, &neighbour, exponent)
        {
            return (neighbour, exponent);
        }
    }
    (digits, exponent)
}

fn scientific_digits(scientific: &str) -> (String, i32) {
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent = exponent.parse().expect("`{:e}` exponent is an integer");
    (mantissa.replace('.', ""), exponent)
}

fn reads_back_as(digits: &str, exponent: i32, value: f64) -> bool {
    format!("0.{digits}e{}", exponent + 1)
        .parse::<f64>()
        .is_ok_and(|parsed| parsed == value)
}

fn is_exact_midpoint(value: f64, digits: &str, neighbour: &str, exponent: i32) -> bool {
    let lower = digits.min(neighbour);
    let (exact, exact_exponent) = scientific_digits(&format!("{value:.800e}"));
    let midpoint = format!("{lower}5");
    exact_exponent == exponent
        && exact.starts_with(&midpoint)
        && exact[midpoint.len()..].bytes().all(|digit| digit == b'0')
}

pub(super) fn equal(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Null, Json::Null) => true,
        (Json::Boolean(left), Json::Boolean(right)) => left == right,
        (Json::String(left), Json::String(right)) => left == right,
        (Json::Opaque(left), Json::Opaque(right)) => left == right,
        (Json::Array(left), Json::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| equal(left, right))
        }
        (Json::Object(left), Json::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| equal(value, other)))
        }
        (left, right) => match (Number::of(left), Number::of(right)) {
            (Some(left), Some(right)) => left.equals(right),
            _ => false,
        },
    }
}

#[derive(Clone, Copy)]
enum Number {
    Whole(i128),
    Double(f64),
}

impl Number {
    fn of(value: &Json) -> Option<Number> {
        match value {
            Json::Integer(value) => Some(Number::Whole(i128::from(*value))),
            Json::Unsigned(value) => Some(Number::Whole(i128::from(*value))),
            Json::Double(value) => Some(Number::Double(*value)),
            _ => None,
        }
    }

    fn equals(self, other: Number) -> bool {
        match (self, other) {
            (Number::Whole(left), Number::Whole(right)) => left == right,
            (Number::Double(left), Number::Double(right)) => left == right,
            (Number::Whole(whole), Number::Double(double))
            | (Number::Double(double), Number::Whole(whole)) => {
                double.fract() == 0.0 && double.abs() < 1e38 && double as i128 == whole
            }
        }
    }
}
