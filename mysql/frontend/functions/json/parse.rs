use std::sync::OnceLock;

use super::document::{Json, Object};

#[derive(Debug)]
pub(super) enum ParseError {
    Syntax {
        message: &'static str,
        offset: usize,
    },
    TooDeep,
}

const MAXIMUM_DEPTH: usize = 100;

pub(super) fn parse(text: &str) -> Result<Json, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        position: 0,
        depth: 0,
    };
    parser.skip_whitespace();
    if parser.peek() == 0 {
        return Err(parser.error("The document is empty.", parser.position));
    }
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.peek() != 0 {
        return Err(parser.error(
            "The document root must not be followed by other values.",
            parser.position,
        ));
    }
    Ok(value)
}

pub(super) fn parse_string(text: &str) -> Option<String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        position: 0,
        depth: 0,
    };
    let value = parser.parse_quoted().ok()?;
    (parser.position == text.len()).then_some(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self) -> Result<Json, ParseError> {
        match self.peek() {
            b'n' => self.parse_literal("null", Json::Null),
            b't' => self.parse_literal("true", Json::Boolean(true)),
            b'f' => self.parse_literal("false", Json::Boolean(false)),
            b'"' => self.parse_quoted().map(Json::String),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            _ => self.parse_number(),
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> Result<Json, ParseError> {
        self.position += 1;
        for expected in &literal.as_bytes()[1..] {
            if !self.consume(*expected) {
                return Err(self.error("Invalid value.", self.position));
            }
        }
        Ok(value)
    }

    fn parse_object(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.position += 1;
        let mut object = Object::default();
        self.skip_whitespace();
        if self.consume(b'}') {
            self.depth -= 1;
            return Ok(Json::Object(object));
        }
        loop {
            if self.peek() != b'"' {
                return Err(self.error("Missing a name for object member.", self.position));
            }
            let key = self.parse_quoted()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(self.error(
                    "Missing a colon after a name of object member.",
                    self.position,
                ));
            }
            self.skip_whitespace();
            let value = self.parse_value()?;
            object.insert(key, value);
            self.skip_whitespace();
            if self.consume(b',') {
                self.skip_whitespace();
            } else if self.consume(b'}') {
                self.depth -= 1;
                return Ok(Json::Object(object));
            } else {
                return Err(self.error(
                    "Missing a comma or '}' after an object member.",
                    self.position,
                ));
            }
        }
    }

    fn parse_array(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.position += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.consume(b']') {
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume(b',') {
                self.skip_whitespace();
            } else if self.consume(b']') {
                self.depth -= 1;
                return Ok(Json::Array(items));
            } else {
                return Err(self.error(
                    "Missing a comma or ']' after an array element.",
                    self.position,
                ));
            }
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAXIMUM_DEPTH {
            return Err(ParseError::TooDeep);
        }
        Ok(())
    }

    fn parse_quoted(&mut self) -> Result<String, ParseError> {
        self.position += 1;
        let mut value = Vec::new();
        loop {
            let byte = self.peek();
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(String::from_utf8(value).expect("JSON input is UTF-8"));
                }
                b'\\' => self.parse_escape(&mut value)?,
                0 => {
                    return Err(
                        self.error("Missing a closing quotation mark in string.", self.position)
                    )
                }
                control if control < 0x20 => {
                    return Err(self.error("Invalid encoding in string.", self.position))
                }
                other => {
                    value.push(other);
                    self.position += 1;
                }
            }
        }
    }

    fn parse_escape(&mut self, value: &mut Vec<u8>) -> Result<(), ParseError> {
        let escape_offset = self.position;
        self.position += 1;
        let simple = match self.peek() {
            b'"' => Some(b'"'),
            b'\\' => Some(b'\\'),
            b'/' => Some(b'/'),
            b'b' => Some(8),
            b'f' => Some(12),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            _ => None,
        };
        if let Some(byte) = simple {
            self.position += 1;
            value.push(byte);
            return Ok(());
        }
        if !self.consume(b'u') {
            return Err(self.error("Invalid escape character in string.", escape_offset));
        }
        let mut code_point = self.parse_hex4(escape_offset)?;
        if (0xD800..=0xDBFF).contains(&code_point) {
            if !(self.consume(b'\\') && self.consume(b'u')) {
                return Err(self.error("The surrogate pair in string is invalid.", escape_offset));
            }
            let low = self.parse_hex4(escape_offset)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.error("The surrogate pair in string is invalid.", escape_offset));
            }
            code_point = 0x10000 + ((code_point - 0xD800) << 10) + (low - 0xDC00);
        } else if (0xDC00..=0xDFFF).contains(&code_point) {
            return Err(self.error("The surrogate pair in string is invalid.", escape_offset));
        }
        let character = char::from_u32(code_point).expect("surrogates were handled");
        let mut buffer = [0; 4];
        value.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        Ok(())
    }

    fn parse_hex4(&mut self, escape_offset: usize) -> Result<u32, ParseError> {
        let mut code_point = 0;
        for _ in 0..4 {
            let Some(digit) = (self.peek() as char).to_digit(16) else {
                return Err(self.error(
                    "Incorrect hex digit after \\u escape in string.",
                    escape_offset,
                ));
            };
            code_point = code_point * 16 + digit;
            self.position += 1;
        }
        Ok(code_point)
    }

    fn parse_number(&mut self) -> Result<Json, ParseError> {
        let start = self.position;
        let minus = self.consume(b'-');
        let mut small: u32 = 0;
        let mut wide: u64 = 0;
        let mut use_wide = false;
        let mut significant_digits = 0;
        if self.peek() == b'0' {
            self.position += 1;
        } else if self.peek().is_ascii_digit() {
            small = u32::from(self.take_digit());
            let (limit, last_digit) = if minus {
                (214_748_364, b'8')
            } else {
                (429_496_729, b'5')
            };
            while self.peek().is_ascii_digit() {
                if small >= limit && (small != limit || self.peek() > last_digit) {
                    wide = u64::from(small);
                    use_wide = true;
                    break;
                }
                small = small * 10 + u32::from(self.take_digit());
                significant_digits += 1;
            }
        } else {
            return Err(self.error("Invalid value.", self.position));
        }

        let mut use_double = false;
        let mut double = 0.0;
        if use_wide {
            let (limit, last_digit) = if minus {
                (0x0CCC_CCCC_CCCC_CCCC, b'8')
            } else {
                (0x1999_9999_9999_9999, b'5')
            };
            while self.peek().is_ascii_digit() {
                if wide >= limit && (wide != limit || self.peek() > last_digit) {
                    double = wide as f64;
                    use_double = true;
                    break;
                }
                wide = wide * 10 + u64::from(self.take_digit());
                significant_digits += 1;
            }
        }
        if use_double {
            while self.peek().is_ascii_digit() {
                if double >= 1.797_693_134_862_315_7e307 {
                    return Err(self.error("Number too big to be stored in double.", start));
                }
                double = double * 10.0 + f64::from(self.take_digit());
            }
        }

        let mut fraction_exponent: i32 = 0;
        if self.consume(b'.') {
            if !self.peek().is_ascii_digit() {
                return Err(self.error("Miss fraction part in number.", self.position));
            }
            if !use_double {
                if !use_wide {
                    wide = u64::from(small);
                }
                while self.peek().is_ascii_digit() {
                    if wide > 0x1F_FFFF_FFFF_FFFF {
                        break;
                    }
                    wide = wide * 10 + u64::from(self.take_digit());
                    fraction_exponent -= 1;
                    if wide != 0 {
                        significant_digits += 1;
                    }
                }
                double = wide as f64;
                use_double = true;
            }
            while self.peek().is_ascii_digit() {
                let digit = self.take_digit();
                if significant_digits < 17 {
                    double = double * 10.0 + f64::from(digit);
                    fraction_exponent -= 1;
                    if double > 0.0 {
                        significant_digits += 1;
                    }
                }
            }
        }

        let mut exponent: i32 = 0;
        if self.consume(b'e') || self.consume(b'E') {
            if !use_double {
                double = if use_wide {
                    wide as f64
                } else {
                    f64::from(small)
                };
                use_double = true;
            }
            let exponent_minus = if self.consume(b'+') {
                false
            } else {
                self.consume(b'-')
            };
            if !self.peek().is_ascii_digit() {
                return Err(self.error("Miss exponent in number.", self.position));
            }
            exponent = i32::from(self.take_digit());
            if exponent_minus {
                let maximum = (fraction_exponent + 2_147_483_639) / 10;
                while self.peek().is_ascii_digit() {
                    exponent = exponent * 10 + i32::from(self.take_digit());
                    if exponent > maximum {
                        while self.peek().is_ascii_digit() {
                            self.position += 1;
                        }
                    }
                }
                exponent = -exponent;
            } else {
                let maximum = 308 - fraction_exponent;
                while self.peek().is_ascii_digit() {
                    exponent = exponent * 10 + i32::from(self.take_digit());
                    if exponent > maximum {
                        return Err(self.error("Number too big to be stored in double.", start));
                    }
                }
            }
        }

        if use_double {
            let value = scale_by_power_of_ten(double, exponent + fraction_exponent);
            if value > f64::MAX {
                return Err(self.error("Number too big to be stored in double.", start));
            }
            return Ok(Json::Double(if minus { -value } else { value }));
        }
        Ok(match (use_wide, minus) {
            (true, true) => Json::Integer(wide.wrapping_neg() as i64),
            (true, false) => Json::Unsigned(wide),
            (false, true) => Json::Integer(-i64::from(small)),
            (false, false) => Json::Integer(i64::from(small)),
        })
    }

    fn take_digit(&mut self) -> u8 {
        let digit = self.peek() - b'0';
        self.position += 1;
        digit
    }

    fn peek(&self) -> u8 {
        self.bytes.get(self.position).copied().unwrap_or(0)
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == expected && expected != 0 {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), b' ' | b'\n' | b'\r' | b'\t') {
            self.position += 1;
        }
    }

    fn error(&self, message: &'static str, offset: usize) -> ParseError {
        ParseError::Syntax { message, offset }
    }
}

fn scale_by_power_of_ten(significand: f64, exponent: i32) -> f64 {
    if exponent < -308 {
        let scaled = scale_within_range(significand, -308);
        scale_within_range(scaled, exponent + 308)
    } else {
        scale_within_range(significand, exponent)
    }
}

fn scale_within_range(significand: f64, exponent: i32) -> f64 {
    if exponent < -308 {
        0.0
    } else if exponent >= 0 {
        significand * power_of_ten(exponent as usize)
    } else {
        significand / power_of_ten(exponent.unsigned_abs() as usize)
    }
}

fn power_of_ten(exponent: usize) -> f64 {
    static POWERS: OnceLock<Vec<f64>> = OnceLock::new();
    POWERS.get_or_init(|| {
        (0..=308)
            .map(|exponent| format!("1e{exponent}").parse().expect("valid literal"))
            .collect()
    })[exponent]
}
