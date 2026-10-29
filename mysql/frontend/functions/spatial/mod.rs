//! MySQL's spatial functions over geometries in its storage format. Only the
//! Cartesian plane (SRID 0) and WGS 84 (SRID 4326) are modeled; a geometry
//! in another spatial reference system is refused.
mod geohash;
mod geometry;
mod wkt;

use super::json::format_json_double;
use super::string::{double, integer};
use crate::types::unsupported_value;
use geometry::{Geometry, Point};
pub(crate) use geometry::{Kind, Stored, CARTESIAN, WGS_84};
use turso_core::{LimboError, Result, Value};

type Function = fn(&str, &[Value]) -> Result<Value>;

pub(super) fn accepts(name: &str, arg_count: usize) -> bool {
    function(name, arg_count).is_some()
}

pub(super) fn execute(name: &str, args: &[Value]) -> Option<Result<Value>> {
    function(name, args.len()).map(|function| function(name, args))
}

fn function(name: &str, arg_count: usize) -> Option<Function> {
    let function: Function = match (name, arg_count) {
        ("POINT", 2) => point,
        ("LINESTRING", 1..) => line_string,
        ("POLYGON", 1..) => polygon,
        ("MULTIPOINT", 1..) => multi_point,
        ("MULTILINESTRING", 1..) => multi_line_string,
        ("MULTIPOLYGON", 1..) => multi_polygon,
        ("GEOMETRYCOLLECTION" | "GEOMCOLLECTION", _) => collection,
        ("ST_ASTEXT" | "ST_ASWKT", 1 | 2) => as_text,
        ("ST_ASBINARY" | "ST_ASWKB", 1 | 2) => as_wkb,
        ("ST_SRID", 1 | 2) => srid,
        ("ST_X", 1 | 2) => x,
        ("ST_Y", 1 | 2) => y,
        ("ST_DIMENSION", 1) => dimension,
        ("ST_GEOMETRYTYPE", 1) => geometry_type,
        ("ST_ISEMPTY", 1) => is_empty,
        ("ST_NUMPOINTS", 1) => num_points,
        ("ST_POINTN", 2) => point_n,
        ("ST_STARTPOINT", 1) => start_point,
        ("ST_ENDPOINT", 1) => end_point,
        ("ST_NUMGEOMETRIES", 1) => num_geometries,
        ("ST_GEOMETRYN", 2) => geometry_n,
        ("ST_EXTERIORRING", 1) => exterior_ring,
        ("ST_NUMINTERIORRINGS" | "ST_NUMINTERIORRING", 1) => num_interior_rings,
        ("ST_INTERIORRINGN", 2) => interior_ring_n,
        ("ST_ENVELOPE", 1) => envelope,
        ("ST_ASGEOJSON", 1) => as_geojson,
        ("ST_GEOHASH", 2 | 3) => geohash,
        ("ST_LATFROMGEOHASH", 1) => latitude_from_geohash,
        ("ST_LONGFROMGEOHASH", 1) => longitude_from_geohash,
        ("ST_POINTFROMGEOHASH", 2) => point_from_geohash,
        (_, 1..=3) if read_kind(name, &["FROMTEXT", "FROMTXT"]).is_some() => from_text,
        (_, 1..=3) if read_kind(name, &["FROMWKB"]).is_some() => from_wkb,
        _ => return None,
    };
    Some(function)
}

/// The kind of geometry `ST_<kind><suffix>` reads, such as `ST_POLYFROMTEXT`.
fn read_kind(name: &str, suffixes: &[&str]) -> Option<Kind> {
    let kind = name.strip_prefix("ST_")?;
    let kind = suffixes
        .iter()
        .find_map(|suffix| kind.strip_suffix(suffix))?;
    Some(match kind {
        "GEOM" | "GEOMETRY" => Kind::Geometry,
        "POINT" => Kind::Point,
        "LINE" | "LINESTRING" => Kind::LineString,
        "POLY" | "POLYGON" => Kind::Polygon,
        "MPOINT" | "MULTIPOINT" => Kind::MultiPoint,
        "MLINE" | "MULTILINESTRING" => Kind::MultiLineString,
        "MPOLY" | "MULTIPOLYGON" => Kind::MultiPolygon,
        "GEOMCOLL" | "GEOMETRYCOLLECTION" => Kind::Collection,
        _ => return None,
    })
}

