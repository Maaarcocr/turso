use turso_core::{LimboError, Result, Value};

use super::convert::{string, Str};

pub(super) fn like_bin(args: &[Value]) -> Result<Value> {
    let (Some(subject), Some(pattern), Some(escape)) =
        (string(&args[0]), string(&args[1]), string(&args[2]))
    else {
        return Ok(Value::Null);
    };
    let matched = match (&subject, &pattern, &escape) {
        (Str::Text(subject), Str::Text(pattern), Str::Text(escape)) => {
            let subject: Vec<char> = subject.chars().collect();
            let pattern: Vec<char> = pattern.chars().collect();
            matches(&subject, &tokens(&pattern, single(escape.chars())?))
        }
        _ => matches(
            subject.as_bytes(),
            &tokens(
                pattern.as_bytes(),
                single(escape.as_bytes().iter().copied())?,
            ),
        ),
    };
    Ok(Value::from_i64(i64::from(matched)))
}

fn single<T>(mut items: impl Iterator<Item = T>) -> Result<T> {
    match (items.next(), items.next()) {
        (Some(item), None) => Ok(item),
        _ => Err(LimboError::InvalidArgument(
            "Incorrect arguments to ESCAPE".to_string(),
        )),
    }
}

enum Token<T> {
    Exactly(T),
    AnyOne,
    AnyMany,
}

trait Wildcard: Copy + PartialEq {
    const ANY_ONE: Self;
    const ANY_MANY: Self;
}

impl Wildcard for char {
    const ANY_ONE: char = '_';
    const ANY_MANY: char = '%';
}

impl Wildcard for u8 {
    const ANY_ONE: u8 = b'_';
    const ANY_MANY: u8 = b'%';
}

fn tokens<T: Wildcard>(pattern: &[T], escape: T) -> Vec<Token<T>> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut items = pattern.iter().copied().peekable();
    while let Some(item) = items.next() {
        tokens.push(if item == escape {
            Token::Exactly(items.next().unwrap_or(escape))
        } else if item == T::ANY_ONE {
            Token::AnyOne
        } else if item == T::ANY_MANY {
            Token::AnyMany
        } else {
            Token::Exactly(item)
        });
    }
    tokens
}

fn matches<T: Wildcard>(subject: &[T], tokens: &[Token<T>]) -> bool {
    let (mut position, mut token) = (0, 0);
    let mut retry: Option<(usize, usize)> = None;
    while position < subject.len() {
        match tokens.get(token) {
            Some(Token::AnyMany) => {
                retry = Some((token, position));
                token += 1;
            }
            Some(Token::AnyOne) => {
                position += 1;
                token += 1;
            }
            Some(Token::Exactly(item)) if *item == subject[position] => {
                position += 1;
                token += 1;
            }
            Some(Token::Exactly(_)) | None => match retry {
                Some((many, start)) => {
                    retry = Some((many, start + 1));
                    token = many + 1;
                    position = start + 1;
                }
                None => return false,
            },
        }
    }
    tokens[token..]
        .iter()
        .all(|token| matches!(token, Token::AnyMany))
}
