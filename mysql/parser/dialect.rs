use crate::ddl_type::{MAX_DECIMAL_PRECISION, MAX_DECIMAL_SCALE};
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, DateTimeField, ExactNumberInfo, Expr, ExtractSyntax,
    Function, FunctionArguments, Ident, Interval, ObjectName, Statement, TrimWhereField,
    UnaryOperator,
};
use sqlparser::dialect::{Dialect, MySqlDialect, Precedence};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, Word};
use std::any::TypeId;
use std::cell::RefCell;

#[derive(Debug, Default)]
pub(crate) struct MySql {
    inner: MySqlDialect,
    rejected_cast: RefCell<Option<String>>,
}

const OR: u8 = 5;
const XOR: u8 = 7;
const AND: u8 = 10;
const NOT: u8 = 15;
const IS: u8 = 20;
const COMPARISON: u8 = 22;
const PREDICATE: u8 = 24;
const BIT_OR: u8 = 30;
const BIT_AND: u8 = 32;
const SHIFT: u8 = 34;
const ADDITION: u8 = 36;
const MULTIPLICATION: u8 = 38;
const BIT_XOR: u8 = 40;
const UNARY: u8 = 45;
const JSON_ARROW: u8 = 50;

pub(crate) const TEMPORAL_UNITS: &[&str] = &[
    "MICROSECOND",
    "SECOND",
    "MINUTE",
    "HOUR",
    "DAY",
    "WEEK",
    "MONTH",
    "QUARTER",
    "YEAR",
    "SECOND_MICROSECOND",
    "MINUTE_MICROSECOND",
    "MINUTE_SECOND",
    "HOUR_MICROSECOND",
    "HOUR_SECOND",
    "HOUR_MINUTE",
    "DAY_MICROSECOND",
    "DAY_SECOND",
    "DAY_MINUTE",
    "DAY_HOUR",
    "YEAR_MONTH",
];

/// The word after the leading `SET` of `sql`, in upper case (`SESSION` in
/// `SET SESSION TRANSACTION ...`, which the parsed statement does not keep).
pub(crate) fn word_after_set(sql: &str) -> Option<String> {
    let dialect = MySql::default();
    let tokens = Tokenizer::new(&dialect, sql).tokenize().ok()?;
    let mut words = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .map(|token| match token {
            Token::Word(word) if word.quote_style.is_none() => {
                Some(word.value.to_ascii_uppercase())
            }
            _ => None,
        });
    match words.next() {
        Some(Some(set)) if set == "SET" => words.next().flatten(),
        _ => None,
    }
}

/// The tokens of `sql`, with strings and quoted identifiers unescaped as
/// MySQL reads them.
pub(crate) fn tokens(sql: &str) -> Result<Vec<TokenWithSpan>, MysqlParserError> {
    let tokens = Tokenizer::new(&MySql::default(), sql)
        .with_unescape(false)
        .tokenize_with_location()
        .map_err(ParserError::from)?;
    Ok(tokens
        .into_iter()
        .map(|token| unescape_token(token, sql))
        .collect())
}

pub(crate) fn parse_statements(sql: &str) -> Result<Vec<Statement>, MysqlParserError> {
    let dialect = MySql::default();
    let mut tokens = tokens(sql)?;
    end_trigger_body(&mut tokens);
    parenthesize_null_patterns(&mut tokens);
    unparenthesize_single_tables(&mut tokens);
    mark_weight_string_casts(&mut tokens);
    let statements = Parser::new(&dialect)
        .with_recursion_limit(RECURSION_LIMIT)
        .with_tokens_with_locations(tokens)
        .parse_statements()?;
    if let Some(reason) = dialect.rejected_cast.take() {
        return Err(MysqlParserError::Rejected(reason));
    }
    Ok(statements)
}

/// How deeply the parser nests expressions and queries: deeper than
/// sqlparser's default of 50, which MySQL accepts in queries of the target
/// workload.
const RECURSION_LIMIT: usize = 200;

