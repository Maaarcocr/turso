use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, ObjectNamePart, UnaryOperator, Value,
};

/// A literal MySQL rejects when it evaluates it.
#[derive(Clone, Copy)]
pub(crate) enum Failure<'a> {
    /// `CAST(text AS JSON)` of a string that is not JSON (3141).
    Json(&'a str),
    /// A negated DOUBLE literal `-digits` below the BIGINT range, read as a
    /// BIGINT by a bit operation or `CHAR` (1690).
    BigintOutOfRange(&'a str),
    /// `x REGEXP ''` of a text subject that is not `NULL` (3685).
    EmptyRegexp,
}

/// Whether `expr` is `x [NOT] REGEXP ''`.
pub(crate) fn is_empty_regexp(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_empty_regexp(inner),
        Expr::RLike { pattern, .. } => string_literal(pattern).is_some_and(str::is_empty),
        _ => false,
    }
}

/// The failure that MySQL meets whenever it evaluates `expr`, reading
/// operands in its order and skipping those it does not read (after a
/// `NULL` operand of most operators, or a decided `AND` or `OR`).
pub(crate) fn failure_evaluated(expr: &Expr) -> Option<Failure<'_>> {
    match outcome(expr)? {
        Outcome::Fails(failure) => Some(failure),
        Outcome::Null | Outcome::NotNull | Outcome::MaybeNull => None,
    }
}

#[derive(Clone, Copy)]
enum Outcome<'a> {
    Null,
    NotNull,
    MaybeNull,
    Fails(Failure<'a>),
}

impl<'a> Outcome<'a> {
    fn failure(self) -> Option<Failure<'a>> {
        match self {
            Outcome::Fails(text) => Some(text),
            Outcome::Null | Outcome::NotNull | Outcome::MaybeNull => None,
        }
    }

    fn is_not_null(self) -> bool {
        matches!(self, Outcome::NotNull | Outcome::Fails(_))
    }
}

fn outcome(expr: &Expr) -> Option<Outcome<'_>> {
    Some(match expr {
        Expr::Value(value) => match &value.value {
            Value::Null => Outcome::Null,
            Value::Number(..)
            | Value::SingleQuotedString(_)
            | Value::DoubleQuotedString(_)
            | Value::NationalStringLiteral(_)
            | Value::HexStringLiteral(_)
            | Value::SingleQuotedByteStringLiteral(_)
            | Value::Boolean(_) => Outcome::NotNull,
            _ => return None,
        },
        Expr::Prefixed { value, .. } => {
            outcome(value)?;
            Outcome::NotNull
        }
        Expr::TypedString(_) => Outcome::NotNull,
        Expr::Nested(inner) => outcome(inner)?,
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            array: false,
            format: None,
        } => cast_outcome(expr, data_type)?,
        Expr::Convert {
            is_try: false,
            expr,
            data_type: Some(data_type),
            charset: None,
            target_before_value: false,
            styles,
        } if styles.is_empty() => cast_outcome(expr, data_type)?,
        Expr::UnaryOp {
            op: UnaryOperator::BitwiseNot,
            expr,
        } => bit_operand_outcome(expr)?,
        Expr::UnaryOp {
            op:
                UnaryOperator::Minus | UnaryOperator::Plus | UnaryOperator::Not | UnaryOperator::BangNot,
            expr,
        } => outcome(expr)?,
        Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner) => first_failure_or(outcome(inner)?, Outcome::NotNull),
        Expr::BinaryOp { left, op, right } => binary_outcome(left, op, right)?,
        Expr::Like {
            any: false,
            expr,
            pattern,
            escape_char,
            ..
        } if escape_char.as_ref().is_none_or(|escape| {
            matches!(
                escape.value,
                Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
            )
        }) =>
        {
            in_order(outcome(expr)?, outcome(pattern)?)
        }
        Expr::RLike { expr, pattern, .. } => {
            let subject = outcome(expr)?;
            if string_literal(pattern).is_some_and(str::is_empty)
                && matches!(subject, Outcome::NotNull)
                && !is_binary_literal(expr)
            {
                Outcome::Fails(Failure::EmptyRegexp)
            } else {
                in_order(outcome(pattern)?, subject)
            }
        }
        Expr::InList { expr, list, .. } => {
            let tested = outcome(expr)?;
            for item in list {
                outcome(item)?;
            }
            first_failure_or(tested, Outcome::MaybeNull)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let tested = outcome(expr)?;
            outcome(low)?;
            outcome(high)?;
            first_failure_or(tested, Outcome::MaybeNull)
        }
        Expr::Function(function) => function_outcome(function)?,
        _ => return None,
    })
}