fn mysql_error(code: u16, state: &str, message: impl std::fmt::Display) -> LimboError {
    LimboError::SqlError(format!("ERROR {code} ({state}): {message}"))
}

fn invalid_data(name: &str) -> LimboError {
    mysql_error(
        3037,
        "22023",
        format!(
            "Invalid GIS data provided to function {}.",
            name.to_ascii_lowercase()
        ),
    )
}

/// A geometry argument; `None` for NULL. Bytes that are not a geometry fail
/// as in MySQL.
fn geometry(name: &str, value: &Value) -> Result<Option<Stored>> {
    let bytes = match value {
        Value::Null => return Ok(None),
        Value::Blob(bytes) => bytes.as_slice(),
        Value::Text(text) => text.as_str().as_bytes(),
        Value::Numeric(_) => return Err(invalid_data(name)),
    };
    let stored = Stored::decode(bytes).ok_or_else(|| invalid_data(name))?;
    modeled_srid(stored.srid)?;
    Ok(Some(stored))
}

pub(crate) fn modeled_srid(srid: u32) -> Result<()> {
    if srid == CARTESIAN || srid == WGS_84 {
        return Ok(());
    }
    Err(unsupported_value(format!(
        "a geometry in the spatial reference system {srid}"
    )))
}

/// An SRID argument; `None` for NULL.
fn srid_argument(name: &str, value: &Value) -> Result<Option<u32>> {
    let Some(srid) = integer(value) else {
        return Ok(None);
    };
    let srid = u32::try_from(srid.value).map_err(|_| {
        mysql_error(
            1690,
            "22003",
            format!(
                "SRID value is out of range in '{}'",
                name.to_ascii_lowercase()
            ),
        )
    })?;
    modeled_srid(srid)?;
    Ok(Some(srid))
}

fn swapped(geometry: &Geometry) -> Geometry {
    geometry.map_points(&|point| Point {
        x: point.y,
        y: point.x,
    })
}

/// Fails as MySQL does for a coordinate outside WGS 84, whose longitude
/// (stored first) is in (-180, 180] and latitude in [-90, 90].
fn check_geographic(name: &str, geometry: &Geometry) -> Result<()> {
    let name = name.to_ascii_lowercase();
    for point in geometry.points() {
        if !(point.x > -180.0 && point.x <= 180.0) {
            return Err(mysql_error(
                3616,
                "22S02",
                format!(
                    "Longitude {:.6} is out of range in function {name}. It must be within (-180.000000, 180.000000].",
                    point.x
                ),
            ));
        }
        if !(-90.0..=90.0).contains(&point.y) {
            return Err(mysql_error(
                3617,
                "22S03",
                format!(
                    "Latitude {:.6} is out of range in function {name}. It must be within [-90.000000, 90.000000].",
                    point.y
                ),
            ));
        }
    }
    Ok(())
}

/// The coordinates as MySQL reads and writes them: latitude first in WGS 84.
fn in_axis_order(stored: &Stored) -> Geometry {
    if stored.is_geographic() {
        swapped(&stored.geometry)
    } else {
        stored.geometry.clone()
    }
}

fn stored_value(srid: u32, geometry: Geometry) -> Result<Value> {
    Ok(Value::from_slice(&Stored { srid, geometry }.encode())?)
}

/// A geometry read in the axis order of `srid`, checked and stored.
fn read_geometry(name: &str, geometry: Geometry, srid: u32) -> Result<Value> {
    if srid == WGS_84 {
        let geometry = swapped(&geometry);
        check_geographic(name, &geometry)?;
        return stored_value(srid, geometry);
    }
    stored_value(srid, geometry)
}

