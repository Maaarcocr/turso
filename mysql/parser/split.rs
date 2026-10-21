//! Splitting MySQL text into statements.
//!
//! Statements end at `;` tokens of the sqlparser-rs MySQL tokenizer, so
//! semicolons inside `'...'`, `"..."` and `` `...` `` (including backslash
//! escapes) and inside `-- `, `#` and `/* */` comments do not split. Like the
//! `mysql` client with its default delimiter, every top-level semicolon ends a
//! statement; `DELIMITER` and compound-statement bodies are not supported.

use crate::MysqlParserError;
use sqlparser::dialect::MySqlDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer, Whitespace};
use std::ops::Range;

/// One statement found in MySQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// Byte range of the statement text, without the surrounding whitespace
    /// and comments or the terminating semicolon.
    pub text: Range<usize>,
    /// Byte offset just past the terminating semicolon, or the input length
    /// when the last statement has no semicolon.
    pub end: usize,
}

/// Splits MySQL text into the byte ranges of its statements.
///
/// Empty statements (`;;`) and whitespace or comment-only input yield no
/// ranges. A conditional comment such as `/*!40101 SET NAMES utf8 */` is
/// statement text, because MySQL executes it.
pub fn split_statements(sql: &str) -> Result<Vec<Range<usize>>, MysqlParserError> {
    let (tokens, error) = tokenize(sql);
    if let Some(error) = error {
        return Err(error);
    }
    let mut statements = Vec::new();
    for_each_statement(sql, &tokens, true, |statement| {
        statements.push(statement.text);
        true
    })?;
    Ok(statements)
}

/// Locates the first statement in MySQL text.
///
/// Returns `None` when the text contains no statement. Text after the first
/// statement's semicolon is not required to be valid: a tokenizer error there
/// belongs to a later statement.
pub fn first_statement(sql: &str) -> Result<Option<StatementSpan>, MysqlParserError> {
    let (tokens, error) = tokenize(sql);
    // On a tokenizer error the tokens are the complete prefix before it, so a
    // statement that ended at a semicolon among them is unaffected.
    let mut first = None;
    for_each_statement(sql, &tokens, error.is_none(), |statement| {
        first = Some(statement);
        false
    })?;
    match (first, error) {
        (Some(first), _) => Ok(Some(first)),
        (None, Some(error)) => Err(error),
        (None, None) => Ok(None),
    }
}

/// Tokenizes `sql` with the MySQL dialect. On a tokenizer error the returned
/// tokens are those before the error.
///
/// The tokenizer expands a `/*!...*/` conditional comment into the tokens of
/// its content, whose spans do not point at the original text. To keep every
/// span exact the tokenizer reads a copy in which each `/*!` reads `/* `, so
/// conditional comments stay whole comment tokens; [`is_significant`] then
/// recognizes them from the original text. The copy has the same length. A
/// `/*!` that is not a comment start lies inside a string, identifier or
/// comment, or overlaps the `/` of a preceding `*/` and only turns a `!`
/// operator into a space, so the copy has the same string, identifier,
/// comment and semicolon boundaries.
fn tokenize(sql: &str) -> (Vec<TokenWithSpan>, Option<MysqlParserError>) {
    let text = sql.replace("/*!", "/* ");
    let mut tokens = Vec::new();
    let result =
        Tokenizer::new(&MySqlDialect {}, &text).tokenize_with_location_into_buf(&mut tokens);
    (tokens, result.err().map(MysqlParserError::from))
}

/// Calls `visit` with each non-empty statement in order until it returns
/// false. A final statement without a semicolon is visited only when the
/// tokens are `complete`, that is, they reach the end of the input.
fn for_each_statement(
    sql: &str,
    tokens: &[TokenWithSpan],
    complete: bool,
    mut visit: impl FnMut(StatementSpan) -> bool,
) -> Result<(), MysqlParserError> {
    let mut cursor = ByteCursor::new(sql);
    let mut text: Option<Range<usize>> = None;
    for token in tokens {
        let range = cursor.advance(token)?;
        if token.token == Token::SemiColon {
            if let Some(text) = text.take() {
                if !visit(StatementSpan {
                    text,
                    end: range.end,
                }) {
                    return Ok(());
                }
            }
        } else if is_significant(sql, &token.token, &range) {
            match &mut text {
                Some(text) => text.end = range.end,
                None => text = Some(range),
            }
        }
    }
    if let Some(text) = text.filter(|_| complete) {
        visit(StatementSpan {
            text,
            end: sql.len(),
        });
    }
    Ok(())
}

/// Whether a token is statement text rather than whitespace or a comment.
fn is_significant(sql: &str, token: &Token, range: &Range<usize>) -> bool {
    match token {
        Token::Whitespace(Whitespace::MultiLineComment(_)) => {
            // Same content rule as the tokenizer's conditional comment
            // expansion: skip `!` and the version digits.
            sql[range.clone()]
                .strip_prefix("/*!")
                .and_then(|comment| comment.strip_suffix("*/"))
                .is_some_and(|content| {
                    !content
                        .trim_start_matches(|c: char| c.is_ascii_digit())
                        .trim()
                        .is_empty()
                })
        }
        Token::Whitespace(_) => false,
        _ => true,
    }
}