fn cast_outcome<'a>(operand: &'a Expr, data_type: &DataType) -> Option<Outcome<'a>> {
    let value = outcome(operand)?;
    if value.failure().is_some() || *data_type != DataType::JSON {
        return Some(first_failure_or(value, Outcome::MaybeNull));
    }
    Some(match string_literal(operand) {
        Some(text) if mysql_rejects_json_text(text) => Outcome::Fails(Failure::Json(text)),
        _ => value,
    })
}

/// The outcome of `expr` read as a BIGINT by a bit operation or `CHAR`.
fn bit_operand_outcome(expr: &Expr) -> Option<Outcome<'_>> {
    match negated_double_beyond_bigint(expr) {
        Some(digits) => Some(Outcome::Fails(Failure::BigintOutOfRange(digits))),
        None => outcome(expr),
    }
}

/// The digits of a negated DOUBLE literal below the BIGINT range.
fn negated_double_beyond_bigint(expr: &Expr) -> Option<&str> {
    let Expr::UnaryOp {
        op: UnaryOperator::Minus,
        expr: operand,
    } = expr
    else {
        return match expr {
            Expr::Nested(inner) => negated_double_beyond_bigint(inner),
            _ => None,
        };
    };
    let mut operand = operand.as_ref();
    while let Expr::Nested(inner) = operand {
        operand = inner;
    }
    let Expr::Value(value) = operand else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    (digits.contains(['e', 'E'])
        && digits
            .parse::<f64>()
            .is_ok_and(|magnitude| magnitude > 9_223_372_036_854_775_808f64))
    .then_some(digits.as_str())
}

fn first_failure_or<'a>(value: Outcome<'a>, otherwise: Outcome<'a>) -> Outcome<'a> {
    match value {
        Outcome::Fails(failure) => Outcome::Fails(failure),
        Outcome::Null | Outcome::NotNull | Outcome::MaybeNull => otherwise,
    }
}

fn in_order<'a>(first: Outcome<'a>, second: Outcome<'a>) -> Outcome<'a> {
    match (first, second) {
        (Outcome::Fails(failure), _) => Outcome::Fails(failure),
        (Outcome::NotNull, Outcome::Fails(failure)) => Outcome::Fails(failure),
        _ => Outcome::MaybeNull,
    }
}

fn binary_outcome<'a>(left: &'a Expr, op: &BinaryOperator, right: &'a Expr) -> Option<Outcome<'a>> {
    let bitwise = matches!(
        op,
        BinaryOperator::BitwiseOr
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseXor
            | BinaryOperator::PGBitwiseShiftLeft
            | BinaryOperator::PGBitwiseShiftRight
    );
    let (lhs, rhs) = if bitwise {
        (bit_operand_outcome(left)?, bit_operand_outcome(right)?)
    } else {
        (outcome(left)?, outcome(right)?)
    };
    let right_is_evaluated = match op {
        BinaryOperator::And => matches!(lhs, Outcome::Null) || literal_truth(left) == Some(true),
        BinaryOperator::Or | BinaryOperator::StringConcat => {
            matches!(lhs, Outcome::Null) || literal_truth(left) == Some(false)
        }
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => {
            lhs.is_not_null() || matches!(rhs.failure(), Some(Failure::Json(_)))
        }
        BinaryOperator::MyIntegerDivide
        | BinaryOperator::BitwiseOr
        | BinaryOperator::BitwiseAnd
        | BinaryOperator::BitwiseXor
        | BinaryOperator::PGBitwiseShiftLeft
        | BinaryOperator::PGBitwiseShiftRight
        | BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::Spaceship
        | BinaryOperator::Xor => lhs.is_not_null(),
        _ => return None,
    };
    Some(match (lhs.failure(), rhs.failure()) {
        (Some(failure), _) => Outcome::Fails(failure),
        (None, Some(failure)) if right_is_evaluated => Outcome::Fails(failure),
        (None, Some(_)) => Outcome::MaybeNull,
        (None, None) if never_null(op, lhs, rhs) => Outcome::NotNull,
        (None, None) => Outcome::MaybeNull,
    })
}