fn no_options(name: &str, args: &[Value], position: usize) -> Result<()> {
    if args.len() > position {
        return Err(unsupported_value(format!(
            "{name} with an options argument"
        )));
    }
    Ok(())
}

fn point(_: &str, args: &[Value]) -> Result<Value> {
    let (Some(x), Some(y)) = (double(&args[0]), double(&args[1])) else {
        return Ok(Value::Null);
    };
    stored_value(CARTESIAN, Geometry::Point(Point { x, y }))
}

/// The geometry arguments of a constructor, which must be of `kind` and
/// share an SRID; `None` when one is NULL.
fn members(name: &str, args: &[Value], kind: Kind) -> Result<Option<(u32, Vec<Geometry>)>> {
    let mut srid = None;
    let mut members = Vec::with_capacity(args.len());
    for arg in args {
        let Some(stored) = geometry(name, arg)? else {
            return Ok(None);
        };
        if !kind.admits(&stored.geometry) {
            return Err(invalid_data(name));
        }
        match srid {
            None => srid = Some(stored.srid),
            Some(srid) if srid != stored.srid => {
                return Err(unsupported_value(format!(
                    "{name} of geometries with different SRIDs"
                )))
            }
            Some(_) => {}
        }
        members.push(stored.geometry);
    }
    Ok(Some((srid.unwrap_or(CARTESIAN), members)))
}

fn points(members: Vec<Geometry>) -> Vec<Point> {
    members
        .into_iter()
        .map(|member| match member {
            Geometry::Point(point) => point,
            _ => unreachable!("members checked as points"),
        })
        .collect()
}

fn lines(members: Vec<Geometry>) -> Vec<Vec<Point>> {
    members
        .into_iter()
        .map(|member| match member {
            Geometry::LineString(points) => points,
            _ => unreachable!("members checked as linestrings"),
        })
        .collect()
}

fn line_string(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::Point)? else {
        return Ok(Value::Null);
    };
    if members.len() < 2 {
        return Err(unsupported_value(format!("{name} of one point")));
    }
    stored_value(srid, Geometry::LineString(points(members)))
}

fn polygon(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::LineString)? else {
        return Ok(Value::Null);
    };
    let rings = lines(members);
    if rings
        .iter()
        .any(|ring| ring.len() < 4 || ring.first() != ring.last())
    {
        return Err(unsupported_value(format!(
            "{name} of a ring that is not closed or has fewer than four points"
        )));
    }
    stored_value(srid, Geometry::Polygon(rings))
}

fn multi_point(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::Point)? else {
        return Ok(Value::Null);
    };
    stored_value(srid, Geometry::MultiPoint(points(members)))
}

fn multi_line_string(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::LineString)? else {
        return Ok(Value::Null);
    };
    stored_value(srid, Geometry::MultiLineString(lines(members)))
}

fn multi_polygon(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::Polygon)? else {
        return Ok(Value::Null);
    };
    let polygons = members
        .into_iter()
        .map(|member| match member {
            Geometry::Polygon(rings) => rings,
            _ => unreachable!("members checked as polygons"),
        })
        .collect();
    stored_value(srid, Geometry::MultiPolygon(polygons))
}

fn collection(name: &str, args: &[Value]) -> Result<Value> {
    let Some((srid, members)) = members(name, args, Kind::Geometry)? else {
        return Ok(Value::Null);
    };
    stored_value(srid, Geometry::Collection(members))
}

