use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const ZONEINFO_DIRECTORY: &str = "/usr/share/zoneinfo";
const EARLIEST_SECOND: i64 = -(1 << 40);
const LATEST_SECOND: i64 = 1 << 40;

#[derive(Clone, Debug)]
pub(super) enum Zone {
    Offset(i64),
    Named(Arc<NamedZone>),
}

impl Zone {
    pub fn find(name: &str) -> Option<Self> {
        if let Some(offset) = parse_offset(name.as_bytes()) {
            return Some(Self::Offset(offset));
        }
        if name.eq_ignore_ascii_case("SYSTEM") {
            return Some(Self::Offset(0));
        }
        named_zone(name.trim_end_matches(' ')).map(Self::Named)
    }

    pub fn local_to_utc(&self, local: i64) -> Option<i64> {
        match self {
            Self::Offset(offset) => Some(local - offset),
            Self::Named(zone) => zone.local_to_utc(local),
        }
    }

    pub fn utc_to_local(&self, utc: i64) -> i64 {
        match self {
            Self::Offset(offset) => utc + offset,
            Self::Named(zone) => utc + zone.offset_at(utc),
        }
    }
}

fn parse_offset(text: &[u8]) -> Option<i64> {
    if text.len() < 4 {
        return None;
    }
    let negative = match text[0] {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let mut pos = 1;
    let mut hours: i64 = 0;
    while pos < text.len() && text[pos].is_ascii_digit() {
        hours = hours
            .saturating_mul(10)
            .saturating_add(i64::from(text[pos] - b'0'));
        pos += 1;
    }
    if pos + 1 >= text.len() || text[pos] != b':' {
        return None;
    }
    pos += 1;
    let mut minutes: i64 = 0;
    while pos < text.len() && text[pos].is_ascii_digit() {
        minutes = minutes
            .saturating_mul(10)
            .saturating_add(i64::from(text[pos] - b'0'));
        pos += 1;
    }
    if pos != text.len() || minutes > 59 {
        return None;
    }
    let seconds = hours
        .saturating_mul(60)
        .saturating_add(minutes)
        .saturating_mul(60);
    let offset = if negative { -seconds } else { seconds };
    (-(13 * 3_600 + 59 * 60)..=14 * 3_600)
        .contains(&offset)
        .then_some(offset)
}

#[derive(Debug)]
pub(super) struct NamedZone {
    transitions: Vec<i64>,
    transition_offsets: Vec<i64>,
    initial_offset: i64,
    local_starts: Vec<i64>,
    local_ranges: Vec<LocalRange>,
}

#[derive(Clone, Copy, Debug)]
struct LocalRange {
    offset: i64,
    in_gap: bool,
}

impl NamedZone {
    fn offset_at(&self, utc: i64) -> i64 {
        if self.transitions.is_empty() || utc < self.transitions[0] {
            return self.initial_offset;
        }
        let index = self.transitions.partition_point(|start| *start <= utc) - 1;
        self.transition_offsets[index]
    }

    fn local_to_utc(&self, local: i64) -> Option<i64> {
        let count = self.local_ranges.len();
        if local < self.local_starts[0] || local > self.local_starts[count] {
            return None;
        }
        let index =
            (self.local_starts[..count].partition_point(|start| *start <= local)).max(1) - 1;
        let range = self.local_ranges[index];
        Some(if range.in_gap {
            self.local_starts[index] - range.offset
        } else {
            local - range.offset
        })
    }

    fn new(transitions: Vec<i64>, transition_offsets: Vec<i64>, initial_offset: i64) -> Self {
        let mut zone = Self {
            transitions,
            transition_offsets,
            initial_offset,
            local_starts: Vec::new(),
            local_ranges: Vec::new(),
        };
        zone.build_local_ranges();
        zone
    }

    fn build_local_ranges(&mut self) {
        let mut utc_start = EARLIEST_SECOND;
        let mut next_transition = 0;
        let mut offset = self.initial_offset;
        let mut latest_local_seen = None;
        let mut local_end;
        loop {
            let local_start = utc_start + offset;
            let utc_end = self
                .transitions
                .get(next_transition)
                .map_or(LATEST_SECOND, |start| start - 1);
            local_end = utc_end + offset;
            match latest_local_seen {
                None => {
                    self.push_local_range(local_start, offset, false);
                    latest_local_seen = Some(local_end);
                }
                Some(seen) if local_end > seen => {
                    let mut seen = seen;
                    if local_start > seen + 1 {
                        let previous_offset = self.local_ranges.last().map_or(offset, |r| r.offset);
                        self.push_local_range(seen + 1, previous_offset, true);
                        seen = local_start - 1;
                    }
                    self.push_local_range(seen + 1, offset, false);
                    latest_local_seen = Some(local_end);
                }
                Some(_) => {}
            }
            if utc_end == LATEST_SECOND {
                break;
            }
            utc_start = utc_end + 1;
            offset = self.transition_offsets[next_transition];
            next_transition += 1;
        }
        self.local_starts.push(local_end);
    }

    fn push_local_range(&mut self, start: i64, offset: i64, in_gap: bool) {
        self.local_starts.push(start);
        self.local_ranges.push(LocalRange { offset, in_gap });
    }
}

fn named_zone(name: &str) -> Option<Arc<NamedZone>> {
    static ZONES: OnceLock<Mutex<HashMap<String, Option<Arc<NamedZone>>>>> = OnceLock::new();
    let key = name.to_ascii_lowercase();
    let mut zones = ZONES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    zones
        .entry(key)
        .or_insert_with(|| load_named_zone(name).map(Arc::new))
        .clone()
}

fn load_named_zone(name: &str) -> Option<NamedZone> {
    let path = find_zone_file(name)?;
    let bytes = std::fs::read(path).ok()?;
    read_tzif_version_1(&bytes)
}

fn find_zone_file(name: &str) -> Option<PathBuf> {
    let directory = std::env::var_os("TZDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(ZONEINFO_DIRECTORY));
    let mut path = directory;
    for component in name.split('/') {
        let valid = !component.is_empty()
            && component != "."
            && component != ".."
            && component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.'));
        if !valid {
            return None;
        }
        let exact = path.join(component);
        if exact.exists() {
            path = exact;
            continue;
        }
        let entry = std::fs::read_dir(&path).ok()?.flatten().find(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|file_name| file_name.eq_ignore_ascii_case(component))
        })?;
        path = entry.path();
    }
    path.is_file().then_some(path)
}