fn never_null(op: &BinaryOperator, lhs: Outcome, rhs: Outcome) -> bool {
    match op {
        BinaryOperator::Spaceship => true,
        BinaryOperator::Divide | BinaryOperator::Modulo | BinaryOperator::MyIntegerDivide => false,
        _ => lhs.is_not_null() && rhs.is_not_null(),
    }
}

fn literal_truth(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) => number.parse::<f64>().ok().map(|value| value != 0.0),
            Value::Boolean(truth) => Some(*truth),
            _ => None,
        },
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => literal_truth(inner),
        _ => None,
    }
}

fn function_outcome(function: &Function) -> Option<Outcome<'_>> {
    let Function {
        name,
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(list),
        filter: None,
        null_treatment: None,
        over: None,
        within_group,
    } = function
    else {
        return None;
    };
    if !within_group.is_empty() || list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return None;
    }
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return None;
    };
    if identifier.quote_style.is_some() {
        return None;
    }
    let upper_name = identifier.value.to_ascii_uppercase();
    let args = list
        .args
        .iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) if upper_name == "CHAR" => {
                bit_operand_outcome(expr)
            }
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => outcome(expr),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let Some((position, failure)) = args
        .iter()
        .enumerate()
        .find_map(|(position, arg)| arg.failure().map(|failure| (position, failure)))
    else {
        return Some(Outcome::MaybeNull);
    };
    let evaluated = match upper_name.as_str() {
        "CHAR" | "INTERVAL" => true,
        "CONCAT_WS" => position == 0 || args[0].is_not_null(),
        "COALESCE" | "ELT" | "FIELD" | "IF" | "IFNULL" | "MAKE_SET" | "NULLIF" => false,
        _ => args
            .iter()
            .enumerate()
            .all(|(other, arg)| other == position || arg.is_not_null()),
    };
    Some(if evaluated {
        Outcome::Fails(failure)
    } else {
        Outcome::MaybeNull
    })
}

fn is_binary_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_)
        ),
        Expr::Prefixed { prefix, .. } => prefix.value.eq_ignore_ascii_case("_binary"),
        Expr::Nested(inner) => is_binary_literal(inner),
        _ => false,
    }
}

fn string_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text)
            | Value::DoubleQuotedString(text)
            | Value::NationalStringLiteral(text) => Some(text),
            _ => None,
        },
        Expr::Nested(inner) => string_literal(inner),
        _ => None,
    }
}

const MAXIMUM_DEPTH: usize = 100;

pub(crate) fn mysql_rejects_json_text(text: &str) -> bool {
    if text.contains('\0') {
        return false;
    }
    let mut reader = JsonText {
        bytes: text.as_bytes(),
        position: 0,
        depth: 0,
    };
    reader.skip_whitespace();
    let valid = reader.value() && {
        reader.skip_whitespace();
        reader.position == reader.bytes.len()
    };
    !valid
}

struct JsonText<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}