/// `ST_GeomFromText(wkt[, srid])` and the forms that read one kind.
fn from_text(name: &str, args: &[Value]) -> Result<Value> {
    no_options(name, args, 2)?;
    let kind = read_kind(name, &["FROMTEXT", "FROMTXT"]).expect("resolved as a text reader");
    let text = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Text(text) => text.as_str().to_string(),
        Value::Blob(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => return Err(invalid_data(name)),
        },
        Value::Numeric(_) => return Err(invalid_data(name)),
    };
    let srid = match args.get(1) {
        None => CARTESIAN,
        Some(srid) => match srid_argument(name, srid)? {
            Some(srid) => srid,
            None => return Ok(Value::Null),
        },
    };
    let geometry = wkt::parse(&text).ok_or_else(|| invalid_data(name))?;
    if !kind.admits(&geometry) {
        return Err(invalid_data(name));
    }
    read_geometry(name, geometry, srid)
}

/// `ST_GeomFromWKB(wkb[, srid])` and the forms that read one kind.
fn from_wkb(name: &str, args: &[Value]) -> Result<Value> {
    no_options(name, args, 2)?;
    let kind = read_kind(name, &["FROMWKB"]).expect("resolved as a WKB reader");
    let bytes = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Blob(bytes) => bytes.clone(),
        Value::Text(text) => text.as_str().as_bytes().to_vec(),
        Value::Numeric(_) => return Err(invalid_data(name)),
    };
    let srid = match args.get(1) {
        None => CARTESIAN,
        Some(srid) => match srid_argument(name, srid)? {
            Some(srid) => srid,
            None => return Ok(Value::Null),
        },
    };
    let geometry = geometry::read_wkb(&bytes).ok_or_else(|| invalid_data(name))?;
    if !kind.admits(&geometry) {
        return Err(invalid_data(name));
    }
    read_geometry(name, geometry, srid)
}

fn as_text(name: &str, args: &[Value]) -> Result<Value> {
    no_options(name, args, 1)?;
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    Ok(Value::build_text(wkt::write(&in_axis_order(&stored))))
}

fn as_wkb(name: &str, args: &[Value]) -> Result<Value> {
    no_options(name, args, 1)?;
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    let mut bytes = Vec::new();
    in_axis_order(&stored).write_wkb(&mut bytes);
    Ok(Value::from_slice(&bytes)?)
}

/// `ST_SRID(g)`, or `ST_SRID(g, srid)`, which keeps the stored coordinates.
fn srid(name: &str, args: &[Value]) -> Result<Value> {
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    let Some(target) = args.get(1) else {
        return Ok(Value::from_i64(i64::from(stored.srid)));
    };
    let Some(srid) = srid_argument(name, target)? else {
        return Ok(Value::Null);
    };
    if srid == WGS_84 {
        check_geographic(name, &stored.geometry)?;
    }
    stored_value(srid, stored.geometry)
}

/// `ST_X` and `ST_Y` of a Cartesian point, or a copy of it with the
/// coordinate set.
fn coordinate(name: &str, args: &[Value], first: bool) -> Result<Value> {
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    let Geometry::Point(point) = stored.geometry else {
        return Err(unsupported_value(format!(
            "{name} of a geometry that is not a point"
        )));
    };
    if stored.is_geographic() {
        return Err(unsupported_value(format!(
            "{name} of a point in a geographic spatial reference system"
        )));
    }
    let Some(value) = args.get(1) else {
        return Ok(Value::from_f64(if first { point.x } else { point.y }));
    };
    let Some(value) = double(value) else {
        return Ok(Value::Null);
    };
    let point = if first {
        Point { x: value, ..point }
    } else {
        Point { y: value, ..point }
    };
    stored_value(stored.srid, Geometry::Point(point))
}

fn x(name: &str, args: &[Value]) -> Result<Value> {
    coordinate(name, args, true)
}

fn y(name: &str, args: &[Value]) -> Result<Value> {
    coordinate(name, args, false)
}

fn dimension(name: &str, args: &[Value]) -> Result<Value> {
    fn of(geometry: &Geometry) -> Option<i64> {
        match geometry {
            Geometry::Point(_) | Geometry::MultiPoint(_) => Some(0),
            Geometry::LineString(_) | Geometry::MultiLineString(_) => Some(1),
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => Some(2),
            Geometry::Collection(geometries) => geometries.iter().filter_map(of).max(),
        }
    }
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    Ok(of(&stored.geometry).map_or(Value::Null, Value::from_i64))
}