/// Writes `REGEXP NULL` and `RLIKE NULL` as `REGEXP (NULL)`: the parser reads
/// a `NULL` after `REGEXP` as the start of `NOT NULL` and then misses the
/// pattern.
fn parenthesize_null_patterns(tokens: &mut Vec<TokenWithSpan>) {
    let keyword = |token: &TokenWithSpan, keywords: &[Keyword]| matches!(&token.token, Token::Word(word) if word.quote_style.is_none() && keywords.contains(&word.keyword));
    let mut index = 0;
    while index < tokens.len() {
        if keyword(&tokens[index], &[Keyword::REGEXP, Keyword::RLIKE]) {
            let next = (index + 1..tokens.len())
                .find(|&next| !matches!(tokens[next].token, Token::Whitespace(_)));
            if let Some(next) = next.filter(|&next| keyword(&tokens[next], &[Keyword::NULL])) {
                let span = tokens[next].span;
                tokens.insert(next + 1, TokenWithSpan::new(Token::RParen, span));
                tokens.insert(next, TokenWithSpan::new(Token::LParen, span));
                index = next + 2;
            }
        }
        index += 1;
    }
}

/// Writes `FROM (t)`, `JOIN ((t AS a))` and the like as `FROM t`: MySQL
/// accepts a table in parentheses, which the parser reads as the start of
/// a join.
fn unparenthesize_single_tables(tokens: &mut Vec<TokenWithSpan>) {
    let significant = |tokens: &[TokenWithSpan], from: usize| {
        (from..tokens.len()).find(|&index| !matches!(tokens[index].token, Token::Whitespace(_)))
    };
    let word = |token: &TokenWithSpan| matches!(&token.token, Token::Word(_));
    let mut index = 0;
    while index < tokens.len() {
        let introduces = matches!(
            &tokens[index].token,
            Token::Word(word) if word.quote_style.is_none() && matches!(word.keyword, Keyword::FROM | Keyword::JOIN)
        );
        index += 1;
        if !introduces {
            continue;
        }
        let mut opening = Vec::new();
        let mut next = significant(tokens, index);
        while let Some(at) = next.filter(|&at| tokens[at].token == Token::LParen) {
            opening.push(at);
            next = significant(tokens, at + 1);
        }
        if opening.is_empty() {
            continue;
        }
        let mut table = Vec::new();
        while let Some(at) =
            next.filter(|&at| word(&tokens[at]) || tokens[at].token == Token::Period)
        {
            table.push(at);
            next = significant(tokens, at + 1);
        }
        let Some(&first) = table.first() else {
            continue;
        };
        let reserved = matches!(
            &tokens[first].token,
            Token::Word(word) if word.quote_style.is_none() && matches!(word.keyword, Keyword::SELECT | Keyword::WITH | Keyword::VALUES | Keyword::TABLE)
        );
        let shape = table
            .iter()
            .map(|&at| match &tokens[at].token {
                Token::Period => '.',
                Token::Word(word) if word.quote_style.is_none() && word.keyword == Keyword::AS => {
                    'a'
                }
                _ => 'w',
            })
            .collect::<String>();
        if reserved
            || !matches!(
                shape.as_str(),
                "w" | "w.w" | "ww" | "w.ww" | "waw" | "w.waw"
            )
        {
            continue;
        }
        let mut closing = Vec::new();
        while closing.len() < opening.len() {
            match next.filter(|&at| tokens[at].token == Token::RParen) {
                Some(at) => {
                    closing.push(at);
                    next = significant(tokens, at + 1);
                }
                None => break,
            }
        }
        if closing.len() != opening.len() {
            continue;
        }
        let mut removed = opening;
        removed.extend(closing);
        removed.sort_unstable();
        for at in removed.into_iter().rev() {
            tokens.remove(at);
        }
    }
}

