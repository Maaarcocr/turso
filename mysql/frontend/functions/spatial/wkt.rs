//! Well-known text as MySQL reads and writes it.
use super::geometry::{Geometry, Point};
use std::fmt::Write;

/// The geometry `text` describes, or `None` where MySQL fails to read it:
/// a linestring of fewer than two points, a polygon ring of fewer than four
/// points or that is not closed, `EMPTY` other than for a collection, or
/// text after the geometry.
pub(crate) fn parse(text: &str) -> Option<Geometry> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let geometry = reader.geometry()?;
    reader.skip_spaces();
    (reader.at == reader.bytes.len()).then_some(geometry)
}

pub(crate) fn write(geometry: &Geometry) -> String {
    let mut text = String::new();
    write_geometry(&mut text, geometry);
    text
}

fn write_geometry(text: &mut String, geometry: &Geometry) {
    match geometry {
        Geometry::Point(point) => {
            text.push_str("POINT(");
            write_point(text, point);
            text.push(')');
        }
        Geometry::LineString(points) => {
            text.push_str("LINESTRING");
            write_line(text, points);
        }
        Geometry::Polygon(rings) => {
            text.push_str("POLYGON");
            write_rings(text, rings);
        }
        Geometry::MultiPoint(points) => {
            text.push_str("MULTIPOINT(");
            for (index, point) in points.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                text.push('(');
                write_point(text, point);
                text.push(')');
            }
            text.push(')');
        }
        Geometry::MultiLineString(lines) => {
            text.push_str("MULTILINESTRING");
            write_rings(text, lines);
        }
        Geometry::MultiPolygon(polygons) => {
            text.push_str("MULTIPOLYGON(");
            for (index, rings) in polygons.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                write_rings(text, rings);
            }
            text.push(')');
        }
        Geometry::Collection(geometries) if geometries.is_empty() => {
            text.push_str("GEOMETRYCOLLECTION EMPTY")
        }
        Geometry::Collection(geometries) => {
            text.push_str("GEOMETRYCOLLECTION(");
            for (index, geometry) in geometries.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                write_geometry(text, geometry);
            }
            text.push(')');
        }
    }
}

/// A coordinate with the shortest digits that read back to it, never in
/// exponent notation, as MySQL writes them.
pub(crate) fn write_number(text: &mut String, number: f64) {
    write!(text, "{number}").expect("writing to a String does not fail");
}

fn write_point(text: &mut String, point: &Point) {
    write_number(text, point.x);
    text.push(' ');
    write_number(text, point.y);
}

fn write_line(text: &mut String, points: &[Point]) {
    text.push('(');
    for (index, point) in points.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        write_point(text, point);
    }
    text.push(')');
}

fn write_rings(text: &mut String, rings: &[Vec<Point>]) {
    text.push('(');
    for (index, ring) in rings.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        write_line(text, ring);
    }
    text.push(')');
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_spaces(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_spaces();
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let eaten = self.peek() == Some(byte);
        if eaten {
            self.at += 1;
        }
        eaten
    }

    fn word(&mut self) -> Option<String> {
        self.skip_spaces();
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
            self.at += 1;
        }
        (self.at > start)
            .then(|| String::from_utf8_lossy(&self.bytes[start..self.at]).to_ascii_uppercase())
    }

    fn number(&mut self) -> Option<f64> {
        self.skip_spaces();
        let start = self.at;
        let mut at = self.at;
        let bytes = self.bytes;
        let byte = |at: usize| bytes.get(at).copied();
        if matches!(byte(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let digits_start = at;
        while byte(at).is_some_and(|byte| byte.is_ascii_digit()) {
            at += 1;
        }
        let mut digits = at - digits_start;
        if byte(at) == Some(b'.') {
            at += 1;
            let fraction_start = at;
            while byte(at).is_some_and(|byte| byte.is_ascii_digit()) {
                at += 1;
            }
            digits += at - fraction_start;
        }
        if digits == 0 {
            return None;
        }
        if matches!(byte(at), Some(b'e' | b'E')) {
            let mut exponent = at + 1;
            if matches!(byte(exponent), Some(b'+' | b'-')) {
                exponent += 1;
            }
            let exponent_start = exponent;
            while byte(exponent).is_some_and(|byte| byte.is_ascii_digit()) {
                exponent += 1;
            }
            if exponent > exponent_start {
                at = exponent;
            }
        }
        let number: f64 = std::str::from_utf8(&self.bytes[start..at])
            .ok()?
            .parse()
            .ok()?;
        self.at = at;
        number.is_finite().then_some(number)
    }

    fn point(&mut self) -> Option<Point> {
        let x = self.number()?;
        let y = self.number()?;
        Some(Point { x, y })
    }

    /// `(` items separated by commas `)`.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        if !self.eat(b'(') {
            return None;
        }
        let mut items = vec![item(self)?];
        while self.eat(b',') {
            items.push(item(self)?);
        }
        self.eat(b')').then_some(items)
    }

    fn line(&mut self) -> Option<Vec<Point>> {
        let points = self.list(Self::point)?;
        (points.len() >= 2).then_some(points)
    }

    fn rings(&mut self) -> Option<Vec<Vec<Point>>> {
        self.list(|reader| {
            let ring = reader.line()?;
            (ring.len() >= 4 && ring.first() == ring.last()).then_some(ring)
        })
    }

    fn geometry(&mut self) -> Option<Geometry> {
        Some(match self.word()?.as_str() {
            "POINT" => {
                if !self.eat(b'(') {
                    return None;
                }
                let point = self.point()?;
                if !self.eat(b')') {
                    return None;
                }
                Geometry::Point(point)
            }
            "LINESTRING" => Geometry::LineString(self.line()?),
            "POLYGON" => Geometry::Polygon(self.rings()?),
            "MULTIPOINT" => Geometry::MultiPoint(self.list(|reader| {
                if reader.eat(b'(') {
                    let point = reader.point()?;
                    reader.eat(b')').then_some(point)
                } else {
                    reader.point()
                }
            })?),
            "MULTILINESTRING" => Geometry::MultiLineString(self.list(Self::line)?),
            "MULTIPOLYGON" => Geometry::MultiPolygon(self.list(Self::rings)?),
            "GEOMETRYCOLLECTION" | "GEOMCOLLECTION" => {
                if self.peek() != Some(b'(') {
                    return (self.word()?.as_str() == "EMPTY")
                        .then_some(Geometry::Collection(Vec::new()));
                }
                self.at += 1;
                if self.eat(b')') {
                    return Some(Geometry::Collection(Vec::new()));
                }
                let mut geometries = vec![self.geometry()?];
                while self.eat(b',') {
                    geometries.push(self.geometry()?);
                }
                if !self.eat(b')') {
                    return None;
                }
                Geometry::Collection(geometries)
            }
            _ => return None,
        })
    }
}