/// Converts the tokenizer's line/column locations into byte offsets by
/// replaying its location rules (columns count characters; only `\n` starts
/// a new line) over the text. Tokens cover the input contiguously, so one
/// forward pass suffices.
struct ByteCursor<'a> {
    chars: std::str::Chars<'a>,
    location: Location,
    offset: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(sql: &'a str) -> Self {
        Self {
            chars: sql.chars(),
            location: Location::new(1, 1),
            offset: 0,
        }
    }

    /// Returns the byte range of the next token.
    fn advance(&mut self, token: &TokenWithSpan) -> Result<Range<usize>, MysqlParserError> {
        if token.span.start != self.location {
            return Err(MysqlParserError::Split(format!(
                "token `{}` starts at {:?}, expected {:?}",
                token.token, token.span.start, self.location
            )));
        }
        let start = self.offset;
        while self.location < token.span.end {
            let Some(ch) = self.chars.next() else {
                return Err(MysqlParserError::Split(format!(
                    "token `{}` ends at {:?}, past the end of the input",
                    token.token, token.span.end
                )));
            };
            self.offset += ch.len_utf8();
            if ch == '\n' {
                self.location = Location::new(self.location.line + 1, 1);
            } else {
                self.location = Location::new(self.location.line, self.location.column + 1);
            }
        }
        if self.location != token.span.end {
            return Err(MysqlParserError::Split(format!(
                "token `{}` ends at {:?}, which is not a character boundary",
                token.token, token.span.end
            )));
        }
        Ok(start..self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split_statements(sql)
            .unwrap_or_else(|error| panic!("{sql:?} must split: {error}"))
            .into_iter()
            .map(|range| &sql[range])
            .collect()
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        assert_eq!(texts("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(texts("SELECT 1;\nSELECT 2;\n"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(texts("  SELECT 1  "), ["SELECT 1"]);
    }

    #[test]
    fn ignores_semicolons_in_strings_and_identifiers() {
        assert_eq!(
            texts(r#"SELECT 'a;b'; SELECT "c;d"; SELECT `e;f`"#),
            [r#"SELECT 'a;b'"#, r#"SELECT "c;d""#, "SELECT `e;f`"]
        );
        assert_eq!(
            texts(r#"SELECT 'it\'s;'; SELECT "say \";\""; SELECT 'x'';y'"#),
            [
                r#"SELECT 'it\'s;'"#,
                r#"SELECT "say \";\"""#,
                "SELECT 'x'';y'"
            ]
        );
        assert_eq!(
            texts(r"SELECT 'a\\'; SELECT 2"),
            [r"SELECT 'a\\'", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT `a``;b`; SELECT 2"),
            ["SELECT `a``;b`", "SELECT 2"]
        );
    }

    #[test]
    fn ignores_semicolons_in_comments_and_trims_them() {
        assert_eq!(
            texts("-- one;\nSELECT 1 # two;\n; /* three; */ SELECT 2 /* tail */;"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT 1 /* inner; */ + 2; SELECT 3"),
            ["SELECT 1 /* inner; */ + 2", "SELECT 3"]
        );
    }

    #[test]
    fn double_dash_needs_whitespace_to_start_a_comment() {
        assert_eq!(texts("SELECT 1--1; SELECT 2"), ["SELECT 1--1", "SELECT 2"]);
        assert_eq!(
            texts("SELECT 1 --\tc;\n;SELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn empty_input_and_empty_statements_yield_nothing() {
        for sql in [
            "",
            "   \n\t",
            ";",
            ";;  ;",
            "-- only a comment",
            "# hash comment\n",
            "/* block */ /* another */",
            "/*!*/",
            "/*!40101 */;",
        ] {
            assert!(texts(sql).is_empty(), "{sql:?}");
            assert_eq!(first_statement(sql).unwrap(), None, "{sql:?}");
        }
        assert_eq!(texts(";;SELECT 1;;"), ["SELECT 1"]);
    }

    #[test]
    fn conditional_comments_are_statement_text() {
        let sql = "/*!40101 SET NAMES utf8mb4 */; SELECT /*!50000 1 */ + 1;";
        assert_eq!(
            texts(sql),
            ["/*!40101 SET NAMES utf8mb4 */", "SELECT /*!50000 1 */ + 1"]
        );
        assert_eq!(texts("/*!40101 a; b */"), ["/*!40101 a; b */"]);
    }

    #[test]
    fn byte_offsets_are_exact_for_multibyte_text() {
        let sql = "SELECT 'Grüße 世界 🦀';\r\n  SELECT `🦀`, \"é\" -- ü;\n;SELECT 3";
        assert_eq!(
            texts(sql),
            ["SELECT 'Grüße 世界 🦀'", "SELECT `🦀`, \"é\"", "SELECT 3"]
        );
        let ranges = split_statements(sql).unwrap();
        assert_eq!(ranges[0], 0.."SELECT 'Grüße 世界 🦀'".len());
        assert_eq!(&sql[ranges[2].end..], "");
    }

    #[test]
    fn first_statement_reports_the_consumed_offset() {
        let sql = "  SELECT 1 ;  SELECT 2";
        assert_eq!(
            first_statement(sql).unwrap(),
            Some(StatementSpan {
                text: 2..10,
                end: 12
            })
        );
        assert_eq!(
            first_statement("SELECT 1 -- done").unwrap(),
            Some(StatementSpan {
                text: 0..8,
                end: 16
            })
        );
        assert_eq!(
            first_statement(";; SELECT 1;").unwrap(),
            Some(StatementSpan {
                text: 3..11,
                end: 12
            })
        );
    }

    #[test]
    fn tokenizer_errors_after_the_first_statement_do_not_affect_it() {
        assert_eq!(
            first_statement("SELECT 1; SELECT 'unterminated").unwrap(),
            Some(StatementSpan { text: 0..8, end: 9 })
        );
        assert!(first_statement("SELECT 'unterminated; SELECT 1").is_err());
        assert!(first_statement("SELECT 1 /* open").is_err());
        assert!(split_statements("SELECT 1; SELECT 'unterminated").is_err());
    }
}