/// Writes `WEIGHT_STRING(x AS BINARY(n))` and `WEIGHT_STRING(x AS CHAR(n))`,
/// which the parser does not read, as `WEIGHT_STRING(x,
/// __mysql_weight_as_binary(n))` and `WEIGHT_STRING(x,
/// __mysql_weight_as_char(n))`, for lowering to find.
fn mark_weight_string_casts(tokens: &mut [TokenWithSpan]) {
    let significant = |tokens: &[TokenWithSpan], from: usize| {
        (from..tokens.len()).find(|&index| !matches!(tokens[index].token, Token::Whitespace(_)))
    };
    let mut index = 0;
    while index < tokens.len() {
        let is_function = matches!(
            &tokens[index].token,
            Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("WEIGHT_STRING")
        );
        index += 1;
        if !is_function {
            continue;
        }
        let Some(open) = significant(tokens, index).filter(|&at| tokens[at].token == Token::LParen)
        else {
            continue;
        };
        let mut depth = 0usize;
        let mut at = open;
        while at < tokens.len() {
            match &tokens[at].token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Token::Word(word)
                    if depth == 1 && word.quote_style.is_none() && word.keyword == Keyword::AS =>
                {
                    let Some(kind) = significant(tokens, at + 1) else {
                        break;
                    };
                    let marker = match &tokens[kind].token {
                        Token::Word(word)
                            if word.quote_style.is_none() && word.keyword == Keyword::BINARY =>
                        {
                            "__mysql_weight_as_binary"
                        }
                        Token::Word(word)
                            if word.quote_style.is_none() && word.keyword == Keyword::CHAR =>
                        {
                            "__mysql_weight_as_char"
                        }
                        _ => break,
                    };
                    tokens[at].token = Token::Comma;
                    tokens[kind].token = Token::make_word(marker, None);
                    break;
                }
                _ => {}
            }
            at += 1;
        }
    }
}

/// Ends a `CREATE TRIGGER` with a `;`, which the parser expects after the
/// statement of a trigger body and the `mysql` client takes away as the end
/// of the whole statement.
fn end_trigger_body(tokens: &mut Vec<TokenWithSpan>) {
    let mut words = tokens.iter().filter_map(|token| match &token.token {
        Token::Whitespace(_) => None,
        Token::Word(word) if word.quote_style.is_none() => Some(Some(word.keyword)),
        _ => Some(None),
    });
    if words.next() != Some(Some(Keyword::CREATE)) || words.next() != Some(Some(Keyword::TRIGGER)) {
        return;
    }
    let last = tokens
        .iter()
        .rev()
        .find(|token| !matches!(token.token, Token::Whitespace(_) | Token::EOF));
    if let Some(last) = last.filter(|last| last.token != Token::SemiColon) {
        let span = last.span;
        tokens.push(TokenWithSpan::new(Token::SemiColon, span));
    }
}

fn unescape_token(token: TokenWithSpan, sql: &str) -> TokenWithSpan {
    let TokenWithSpan { token, span } = token;
    let token = match token {
        Token::SingleQuotedString(raw) => Token::SingleQuotedString(unescape_string(&raw, '\'')),
        Token::DoubleQuotedString(raw) => Token::DoubleQuotedString(unescape_string(&raw, '"')),
        Token::NationalStringLiteral(raw) => {
            Token::NationalStringLiteral(unescape_string(&raw, '\''))
        }
        Token::Word(word) => unescape_word(word),
        Token::HexStringLiteral(digits) if starts_with_zero_x(sql, span.start) => {
            if digits.is_empty() {
                Token::make_word("0x", None)
            } else if digits.len() % 2 == 1 {
                Token::HexStringLiteral(format!("0{digits}"))
            } else {
                Token::HexStringLiteral(digits)
            }
        }
        other => other,
    };
    TokenWithSpan { token, span }
}