impl JsonText<'_> {
    fn value(&mut self) -> bool {
        match self.peek() {
            Some(b'n') => self.word(b"null"),
            Some(b't') => self.word(b"true"),
            Some(b'f') => self.word(b"false"),
            Some(b'"') => self.string(),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            _ => self.number(),
        }
    }

    fn word(&mut self, word: &[u8]) -> bool {
        if self.bytes[self.position..].starts_with(word) {
            self.position += word.len();
            true
        } else {
            false
        }
    }

    fn object(&mut self) -> bool {
        if !self.enter() {
            return false;
        }
        self.skip_whitespace();
        if self.consume(b'}') {
            self.depth -= 1;
            return true;
        }
        loop {
            if self.peek() != Some(b'"') || !self.string() {
                return false;
            }
            self.skip_whitespace();
            if !self.consume(b':') {
                return false;
            }
            self.skip_whitespace();
            if !self.value() {
                return false;
            }
            self.skip_whitespace();
            if self.consume(b',') {
                self.skip_whitespace();
            } else if self.consume(b'}') {
                self.depth -= 1;
                return true;
            } else {
                return false;
            }
        }
    }

    fn array(&mut self) -> bool {
        if !self.enter() {
            return false;
        }
        self.skip_whitespace();
        if self.consume(b']') {
            self.depth -= 1;
            return true;
        }
        loop {
            if !self.value() {
                return false;
            }
            self.skip_whitespace();
            if self.consume(b',') {
                self.skip_whitespace();
            } else if self.consume(b']') {
                self.depth -= 1;
                return true;
            } else {
                return false;
            }
        }
    }

    fn enter(&mut self) -> bool {
        self.position += 1;
        self.depth += 1;
        self.depth <= MAXIMUM_DEPTH
    }

    fn string(&mut self) -> bool {
        self.position += 1;
        loop {
            match self.next() {
                Some(b'"') => return true,
                Some(b'\\') => {
                    if !self.escape() {
                        return false;
                    }
                }
                Some(byte) if byte >= 0x20 => {}
                _ => return false,
            }
        }
    }

    fn escape(&mut self) -> bool {
        match self.next() {
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => true,
            Some(b'u') => match self.hex4() {
                Some(0xD800..=0xDBFF) => {
                    self.consume(b'\\')
                        && self.consume(b'u')
                        && matches!(self.hex4(), Some(0xDC00..=0xDFFF))
                }
                Some(0xDC00..=0xDFFF) | None => false,
                Some(_) => true,
            },
            _ => false,
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut code_point = 0;
        for _ in 0..4 {
            let digit = (self.next()? as char).to_digit(16)?;
            code_point = code_point * 16 + digit;
        }
        Some(code_point)
    }

    fn number(&mut self) -> bool {
        self.consume(b'-');
        if !self.consume(b'0') && self.digits() == 0 {
            return false;
        }
        if self.consume(b'.') && self.digits() == 0 {
            return false;
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            if self.digits() == 0 {
                return false;
            }
        }
        true
    }

    fn digits(&mut self) -> usize {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        self.position - start
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::mysql_rejects_json_text;

    #[test]
    fn rejects_json_text_like_mysql() {
        let valid = [
            "1",
            " -0.5e+10 ",
            "\"a\\u00e9\\ud83d\\ude00\"",
            "[1, {\"a\": [true, false, null]}]",
            "{}",
            "[]",
            "\t\"x\"\n",
        ];
        let invalid = [
            "",
            "   ",
            "invalid",
            "\"a\": 1",
            " \"b\": 2}",
            "01",
            "1.",
            ".5",
            "1e",
            "-",
            "+1",
            "[1,]",
            "{\"a\":1,}",
            "{a:1}",
            "\"unterminated",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\u12\"",
            "\"tab\tinside\"",
            "nul",
            "truex",
            "[1] 2",
            "2000-01-01",
            "10:04:58",
        ];
        for text in valid {
            assert!(!mysql_rejects_json_text(text), "{text:?} is valid JSON");
        }
        for text in invalid {
            assert!(mysql_rejects_json_text(text), "{text:?} is invalid JSON");
        }
        let deep = format!("{}{}", "[".repeat(100), "]".repeat(100));
        assert!(!mysql_rejects_json_text(&deep));
        let too_deep = format!("{}{}", "[".repeat(101), "]".repeat(101));
        assert!(mysql_rejects_json_text(&too_deep));
    }
}
