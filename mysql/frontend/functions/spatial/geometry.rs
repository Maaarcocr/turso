//! MySQL's geometry values as it stores them: the SRID as four
//! little-endian bytes, then the geometry as little-endian WKB.

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Geometry {
    Point(Point),
    LineString(Vec<Point>),
    Polygon(Vec<Vec<Point>>),
    MultiPoint(Vec<Point>),
    MultiLineString(Vec<Vec<Point>>),
    MultiPolygon(Vec<Vec<Vec<Point>>>),
    Collection(Vec<Geometry>),
}

/// The WKB type codes, which are also the types of the spatial columns
/// (`Geometry` takes any of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Geometry = 0,
    Point = 1,
    LineString = 2,
    Polygon = 3,
    MultiPoint = 4,
    MultiLineString = 5,
    MultiPolygon = 6,
    Collection = 7,
}

impl Kind {
    pub(crate) fn from_code(code: i64) -> Option<Kind> {
        Some(match code {
            0 => Kind::Geometry,
            1 => Kind::Point,
            2 => Kind::LineString,
            3 => Kind::Polygon,
            4 => Kind::MultiPoint,
            5 => Kind::MultiLineString,
            6 => Kind::MultiPolygon,
            7 => Kind::Collection,
            _ => return None,
        })
    }

    pub(crate) fn admits(self, geometry: &Geometry) -> bool {
        self == Kind::Geometry || self == geometry.kind()
    }
}

/// The SRIDs whose spatial reference system is modeled: 0, the Cartesian
/// plane, and 4326, WGS 84, whose coordinates MySQL reads and writes as
/// latitude then longitude and stores as longitude then latitude.
pub(crate) const CARTESIAN: u32 = 0;
pub(crate) const WGS_84: u32 = 4326;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stored {
    pub(crate) srid: u32,
    pub(crate) geometry: Geometry,
}

impl Stored {
    pub(crate) fn decode(bytes: &[u8]) -> Option<Stored> {
        let srid = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
        let geometry = read_wkb(&bytes[4..])?;
        Some(Stored { srid, geometry })
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = self.srid.to_le_bytes().to_vec();
        self.geometry.write_wkb(&mut bytes);
        bytes
    }

    pub(crate) fn is_geographic(&self) -> bool {
        self.srid == WGS_84
    }
}

impl Geometry {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Geometry::Point(_) => Kind::Point,
            Geometry::LineString(_) => Kind::LineString,
            Geometry::Polygon(_) => Kind::Polygon,
            Geometry::MultiPoint(_) => Kind::MultiPoint,
            Geometry::MultiLineString(_) => Kind::MultiLineString,
            Geometry::MultiPolygon(_) => Kind::MultiPolygon,
            Geometry::Collection(_) => Kind::Collection,
        }
    }

    /// Every point, in order.
    pub(crate) fn points(&self) -> Vec<Point> {
        let mut points = Vec::new();
        self.visit_points(&mut |point| points.push(point));
        points
    }

    fn visit_points(&self, visit: &mut impl FnMut(Point)) {
        match self {
            Geometry::Point(point) => visit(*point),
            Geometry::LineString(points) | Geometry::MultiPoint(points) => {
                points.iter().copied().for_each(visit)
            }
            Geometry::Polygon(lines) | Geometry::MultiLineString(lines) => {
                lines.iter().flatten().copied().for_each(visit)
            }
            Geometry::MultiPolygon(polygons) => {
                polygons.iter().flatten().flatten().copied().for_each(visit)
            }
            Geometry::Collection(geometries) => {
                for geometry in geometries {
                    geometry.visit_points(visit);
                }
            }
        }
    }

    pub(crate) fn map_points(&self, map: &impl Fn(Point) -> Point) -> Geometry {
        let line = |points: &Vec<Point>| points.iter().map(|point| map(*point)).collect();
        let lines = |lines: &Vec<Vec<Point>>| lines.iter().map(line).collect();
        match self {
            Geometry::Point(point) => Geometry::Point(map(*point)),
            Geometry::LineString(points) => Geometry::LineString(line(points)),
            Geometry::MultiPoint(points) => Geometry::MultiPoint(line(points)),
            Geometry::Polygon(rings) => Geometry::Polygon(lines(rings)),
            Geometry::MultiLineString(rings) => Geometry::MultiLineString(lines(rings)),
            Geometry::MultiPolygon(polygons) => {
                Geometry::MultiPolygon(polygons.iter().map(lines).collect())
            }
            Geometry::Collection(geometries) => Geometry::Collection(
                geometries
                    .iter()
                    .map(|geometry| geometry.map_points(map))
                    .collect(),
            ),
        }
    }

    /// Whether the geometry holds no point: only a collection can.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Geometry::Collection(geometries) => geometries.iter().all(Geometry::is_empty),
            _ => false,
        }
    }

    pub(crate) fn write_wkb(&self, out: &mut Vec<u8>) {
        out.push(1);
        out.extend_from_slice(&(self.kind() as u32).to_le_bytes());
        let count =
            |out: &mut Vec<u8>, count: usize| out.extend_from_slice(&(count as u32).to_le_bytes());
        let point = |out: &mut Vec<u8>, point: &Point| {
            out.extend_from_slice(&point.x.to_le_bytes());
            out.extend_from_slice(&point.y.to_le_bytes());
        };
        let line = |out: &mut Vec<u8>, points: &[Point]| {
            count(out, points.len());
            points.iter().for_each(|p| point(out, p));
        };
        let polygon = |out: &mut Vec<u8>, rings: &[Vec<Point>]| {
            count(out, rings.len());
            rings.iter().for_each(|ring| line(out, ring));
        };
        match self {
            Geometry::Point(p) => point(out, p),
            Geometry::LineString(points) => line(out, points),
            Geometry::Polygon(rings) => polygon(out, rings),
            Geometry::MultiPoint(points) => {
                count(out, points.len());
                for p in points {
                    Geometry::Point(*p).write_wkb(out);
                }
            }
            Geometry::MultiLineString(lines) => {
                count(out, lines.len());
                for points in lines {
                    Geometry::LineString(points.clone()).write_wkb(out);
                }
            }
            Geometry::MultiPolygon(polygons) => {
                count(out, polygons.len());
                for rings in polygons {
                    Geometry::Polygon(rings.clone()).write_wkb(out);
                }
            }
            Geometry::Collection(geometries) => {
                count(out, geometries.len());
                for geometry in geometries {
                    geometry.write_wkb(out);
                }
            }
        }
    }
}