fn geometry_type(name: &str, args: &[Value]) -> Result<Value> {
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    Ok(Value::build_text(match stored.geometry.kind() {
        Kind::Point => "POINT",
        Kind::LineString => "LINESTRING",
        Kind::Polygon => "POLYGON",
        Kind::MultiPoint => "MULTIPOINT",
        Kind::MultiLineString => "MULTILINESTRING",
        Kind::MultiPolygon => "MULTIPOLYGON",
        Kind::Collection => "GEOMCOLLECTION",
        Kind::Geometry => unreachable!("a stored geometry has a concrete kind"),
    }))
}

fn is_empty(name: &str, args: &[Value]) -> Result<Value> {
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    match &stored.geometry {
        Geometry::Collection(members) if members.is_empty() => Ok(Value::from_i64(1)),
        geometry if geometry.is_empty() => Err(unsupported_value(format!(
            "{name} of a collection of empty collections"
        ))),
        _ => Ok(Value::from_i64(0)),
    }
}

/// The 1-based item `index` of `items`; `None` when the index is NULL or
/// outside them.
fn nth<T: Clone>(items: &[T], index: &Value) -> Option<T> {
    let index = integer(index)?.value;
    let index = usize::try_from(index.checked_sub(1)?).ok()?;
    items.get(index).cloned()
}

/// A part of a geometry argument, as MySQL returns it: NULL for a NULL
/// argument or one of another kind.
fn part(name: &str, value: &Value, part: impl FnOnce(&Geometry) -> Option<Value>) -> Result<Value> {
    let Some(stored) = geometry(name, value)? else {
        return Ok(Value::Null);
    };
    Ok(part(&stored.geometry).unwrap_or(Value::Null))
}

/// A part of a geometry argument that is itself a geometry, in the same
/// spatial reference system.
fn geometry_part(
    name: &str,
    value: &Value,
    part: impl FnOnce(&Geometry) -> Option<Geometry>,
) -> Result<Value> {
    let Some(stored) = geometry(name, value)? else {
        return Ok(Value::Null);
    };
    match part(&stored.geometry) {
        Some(geometry) => stored_value(stored.srid, geometry),
        None => Ok(Value::Null),
    }
}

fn count(count: usize) -> Value {
    Value::from_i64(i64::try_from(count).expect("a geometry holds fewer than 2^63 parts"))
}

fn num_points(name: &str, args: &[Value]) -> Result<Value> {
    part(name, &args[0], |geometry| match geometry {
        Geometry::LineString(points) => Some(count(points.len())),
        _ => None,
    })
}

fn point_n(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| match geometry {
        Geometry::LineString(points) => nth(points, &args[1]).map(Geometry::Point),
        _ => None,
    })
}

fn start_point(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| match geometry {
        Geometry::LineString(points) => points.first().copied().map(Geometry::Point),
        _ => None,
    })
}

fn end_point(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| match geometry {
        Geometry::LineString(points) => points.last().copied().map(Geometry::Point),
        _ => None,
    })
}

/// The members of a multi-geometry or collection.
fn members_of(geometry: &Geometry) -> Option<Vec<Geometry>> {
    Some(match geometry {
        Geometry::MultiPoint(points) => points.iter().copied().map(Geometry::Point).collect(),
        Geometry::MultiLineString(lines) => {
            lines.iter().cloned().map(Geometry::LineString).collect()
        }
        Geometry::MultiPolygon(polygons) => {
            polygons.iter().cloned().map(Geometry::Polygon).collect()
        }
        Geometry::Collection(geometries) => geometries.clone(),
        _ => return None,
    })
}

fn num_geometries(name: &str, args: &[Value]) -> Result<Value> {
    part(name, &args[0], |geometry| {
        members_of(geometry).map(|members| count(members.len()))
    })
}