fn unescape_word(word: Word) -> Token {
    match word.quote_style {
        Some(quote) => {
            let doubled = format!("{quote}{quote}");
            Token::Word(Word {
                value: word.value.replace(&doubled, &quote.to_string()),
                ..word
            })
        }
        None => match bit_literal_digits(&word.value) {
            Some(digits) => Token::SingleQuotedByteStringLiteral(digits.to_string()),
            None => Token::Word(word),
        },
    }
}

fn bit_literal_digits(word: &str) -> Option<&str> {
    let digits = word.strip_prefix("0b")?;
    (!digits.is_empty() && digits.bytes().all(|byte| matches!(byte, b'0' | b'1'))).then_some(digits)
}

fn starts_with_zero_x(sql: &str, start: sqlparser::tokenizer::Location) -> bool {
    let Some(line) = sql.split('\n').nth((start.line as usize).saturating_sub(1)) else {
        return false;
    };
    line.chars()
        .skip((start.column as usize).saturating_sub(1))
        .take(2)
        .eq("0x".chars())
}

fn unescape_string(raw: &str, quote: char) -> String {
    let mut value = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == quote && chars.peek() == Some(&quote) {
            chars.next();
            value.push(quote);
            continue;
        }
        if ch != '\\' {
            value.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            value.push('\\');
            break;
        };
        match escaped {
            '0' => value.push('\0'),
            'b' => value.push('\u{8}'),
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            'Z' => value.push('\u{1a}'),
            '%' | '_' => {
                value.push('\\');
                value.push(escaped);
            }
            other => value.push(other),
        }
    }
    value
}

