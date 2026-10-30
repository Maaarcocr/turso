//! MySQL's `TABLE t [ORDER BY ...] [LIMIT ...]`, which is `SELECT * FROM t`,
//! alone or as an operand of `UNION`, `EXCEPT` or `INTERSECT`.

use crate::dialect::tokens;
use crate::prepared::byte_offset;
use crate::MysqlParserError;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, TokenWithSpan};

/// `sql`, a statement that starts with `TABLE`, with each `TABLE t` operand
/// written as `SELECT * FROM t`. Refused: a `TABLE` operand followed by
/// anything but the end, `ORDER BY`, `LIMIT`, a set operator or `)`, which
/// MySQL rejects.
pub(crate) fn as_select(sql: &str) -> Result<String, MysqlParserError> {
    let tokens = tokens(sql)?
        .into_iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let keyword = |token: Option<&TokenWithSpan>, keywords: &[Keyword]| {
        matches!(
            token.map(|token| &token.token),
            Some(Token::Word(word)) if word.quote_style.is_none() && keywords.contains(&word.keyword)
        )
    };
    let mut operands = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !keyword(Some(token), &[Keyword::TABLE]) {
            continue;
        }
        let previous = index
            .checked_sub(1)
            .and_then(|previous| tokens.get(previous));
        let starts_query = previous.is_none()
            || matches!(previous.map(|token| &token.token), Some(Token::LParen))
            || keyword(
                previous,
                &[
                    Keyword::UNION,
                    Keyword::EXCEPT,
                    Keyword::INTERSECT,
                    Keyword::ALL,
                    Keyword::DISTINCT,
                ],
            );
        if !starts_query {
            return Err(unsupported(sql));
        }
        let mut next = index + 1;
        loop {
            if !matches!(
                tokens.get(next).map(|token| &token.token),
                Some(Token::Word(_))
            ) {
                return Err(unsupported(sql));
            }
            next += 1;
            if !matches!(
                tokens.get(next).map(|token| &token.token),
                Some(Token::Period)
            ) {
                break;
            }
            next += 1;
        }
        let follows = tokens.get(next);
        let ends = matches!(
            follows.map(|token| &token.token),
            None | Some(Token::EOF | Token::SemiColon | Token::RParen)
        ) || keyword(
            follows,
            &[
                Keyword::ORDER,
                Keyword::LIMIT,
                Keyword::UNION,
                Keyword::EXCEPT,
                Keyword::INTERSECT,
            ],
        );
        if !ends {
            return Err(unsupported(sql));
        }
        operands.push(byte_offset(sql, token.span.start));
    }
    let mut select = String::with_capacity(sql.len() + operands.len() * 8);
    let mut copied = 0;
    for offset in operands {
        assert!(
            sql[offset..]
                .get(..5)
                .is_some_and(|word| word.eq_ignore_ascii_case("TABLE")),
            "a TABLE operand starts with TABLE"
        );
        select.push_str(&sql[copied..offset]);
        select.push_str("SELECT * FROM");
        copied = offset + 5;
    }
    select.push_str(&sql[copied..]);
    Ok(select)
}

fn unsupported(sql: &str) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("TABLE statement `{sql}` is not lowered yet"))
}