fn geometry_n(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| {
        nth(&members_of(geometry)?, &args[1])
    })
}

fn exterior_ring(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| match geometry {
        Geometry::Polygon(rings) => rings.first().cloned().map(Geometry::LineString),
        _ => None,
    })
}

fn num_interior_rings(name: &str, args: &[Value]) -> Result<Value> {
    part(name, &args[0], |geometry| match geometry {
        Geometry::Polygon(rings) => Some(count(rings.len().saturating_sub(1))),
        _ => None,
    })
}

fn interior_ring_n(name: &str, args: &[Value]) -> Result<Value> {
    geometry_part(name, &args[0], |geometry| match geometry {
        Geometry::Polygon(rings) => nth(rings.get(1..)?, &args[1]).map(Geometry::LineString),
        _ => None,
    })
}

/// The bounding box of a Cartesian geometry: a point or a segment when it
/// has no width or height, as in MySQL.
fn envelope(name: &str, args: &[Value]) -> Result<Value> {
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    if stored.is_geographic() {
        return Err(unsupported_value(format!(
            "{name} of a geometry in a geographic spatial reference system"
        )));
    }
    let points = stored.geometry.points();
    let Some(first) = points.first() else {
        return Err(unsupported_value(format!("{name} of an empty geometry")));
    };
    let (mut low, mut high) = (*first, *first);
    for point in &points {
        low = Point {
            x: low.x.min(point.x),
            y: low.y.min(point.y),
        };
        high = Point {
            x: high.x.max(point.x),
            y: high.y.max(point.y),
        };
    }
    let envelope = if low == high {
        Geometry::Point(low)
    } else if low.x == high.x || low.y == high.y {
        Geometry::LineString(vec![low, high])
    } else {
        Geometry::Polygon(vec![vec![
            low,
            Point {
                x: high.x,
                y: low.y,
            },
            high,
            Point {
                x: low.x,
                y: high.y,
            },
            low,
        ]])
    };
    stored_value(stored.srid, envelope)
}

/// `ST_AsGeoJSON(g)` of a Cartesian geometry, as MySQL writes the JSON.
fn as_geojson(name: &str, args: &[Value]) -> Result<Value> {
    fn list<T>(text: &mut String, items: &[T], item: &impl Fn(&mut String, &T)) {
        text.push('[');
        for (index, value) in items.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            item(text, value);
        }
        text.push(']');
    }
    fn point(text: &mut String, point: &Point) {
        text.push('[');
        text.push_str(&format_json_double(point.x));
        text.push_str(", ");
        text.push_str(&format_json_double(point.y));
        text.push(']');
    }
    fn line(text: &mut String, points: &[Point]) {
        list(text, points, &point);
    }
    fn rings(text: &mut String, rings: &[Vec<Point>]) {
        list(text, rings, &|text: &mut String, ring: &Vec<Point>| {
            line(text, ring)
        });
    }
    fn polygons(text: &mut String, polygons: &[Vec<Vec<Point>>]) {
        list(
            text,
            polygons,
            &|text: &mut String, polygon: &Vec<Vec<Point>>| rings(text, polygon),
        );
    }
    fn write(text: &mut String, geometry: &Geometry) {
        let kind = match geometry {
            Geometry::Point(_) => "Point",
            Geometry::LineString(_) => "LineString",
            Geometry::Polygon(_) => "Polygon",
            Geometry::MultiPoint(_) => "MultiPoint",
            Geometry::MultiLineString(_) => "MultiLineString",
            Geometry::MultiPolygon(_) => "MultiPolygon",
            Geometry::Collection(geometries) => {
                text.push_str("{\"type\": \"GeometryCollection\", \"geometries\": ");
                list(text, geometries, &write);
                text.push('}');
                return;
            }
        };
        text.push_str("{\"type\": \"");
        text.push_str(kind);
        text.push_str("\", \"coordinates\": ");
        match geometry {
            Geometry::Point(p) => point(text, p),
            Geometry::LineString(points) | Geometry::MultiPoint(points) => line(text, points),
            Geometry::Polygon(lines) | Geometry::MultiLineString(lines) => rings(text, lines),
            Geometry::MultiPolygon(members) => polygons(text, members),
            Geometry::Collection(_) => unreachable!("a collection has no coordinates"),
        }
        text.push('}');
    }
    let Some(stored) = geometry(name, &args[0])? else {
        return Ok(Value::Null);
    };
    if stored.is_geographic() {
        return Err(unsupported_value(format!(
            "{name} of a geometry in a geographic spatial reference system"
        )));
    }
    let mut text = String::new();
    write(&mut text, &stored.geometry);
    Ok(Value::build_text(text))
}