impl Dialect for MySql {
    fn dialect(&self) -> TypeId {
        TypeId::of::<MySqlDialect>()
    }

    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        let precedence = match &parser.peek_token_ref().token {
            Token::Word(word) if word.quote_style.is_none() => match word.keyword {
                Keyword::OR => OR,
                Keyword::XOR => XOR,
                Keyword::AND => AND,
                Keyword::DIV | Keyword::MOD => MULTIPLICATION,
                Keyword::IS => IS,
                Keyword::IN
                | Keyword::BETWEEN
                | Keyword::LIKE
                | Keyword::REGEXP
                | Keyword::RLIKE
                | Keyword::MEMBER => PREDICATE,
                Keyword::NOT => match &parser.peek_nth_token_ref(1).token {
                    Token::Word(next)
                        if matches!(
                            next.keyword,
                            Keyword::IN
                                | Keyword::BETWEEN
                                | Keyword::LIKE
                                | Keyword::REGEXP
                                | Keyword::RLIKE
                                | Keyword::MEMBER
                        ) =>
                    {
                        PREDICATE
                    }
                    _ => return None,
                },
                _ => return None,
            },
            Token::StringConcat => OR,
            Token::Overlap => AND,
            Token::Eq
            | Token::DoubleEq
            | Token::Neq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::Spaceship => COMPARISON,
            Token::Pipe => BIT_OR,
            Token::Ampersand => BIT_AND,
            Token::ShiftLeft | Token::ShiftRight => SHIFT,
            Token::Plus | Token::Minus => ADDITION,
            Token::Mul | Token::Div | Token::Mod => MULTIPLICATION,
            Token::Caret => BIT_XOR,
            Token::Arrow | Token::LongArrow => JSON_ARROW,
            _ => return None,
        };
        Some(Ok(precedence))
    }

    fn prec_value(&self, precedence: Precedence) -> u8 {
        match precedence {
            Precedence::Period => 100,
            Precedence::DoubleColon => 60,
            Precedence::AtTz => 55,
            Precedence::MulDivModOp => MULTIPLICATION,
            Precedence::PlusMinus => ADDITION,
            Precedence::Xor => XOR,
            Precedence::Ampersand => BIT_AND,
            Precedence::Caret => BIT_XOR,
            Precedence::Pipe | Precedence::Colon => BIT_OR,
            Precedence::Between | Precedence::Like => PREDICATE,
            Precedence::Eq => COMPARISON,
            Precedence::Is => IS,
            Precedence::PgOther => JSON_ARROW,
            Precedence::UnaryNot => NOT,
            Precedence::And => AND,
            Precedence::Or => OR,
        }
    }

    fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>> {
        let op = match &parser.peek_token_ref().token {
            Token::ExclamationMark => UnaryOperator::BangNot,
            Token::Minus => UnaryOperator::Minus,
            Token::Plus => UnaryOperator::Plus,
            Token::Tilde => UnaryOperator::BitwiseNot,
            Token::Word(word)
                if word.quote_style.is_none() && word.keyword == Keyword::INTERVAL =>
            {
                let name = Ident::new(word.value.clone());
                parser.advance_token();
                return Some(parse_interval_function_or_interval(parser, name));
            }
            Token::Word(word)
                if word.quote_style.is_none()
                    && word.keyword == Keyword::EXTRACT
                    && parser.peek_nth_token_ref(1).token == Token::LParen =>
            {
                parser.advance_token();
                return Some(parse_extract(parser));
            }
            Token::Word(word)
                if word.quote_style.is_none()
                    && word.keyword == Keyword::TRIM
                    && parser.peek_nth_token_ref(1).token == Token::LParen =>
            {
                parser.advance_token();
                return Some(parse_trim(parser));
            }
            Token::Word(word)
                if word.quote_style.is_none()
                    && matches!(word.keyword, Keyword::CAST | Keyword::CONVERT)
                    && parser.peek_nth_token_ref(1).token == Token::LParen =>
            {
                let keyword = word.keyword;
                parser.advance_token();
                let cast = if keyword == Keyword::CAST {
                    parser.parse_cast_expr(CastKind::Cast)
                } else {
                    parser.parse_convert_expr(false)
                };
                return Some(cast.inspect(|cast| self.check_cast_target(cast)));
            }
            Token::Word(word) if word.quote_style.is_none() && word.keyword == Keyword::BINARY => {
                parser.advance_token();
                return Some(parser.parse_subexpr(UNARY).map(|expr| Expr::Cast {
                    kind: CastKind::Cast,
                    expr: Box::new(expr),
                    data_type: DataType::Binary(None),
                    array: false,
                    format: None,
                }));
            }
            _ => return None,
        };
        parser.advance_token();
        Some(parser.parse_subexpr(UNARY).map(|expr| Expr::UnaryOp {
            op,
            expr: Box::new(expr),
        }))
    }

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        let op = if parser.parse_keyword(Keyword::DIV) {
            BinaryOperator::MyIntegerDivide
        } else if parser.parse_keyword(Keyword::MOD) {
            BinaryOperator::Modulo
        } else {
            return None;
        };
        Some(
            parser
                .parse_subexpr(precedence)
                .map(|right| Expr::BinaryOp {
                    left: Box::new(expr.clone()),
                    op,
                    right: Box::new(right),
                }),
        )
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        self.inner.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        self.inner.is_identifier_part(ch)
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        self.inner.is_delimited_identifier_start(ch)
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        self.inner.identifier_quote_style(identifier)
    }

    fn supports_string_literal_backslash_escape(&self) -> bool {
        self.inner.supports_string_literal_backslash_escape()
    }

    fn supports_string_literal_concatenation(&self) -> bool {
        self.inner.supports_string_literal_concatenation()
    }

    fn ignores_wildcard_escapes(&self) -> bool {
        self.inner.ignores_wildcard_escapes()
    }

    fn supports_numeric_prefix(&self) -> bool {
        self.inner.supports_numeric_prefix()
    }

    fn supports_bitwise_shift_operators(&self) -> bool {
        self.inner.supports_bitwise_shift_operators()
    }

    fn supports_multiline_comment_hints(&self) -> bool {
        self.inner.supports_multiline_comment_hints()
    }

    fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>> {
        self.inner.parse_statement(parser)
    }

    fn require_interval_qualifier(&self) -> bool {
        self.inner.require_interval_qualifier()
    }

    fn supports_limit_comma(&self) -> bool {
        self.inner.supports_limit_comma()
    }

    fn supports_create_table_select(&self) -> bool {
        self.inner.supports_create_table_select()
    }

    fn supports_insert_set(&self) -> bool {
        self.inner.supports_insert_set()
    }

    fn supports_user_host_grantee(&self) -> bool {
        self.inner.supports_user_host_grantee()
    }

    fn is_table_factor_alias(&self, explicit: bool, kw: &Keyword, parser: &mut Parser) -> bool {
        self.inner.is_table_factor_alias(explicit, kw, parser)
    }

    fn supports_table_hints(&self) -> bool {
        self.inner.supports_table_hints()
    }

    fn requires_single_line_comment_whitespace(&self) -> bool {
        self.inner.requires_single_line_comment_whitespace()
    }

    fn supports_match_against(&self) -> bool {
        self.inner.supports_match_against()
    }

    fn supports_select_modifiers(&self) -> bool {
        self.inner.supports_select_modifiers()
    }

    fn supports_set_names(&self) -> bool {
        self.inner.supports_set_names()
    }

    fn supports_comma_separated_set_assignments(&self) -> bool {
        self.inner.supports_comma_separated_set_assignments()
    }

    fn supports_update_order_by(&self) -> bool {
        self.inner.supports_update_order_by()
    }

    fn supports_data_type_signed_suffix(&self) -> bool {
        self.inner.supports_data_type_signed_suffix()
    }

    fn supports_cross_join_constraint(&self) -> bool {
        self.inner.supports_cross_join_constraint()
    }

    fn supports_double_ampersand_operator(&self) -> bool {
        self.inner.supports_double_ampersand_operator()
    }

    fn supports_binary_kw_as_cast(&self) -> bool {
        self.inner.supports_binary_kw_as_cast()
    }

    fn supports_comment_optimizer_hint(&self) -> bool {
        self.inner.supports_comment_optimizer_hint()
    }

    fn supports_constraint_keyword_without_name(&self) -> bool {
        self.inner.supports_constraint_keyword_without_name()
    }

    fn supports_key_column_option(&self) -> bool {
        self.inner.supports_key_column_option()
    }
}