/// A geometry from WKB of either byte order, which must fill `bytes`.
pub(crate) fn read_wkb(bytes: &[u8]) -> Option<Geometry> {
    let mut reader = Reader { bytes, at: 0 };
    let geometry = reader.geometry(None)?;
    (reader.at == bytes.len()).then_some(geometry)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let taken = self.bytes.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(taken)
    }

    fn u32(&mut self, little: bool) -> Option<u32> {
        let bytes = self.take::<4>()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    /// A count of items of at least `size` bytes each, which the rest of the
    /// bytes must be able to hold.
    fn count(&mut self, little: bool, size: usize) -> Option<usize> {
        let count = self.u32(little)? as usize;
        (count.checked_mul(size)? <= self.bytes.len() - self.at).then_some(count)
    }

    fn point(&mut self, little: bool) -> Option<Point> {
        let read = |bytes: [u8; 8]| {
            if little {
                f64::from_le_bytes(bytes)
            } else {
                f64::from_be_bytes(bytes)
            }
        };
        let x = read(self.take::<8>()?);
        let y = read(self.take::<8>()?);
        (x.is_finite() && y.is_finite()).then_some(Point { x, y })
    }

    fn line(&mut self, little: bool) -> Option<Vec<Point>> {
        let count = self.count(little, 16)?;
        (0..count).map(|_| self.point(little)).collect()
    }

    fn rings(&mut self, little: bool) -> Option<Vec<Vec<Point>>> {
        let count = self.count(little, 4)?;
        (0..count).map(|_| self.line(little)).collect()
    }

    /// A geometry with its header; `expected` is the kind a multi-geometry
    /// requires of its members.
    fn geometry(&mut self, expected: Option<Kind>) -> Option<Geometry> {
        let little = match self.take::<1>()? {
            [0] => false,
            [1] => true,
            _ => return None,
        };
        let kind = Kind::from_code(i64::from(self.u32(little)?))?;
        if expected.is_some_and(|expected| expected != kind) {
            return None;
        }
        let members = |reader: &mut Self, kind: Kind| -> Option<Vec<Geometry>> {
            let count = reader.count(little, 5)?;
            (0..count).map(|_| reader.geometry(Some(kind))).collect()
        };
        Some(match kind {
            Kind::Geometry => return None,
            Kind::Point => Geometry::Point(self.point(little)?),
            Kind::LineString => Geometry::LineString(self.line(little)?),
            Kind::Polygon => Geometry::Polygon(self.rings(little)?),
            Kind::MultiPoint => Geometry::MultiPoint(
                members(self, Kind::Point)?
                    .into_iter()
                    .map(|member| match member {
                        Geometry::Point(point) => point,
                        _ => unreachable!("a member read as a point"),
                    })
                    .collect(),
            ),
            Kind::MultiLineString => Geometry::MultiLineString(
                members(self, Kind::LineString)?
                    .into_iter()
                    .map(|member| match member {
                        Geometry::LineString(points) => points,
                        _ => unreachable!("a member read as a linestring"),
                    })
                    .collect(),
            ),
            Kind::MultiPolygon => Geometry::MultiPolygon(
                members(self, Kind::Polygon)?
                    .into_iter()
                    .map(|member| match member {
                        Geometry::Polygon(rings) => rings,
                        _ => unreachable!("a member read as a polygon"),
                    })
                    .collect(),
            ),
            Kind::Collection => {
                let count = self.count(little, 5)?;
                Geometry::Collection(
                    (0..count)
                        .map(|_| self.geometry(None))
                        .collect::<Option<_>>()?,
                )
            }
        })
    }
}
