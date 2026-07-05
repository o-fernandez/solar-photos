//! Reading EXIF metadata — the capture date (the spine of the timeline) and the
//! GPS position (the spine of the Places map).
//!
//! Only ever read for files we already have locally: at scan time for local
//! originals, and after download for cloud originals. We never read EXIF from a
//! cloud-only file up front, because reading it would force a download (see the
//! on-demand policy).

use std::path::Path;

/// Everything we pull from a file's EXIF in one pass (one open, one parse).
#[derive(Default)]
pub struct ExifMeta {
    /// Capture date as a Unix timestamp (seconds), if the file has a usable tag.
    pub taken_ts: Option<i64>,
    /// GPS position as decimal degrees (lat, lon), if a plausible fix is present.
    pub gps: Option<(f64, f64)>,
}

/// Read capture date + GPS from a local file. Never fails — a missing or
/// unreadable EXIF block just yields empty fields.
pub fn read_exif_meta(path: &Path) -> ExifMeta {
    let Ok(file) = std::fs::File::open(path) else { return ExifMeta::default() };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut buf) else {
        return ExifMeta::default();
    };
    ExifMeta { taken_ts: taken_ts_of(&exif), gps: gps_of(&exif) }
}

fn taken_ts_of(exif: &exif::Exif) -> Option<i64> {
    let field = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;

    // The raw ASCII value is "YYYY:MM:DD HH:MM:SS".
    if let exif::Value::Ascii(ref vals) = field.value {
        let bytes = vals.first()?;
        let s = std::str::from_utf8(bytes).ok()?;
        return parse_exif_datetime(s);
    }
    None
}

/// GPS position in decimal degrees. Rejects out-of-range values and the exact
/// (0, 0) "null island" a lost fix writes — a real photo there is vanishingly
/// rarer than a bugged GPS chip.
fn gps_of(exif: &exif::Exif) -> Option<(f64, f64)> {
    let lat = gps_coord(exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, b'S')?;
    let lon = gps_coord(exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, b'W')?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some((lat, lon))
}

/// One GPS coordinate: degrees/minutes/seconds rationals → decimal degrees,
/// negated when the hemisphere ref matches `neg` (S or W).
fn gps_coord(exif: &exif::Exif, tag: exif::Tag, ref_tag: exif::Tag, neg: u8) -> Option<f64> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    let dms = match &field.value {
        exif::Value::Rational(v) if !v.is_empty() => v,
        _ => return None,
    };
    let part = |i: usize| dms.get(i).map(|r| r.to_f64()).unwrap_or(0.0);
    let mut v = part(0) + part(1) / 60.0 + part(2) / 3600.0;
    if let Some(rf) = exif.get_field(ref_tag, exif::In::PRIMARY) {
        if let exif::Value::Ascii(ref vals) = rf.value {
            if vals.first().and_then(|b| b.first()).map(|c| c.to_ascii_uppercase()) == Some(neg) {
                v = -v;
            }
        }
    }
    v.is_finite().then_some(v)
}

/// Parse "YYYY:MM:DD HH:MM:SS" (also tolerates '-' separators). Treated as UTC —
/// EXIF carries no timezone, so time-of-day display may shift by the viewer's
/// offset, but the calendar date (what the timeline sorts on) is what matters.
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once(' ').unwrap_or((s, "00:00:00"));
    let mut d = date.split([':', '-']);
    let year: i64 = d.next()?.trim().parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next().unwrap_or("0").parse().unwrap_or(0);
    let min: i64 = t.next().unwrap_or("0").parse().unwrap_or(0);
    let sec: i64 = t.next().unwrap_or("0").parse().unwrap_or(0);
    if year < 1900 || month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }
    Some(civil_to_unix(year, month, day, hour, min, sec))
}

/// Best-effort capture date from a filename — e.g. `PXL_20240614_155210.jpg`,
/// `IMG_20230101.jpg`, `2024-06-14 12.30.00.jpg`. Free (no file read), so it's
/// the date fallback for cloud-only photos whose EXIF we can't reach without
/// downloading. Returns the first plausible `YYYYMMDD` (optionally + `HHMMSS`).
pub fn parse_filename_date(name: &str) -> Option<i64> {
    let c = name.as_bytes();
    let n = c.len();
    let is_d = |b: u8| b.is_ascii_digit();
    let is_sep = |b: u8| matches!(b, b'-' | b'_' | b'.' | b' ' | b'T' | b':' | b'/');
    // Read `len` contiguous digits at `start`; returns (value, index-after).
    let take = |start: usize, len: usize| -> Option<(i64, usize)> {
        if start + len > n || !(0..len).all(|k| is_d(c[start + k])) {
            return None;
        }
        let v: i64 = std::str::from_utf8(&c[start..start + len]).ok()?.parse().ok()?;
        Some((v, start + len))
    };
    // Optionally skip one separator.
    let skip_sep = |p: usize| if p < n && is_sep(c[p]) { p + 1 } else { p };

    let mut i = 0;
    while i < n {
        if (i == 0 || !is_d(c[i - 1])) && is_d(c[i]) {
            // Year, then month, then day — separators between components optional,
            // so this matches 20240614, 2024-06-14, 2024_06_14, 2024.06.14, …
            if let Some((y, p)) = take(i, 4) {
                if (1990..=2035).contains(&y) {
                    if let Some((m, p)) = take(skip_sep(p), 2) {
                        if (1..=12).contains(&m) {
                            if let Some((d, p)) = take(skip_sep(p), 2) {
                                if (1..=31).contains(&d) {
                                    // Optional time HH MM SS.
                                    let (mut h, mut mi, mut s) = (0, 0, 0);
                                    if let Some((hh, p)) = take(skip_sep(p), 2) {
                                        if let Some((mm, p)) = take(skip_sep(p), 2) {
                                            let ss = take(skip_sep(p), 2).map(|x| x.0).unwrap_or(0);
                                            if hh <= 23 && mm <= 59 && ss <= 59 {
                                                h = hh;
                                                mi = mm;
                                                s = ss;
                                            }
                                        }
                                    }
                                    return Some(civil_to_unix(y, m, d, h, mi, s));
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ymd(ts: i64) -> (i64, i64, i64) {
        let days = ts.div_euclid(86400);
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
    #[test]
    fn filename_dates() {
        assert_eq!(ymd(parse_filename_date("PXL_20240614_155210823.jpg").unwrap()), (2024, 6, 14));
        assert_eq!(ymd(parse_filename_date("IMG_20230101.jpg").unwrap()), (2023, 1, 1));
        assert_eq!(ymd(parse_filename_date("2024-06-14 12.30.00.jpg").unwrap()), (2024, 6, 14));
        assert_eq!(ymd(parse_filename_date("VID_2022_12_25.mp4").unwrap()), (2022, 12, 25));
        assert_eq!(parse_filename_date("DSC_0042.jpg"), None);
        assert_eq!(parse_filename_date("99999999.jpg"), None);
    }
}

/// Days-from-civil (Howard Hinnant's algorithm) → Unix seconds, UTC.
fn civil_to_unix(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + hour * 3600 + min * 60 + sec
}