impl MySql {
    fn check_cast_target(&self, cast: &Expr) {
        let data_type = match cast {
            Expr::Cast { data_type, .. } => data_type,
            Expr::Convert {
                data_type: Some(data_type),
                ..
            } => data_type,
            _ => return,
        };
        let reason = match data_type {
            DataType::Decimal(info) | DataType::Dec(info) => invalid_cast_decimal(info),
            DataType::Datetime(Some(precision)) | DataType::Time(Some(precision), _)
                if *precision > MAX_TEMPORAL_PRECISION =>
            {
                Some(format!(
                    "{data_type} has more than {MAX_TEMPORAL_PRECISION} fractional digits"
                ))
            }
            _ => None,
        };
        if let Some(reason) = reason {
            self.rejected_cast.borrow_mut().get_or_insert(reason);
        }
    }
}

pub(crate) const MAX_TEMPORAL_PRECISION: u64 = 6;

fn invalid_cast_decimal(info: &ExactNumberInfo) -> Option<String> {
    let (precision, scale) = match *info {
        ExactNumberInfo::None => return None,
        ExactNumberInfo::Precision(precision) => (precision, 0),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => match u64::try_from(scale) {
            Ok(scale) => (precision, scale),
            Err(_) => return Some(format!("DECIMAL{info} has a negative scale")),
        },
    };
    if precision < scale {
        Some(format!("DECIMAL{info} has fewer digits than decimals"))
    } else if precision > MAX_DECIMAL_PRECISION {
        Some(format!(
            "DECIMAL{info} has more than {MAX_DECIMAL_PRECISION} digits"
        ))
    } else if scale > MAX_DECIMAL_SCALE {
        Some(format!(
            "DECIMAL{info} has more than {MAX_DECIMAL_SCALE} decimals"
        ))
    } else {
        None
    }
}

