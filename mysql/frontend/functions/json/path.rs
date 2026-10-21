use std::collections::HashSet;

use super::document::{quote, Json};
use super::parse::parse_string;

#[derive(Debug)]
pub(super) struct Path {
    pub(super) legs: Vec<Leg>,
}

#[derive(Clone, Debug)]
pub(super) enum Leg {
    Member(String),
    AnyMember,
    Cell(ArrayIndex),
    Range(ArrayIndex, ArrayIndex),
    AnyCell,
    AnyDescendant,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ArrayIndex {
    FromStart(u32),
    FromEnd(u32),
}

#[derive(Clone, Copy)]
pub(super) struct Position {
    pub(super) index: usize,
    pub(super) within_bounds: bool,
}

#[derive(Debug)]
pub(super) struct PathError(pub(super) usize);

impl Path {
    pub(super) fn parse(text: &str) -> Result<Path, PathError> {
        let mut parser = PathParser {
            text: text.as_bytes(),
            position: 0,
        };
        parser.skip_whitespace();
        if !parser.consume(b'$') {
            return Err(PathError(parser.position));
        }
        let mut legs = Vec::new();
        parser.skip_whitespace();
        while parser.position < text.len() {
            legs.push(parser.parse_leg()?);
            parser.skip_whitespace();
        }
        if matches!(legs.last(), Some(Leg::AnyDescendant)) {
            return Err(PathError(parser.position));
        }
        Ok(Path { legs })
    }

    pub(super) fn can_match_many(&self) -> bool {
        self.legs.iter().any(|leg| {
            matches!(
                leg,
                Leg::AnyMember | Leg::AnyCell | Leg::Range(..) | Leg::AnyDescendant
            )
        })
    }

    pub(super) fn select<'a>(&self, root: &'a Json, wrap_scalars: bool) -> Vec<&'a Json> {
        let mut selected = vec![root];
        for leg in &self.legs {
            let mut found = Found::default();
            for value in selected {
                leg.select_children(value, wrap_scalars, &mut found);
            }
            selected = found.values;
        }
        selected
    }

    pub(super) fn locate(&self, root: &Json, wrap_scalars: bool) -> Option<Vec<Step>> {
        locate_legs(&self.legs, root, wrap_scalars)
    }
}

pub(super) fn locate_legs(legs: &[Leg], root: &Json, wrap_scalars: bool) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    let mut value = root;
    for leg in legs {
        match (leg, value) {
            (Leg::Member(name), Json::Object(object)) => {
                value = object.get(name)?;
                steps.push(Step::Member(name.clone()));
            }
            (Leg::Cell(index), Json::Array(items)) => {
                let position = index.position(items.len());
                if !position.within_bounds {
                    return None;
                }
                value = &items[position.index];
                steps.push(Step::Cell(position.index));
            }
            (Leg::Cell(index), _) if wrap_scalars && index.position(1).within_bounds => {}
            (Leg::Member(_) | Leg::Cell(_), _) => return None,
            (wildcard, _) => unreachable!("{wildcard:?} must be rejected before locating"),
        }
    }
    Some(steps)
}

pub(super) enum Step {
    Member(String),
    Cell(usize),
}

pub(super) fn value_at_mut<'a>(root: &'a mut Json, steps: &[Step]) -> &'a mut Json {
    steps.iter().fold(root, |value, step| match (step, value) {
        (Step::Member(name), Json::Object(object)) => {
            object.get_mut(name).expect("located member exists")
        }
        (Step::Cell(index), Json::Array(items)) => &mut items[*index],
        _ => unreachable!("steps come from locating the same document"),
    })
}

#[derive(Default)]
struct Found<'a> {
    values: Vec<&'a Json>,
    seen: HashSet<*const Json>,
}

impl<'a> Found<'a> {
    fn add(&mut self, value: &'a Json) {
        if self.seen.insert(value as *const Json) {
            self.values.push(value);
        }
    }
}

impl Leg {
    fn select_children<'a>(&self, value: &'a Json, wrap_scalars: bool, found: &mut Found<'a>) {
        match (self, value) {
            (Leg::Member(name), Json::Object(object)) => {
                if let Some(child) = object.get(name) {
                    found.add(child);
                }
            }
            (Leg::AnyMember, Json::Object(object)) => {
                object.values().for_each(|child| found.add(child));
            }
            (Leg::Cell(index), Json::Array(items)) => {
                let position = index.position(items.len());
                if position.within_bounds {
                    found.add(&items[position.index]);
                }
            }
            (Leg::Cell(index), _) => {
                if wrap_scalars && index.position(1).within_bounds {
                    found.add(value);
                }
            }
            (Leg::Range(..) | Leg::AnyCell, Json::Array(items)) => {
                items[self.cells(items.len())]
                    .iter()
                    .for_each(|child| found.add(child));
            }
            (Leg::Range(..), _) => {
                if wrap_scalars && self.cells(1) == (0..1) {
                    found.add(value);
                }
            }
            (Leg::AnyDescendant, _) => {
                found.add(value);
                match value {
                    Json::Array(items) => items
                        .iter()
                        .for_each(|child| self.select_children(child, wrap_scalars, found)),
                    Json::Object(object) => object
                        .values()
                        .for_each(|child| self.select_children(child, wrap_scalars, found)),
                    _ => {}
                }
            }
            (Leg::Member(_) | Leg::AnyMember | Leg::AnyCell, _) => {}
        }
    }

    fn cells(&self, len: usize) -> std::ops::Range<usize> {
        match self {
            Leg::AnyCell => 0..len,
            Leg::Range(first, last) => {
                let begin = first.position(len).index;
                let last = last.position(len);
                let end = if last.within_bounds {
                    last.index + 1
                } else {
                    last.index
                };
                if begin < end {
                    begin..end
                } else {
                    0..0
                }
            }
            other => unreachable!("{other:?} does not select a range of cells"),
        }
    }
}

