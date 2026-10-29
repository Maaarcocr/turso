//! Geohashes as MySQL encodes and decodes them.

const DIGITS: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// The geohash of `length` characters of a longitude in [-180, 180] and a
/// latitude in [-90, 90]; `None` when the point is the center of a cell
/// before the last character, where MySQL may stop early.
pub(crate) fn encode(longitude: f64, latitude: f64, length: usize) -> Option<String> {
    let mut longitudes = (-180.0, 180.0);
    let mut latitudes = (-90.0, 90.0);
    let mut hash = String::with_capacity(length);
    let mut even = true;
    for position in 0..length {
        let mut digit = 0;
        for bit in (0..5).rev() {
            let (range, value) = if even {
                (&mut longitudes, longitude)
            } else {
                (&mut latitudes, latitude)
            };
            let middle = (range.0 + range.1) / 2.0;
            if value >= middle {
                digit |= 1 << bit;
                range.0 = middle;
            } else {
                range.1 = middle;
            }
            even = !even;
        }
        hash.push(char::from(DIGITS[digit]));
        let center = |range: (f64, f64)| (range.0 + range.1) / 2.0;
        if position + 1 < length && center(longitudes) == longitude && center(latitudes) == latitude
        {
            return None;
        }
    }
    Some(hash)
}

/// The longitude and latitude a geohash names, each rounded as MySQL rounds
/// them; `None` for a hash with a character outside the lower-case geohash
/// digits.
pub(crate) fn decode(hash: &str) -> Option<(f64, f64)> {
    if hash.is_empty() {
        return None;
    }
    let mut longitudes = (-180.0, 180.0);
    let mut latitudes = (-90.0, 90.0);
    let mut even = true;
    for byte in hash.bytes() {
        let digit = DIGITS.iter().position(|digit| *digit == byte)?;
        for bit in (0..5).rev() {
            let range = if even {
                &mut longitudes
            } else {
                &mut latitudes
            };
            let middle = (range.0 + range.1) / 2.0;
            if digit & (1 << bit) != 0 {
                range.0 = middle;
            } else {
                range.1 = middle;
            }
            even = !even;
        }
    }
    Some((rounded(longitudes), rounded(latitudes)))
}

/// The center of a cell with the fewest decimals that stay inside it, as
/// MySQL's `round_latlongitude` gives it.
fn rounded((low, high): (f64, f64)) -> f64 {
    let center = (low + high) / 2.0;
    let mut error = (high - low) / 2.0;
    if error == 0.0 {
        return center;
    }
    let mut decimals = 0;
    while error <= 0.1 && decimals <= 15 {
        error *= 10.0;
        decimals += 1;
    }
    loop {
        let scale = 10f64.powi(decimals);
        let value = (center * scale).round_ties_even() / scale;
        decimals += 1;
        if (low <= value && value <= high) || decimals > 15 {
            return value;
        }
    }
}