fn parse_interval_function_or_interval(
    parser: &mut Parser,
    name: Ident,
) -> Result<Expr, ParserError> {
    if parser.peek_token_ref().token == Token::LParen {
        let function = parser.maybe_parse(|parser| {
            let function = parser.parse_function(ObjectName::from(vec![name.clone()]))?;
            let Expr::Function(Function {
                args: FunctionArguments::List(list),
                ..
            }) = &function
            else {
                return parser.expected("INTERVAL(N, N1, ...)", parser.peek_token());
            };
            if list.args.len() < 2 || is_temporal_unit(&parser.peek_token_ref().token) {
                return parser.expected("INTERVAL(N, N1, ...)", parser.peek_token());
            }
            Ok(function)
        })?;
        if let Some(function) = function {
            return Ok(function);
        }
    }
    parse_interval(parser)
}

fn is_temporal_unit(token: &Token) -> bool {
    matches!(
        token,
        Token::Word(word)
            if word.quote_style.is_none()
                && TEMPORAL_UNITS.contains(&word.value.to_ascii_uppercase().as_str())
    )
}

fn parse_interval(parser: &mut Parser) -> Result<Expr, ParserError> {
    let value = parser.parse_expr()?;
    let unit = parse_temporal_unit(parser)?;
    Ok(Expr::Interval(Interval {
        value: Box::new(value),
        leading_field: Some(unit),
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    }))
}

fn parse_extract(parser: &mut Parser) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let field = parse_temporal_unit(parser)?;
    parser.expect_keyword_is(Keyword::FROM)?;
    let expr = parser.parse_expr()?;
    parser.expect_token(&Token::RParen)?;
    Ok(Expr::Extract {
        field,
        syntax: ExtractSyntax::From,
        expr: Box::new(expr),
    })
}

fn parse_temporal_unit(parser: &mut Parser) -> Result<DateTimeField, ParserError> {
    let token = parser.next_token();
    match &token.token {
        Token::Word(word) if word.quote_style.is_none() => {
            let unit = word.value.to_ascii_uppercase();
            if TEMPORAL_UNITS.contains(&unit.as_str()) {
                return Ok(DateTimeField::Custom(Ident::new(unit)));
            }
            parser.expected("a MySQL temporal unit", token)
        }
        _ => parser.expected("a MySQL temporal unit", token),
    }
}

fn parse_trim(parser: &mut Parser) -> Result<Expr, ParserError> {
    parser.expect_token(&Token::LParen)?;
    let trim_where =
        match parser.parse_one_of_keywords(&[Keyword::BOTH, Keyword::LEADING, Keyword::TRAILING]) {
            Some(Keyword::BOTH) => Some(TrimWhereField::Both),
            Some(Keyword::LEADING) => Some(TrimWhereField::Leading),
            Some(Keyword::TRAILING) => Some(TrimWhereField::Trailing),
            _ => None,
        };
    let (trim_what, expr) = if trim_where.is_some() && parser.parse_keyword(Keyword::FROM) {
        (None, parser.parse_expr()?)
    } else {
        let first = parser.parse_expr()?;
        if parser.parse_keyword(Keyword::FROM) {
            (Some(Box::new(first)), parser.parse_expr()?)
        } else if trim_where.is_some() {
            return parser.expected("FROM", parser.peek_token());
        } else {
            (None, first)
        }
    };
    parser.expect_token(&Token::RParen)?;
    Ok(Expr::Trim {
        expr: Box::new(expr),
        trim_where,
        trim_what,
        trim_characters: None,
    })
}