impl ArrayIndex {
    pub(super) fn position(self, len: usize) -> Position {
        match self {
            ArrayIndex::FromStart(index) => Position {
                index: (index as usize).min(len),
                within_bounds: (index as usize) < len,
            },
            ArrayIndex::FromEnd(back) if (back as usize) < len => Position {
                index: len - 1 - back as usize,
                within_bounds: true,
            },
            ArrayIndex::FromEnd(_) => Position {
                index: 0,
                within_bounds: false,
            },
        }
    }
}

pub(super) fn format_steps(steps: &[Step]) -> String {
    let mut out = String::from("$");
    for step in steps {
        match step {
            Step::Member(name) if is_identifier(name) => {
                out.push('.');
                out.push_str(name);
            }
            Step::Member(name) => {
                out.push('.');
                out.push_str(&quote(name));
            }
            Step::Cell(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

struct PathParser<'a> {
    text: &'a [u8],
    position: usize,
}

impl PathParser<'_> {
    fn parse_leg(&mut self) -> Result<Leg, PathError> {
        match self.peek() {
            Some(b'.') => self.parse_member(),
            Some(b'[') => self.parse_cell(),
            Some(b'*') if self.text.get(self.position + 1) == Some(&b'*') => {
                self.position += 2;
                if self.peek() == Some(b'*') {
                    return Err(PathError(self.position));
                }
                Ok(Leg::AnyDescendant)
            }
            _ => Err(PathError(self.position)),
        }
    }

    fn parse_member(&mut self) -> Result<Leg, PathError> {
        self.position += 1;
        self.skip_whitespace();
        if self.consume(b'*') {
            return Ok(Leg::AnyMember);
        }
        let start = self.position;
        let end = self.end_of_member_name();
        if start == end {
            return Err(PathError(self.position));
        }
        let text = std::str::from_utf8(&self.text[start..end]).map_err(|_| PathError(start))?;
        let name = if text.starts_with('"') {
            parse_string(text)
        } else {
            parse_string(&format!("\"{text}\"")).filter(|name| is_identifier(name))
        };
        self.position = end;
        name.map(Leg::Member).ok_or(PathError(end))
    }

    fn end_of_member_name(&self) -> usize {
        let mut position = self.position;
        if self.text.get(position) == Some(&b'"') {
            position += 1;
            while position < self.text.len() {
                match self.text[position] {
                    b'\\' => position += 2,
                    b'"' => return position + 1,
                    _ => position += 1,
                }
            }
            return self.text.len();
        }
        while position < self.text.len()
            && !is_whitespace(self.text[position])
            && !matches!(self.text[position], b'[' | b'.' | b'*')
        {
            position += 1;
        }
        position
    }

    fn parse_cell(&mut self) -> Result<Leg, PathError> {
        self.position += 1;
        self.skip_whitespace();
        let leg = if self.consume(b'*') {
            Leg::AnyCell
        } else {
            let first = self.parse_index()?;
            let before_whitespace = self.position;
            self.skip_whitespace();
            if self.position > before_whitespace && self.starts_range() {
                self.position += 2;
                self.skip_whitespace();
                let last = self.parse_index()?;
                if let (ArrayIndex::FromStart(first), ArrayIndex::FromStart(last)) = (first, last) {
                    if first > last {
                        return Err(PathError(self.position));
                    }
                }
                Leg::Range(first, last)
            } else {
                Leg::Cell(first)
            }
        };
        self.skip_whitespace();
        if !self.consume(b']') {
            return Err(PathError(self.position));
        }
        Ok(leg)
    }

    fn starts_range(&self) -> bool {
        self.text[self.position..].starts_with(b"to")
            && self
                .text
                .get(self.position + 2)
                .is_some_and(|byte| is_whitespace(*byte))
    }

    fn parse_index(&mut self) -> Result<ArrayIndex, PathError> {
        if self.text[self.position..].starts_with(b"last") {
            self.position += 4;
            let after_last = self.position;
            self.skip_whitespace();
            if self.consume(b'-') {
                self.skip_whitespace();
                return self.parse_number().map(ArrayIndex::FromEnd);
            }
            self.position = after_last;
            return Ok(ArrayIndex::FromEnd(0));
        }
        self.parse_number().map(ArrayIndex::FromStart)
    }

    fn parse_number(&mut self) -> Result<u32, PathError> {
        let start = self.position;
        let mut value: u64 = 0;
        while let Some(digit) = self.peek().filter(u8::is_ascii_digit) {
            value = value * 10 + u64::from(digit - b'0');
            if value > u64::from(u32::MAX) {
                return Err(PathError(start));
            }
            self.position += 1;
        }
        if self.position == start {
            return Err(PathError(start));
        }
        Ok(value as u32)
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.position).copied()
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_whitespace) {
            self.position += 1;
        }
    }
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let starts_identifier =
        |character: char| character == '$' || character == '_' || character.is_alphabetic();
    characters.next().is_some_and(starts_identifier)
        && characters.all(|character| {
            starts_identifier(character)
                || character.is_numeric()
                || character == '\u{200C}'
                || character == '\u{200D}'
        })
}