/// `ST_GeoHash(longitude, latitude, length)` or `ST_GeoHash(point, length)`.
fn geohash(name: &str, args: &[Value]) -> Result<Value> {
    let (longitude, latitude, length) = if let [longitude, latitude, length] = args {
        let (Some(longitude), Some(latitude), Some(length)) =
            (double(longitude), double(latitude), integer(length))
        else {
            return Ok(Value::Null);
        };
        (longitude, latitude, length.value)
    } else {
        let Some(stored) = geometry(name, &args[0])? else {
            return Ok(Value::Null);
        };
        let (Geometry::Point(point), false) = (&stored.geometry, stored.is_geographic()) else {
            return Err(unsupported_value(format!(
                "{name} of a geometry that is not a Cartesian point"
            )));
        };
        let Some(length) = integer(&args[1]) else {
            return Ok(Value::Null);
        };
        (point.x, point.y, length.value)
    };
    if !(-180.0..=180.0).contains(&longitude)
        || !(-90.0..=90.0).contains(&latitude)
        || !(1..=100).contains(&length)
    {
        return Err(unsupported_value(format!(
            "{name} of a coordinate or length out of range"
        )));
    }
    let length = usize::try_from(length).expect("a length from 1 to 100");
    let hash = geohash::encode(longitude, latitude, length).ok_or_else(|| {
        unsupported_value(format!(
            "{name} of a point at the center of a cell shorter than the length"
        ))
    })?;
    Ok(Value::build_text(hash))
}

/// The longitude and latitude of a geohash argument; `None` for NULL.
fn geohash_argument(name: &str, value: &Value) -> Result<Option<(f64, f64)>> {
    let hash = match value {
        Value::Null => return Ok(None),
        Value::Text(text) => text.as_str().to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Numeric(_) => {
            return Err(unsupported_value(format!("{name} of a number")));
        }
    };
    geohash::decode(&hash).map(Some).ok_or_else(|| {
        unsupported_value(format!(
            "{name} of `{hash}`, which is not a lower-case geohash"
        ))
    })
}

fn latitude_from_geohash(name: &str, args: &[Value]) -> Result<Value> {
    Ok(geohash_argument(name, &args[0])?
        .map_or(Value::Null, |(_, latitude)| Value::from_f64(latitude)))
}

fn longitude_from_geohash(name: &str, args: &[Value]) -> Result<Value> {
    Ok(geohash_argument(name, &args[0])?
        .map_or(Value::Null, |(longitude, _)| Value::from_f64(longitude)))
}

/// `ST_PointFromGeoHash(hash, 0)`: a Cartesian point of the longitude and
/// latitude.
fn point_from_geohash(name: &str, args: &[Value]) -> Result<Value> {
    let Some((longitude, latitude)) = geohash_argument(name, &args[0])? else {
        return Ok(Value::Null);
    };
    match srid_argument(name, &args[1])? {
        None => Ok(Value::Null),
        Some(CARTESIAN) => stored_value(
            CARTESIAN,
            Geometry::Point(Point {
                x: longitude,
                y: latitude,
            }),
        ),
        Some(srid) => Err(unsupported_value(format!(
            "{name} in the spatial reference system {srid}"
        ))),
    }
}