fn read_tzif_version_1(bytes: &[u8]) -> Option<NamedZone> {
    if bytes.len() < 44 || &bytes[..4] != b"TZif" {
        return None;
    }
    let count = |index: usize| {
        let start = 20 + index * 4;
        u32::from_be_bytes(bytes[start..start + 4].try_into().expect("4 bytes")) as usize
    };
    let transition_count = count(3);
    let type_count = count(4);
    if type_count == 0 {
        return None;
    }
    let transitions_start = 44;
    let indexes_start = transitions_start + transition_count * 4;
    let types_start = indexes_start + transition_count;
    if bytes.len() < types_start + type_count * 6 {
        return None;
    }
    let offsets: Vec<(i64, bool)> = (0..type_count)
        .map(|i| {
            let start = types_start + i * 6;
            let offset = i32::from_be_bytes(bytes[start..start + 4].try_into().expect("4 bytes"));
            (i64::from(offset), bytes[start + 4] != 0)
        })
        .collect();
    let transitions: Vec<i64> = (0..transition_count)
        .map(|i| {
            let start = transitions_start + i * 4;
            i64::from(i32::from_be_bytes(
                bytes[start..start + 4].try_into().expect("4 bytes"),
            ))
        })
        .collect();
    let transition_offsets = bytes[indexes_start..types_start]
        .iter()
        .map(|index| offsets.get(usize::from(*index)).map(|(offset, _)| *offset))
        .collect::<Option<Vec<_>>>()?;
    let initial_offset = offsets
        .iter()
        .find(|(_, is_dst)| !is_dst)
        .unwrap_or(&offsets[0])
        .0;
    Some(NamedZone::new(
        transitions,
        transition_offsets,
        initial_offset,
    ))
}
