//! Reading the capture date — the spine of the timeline.
//!
//! We pull EXIF DateTimeOriginal (when the shutter fired). This is only ever
//! read for files we already have locally: at scan time for local originals, and
//! after download for cloud originals. We never read EXIF from a cloud-only file
//! up front, because reading it would force a download (see the on-demand policy).

use std::path::Path;

/// EXIF capture date for a photo, as a Unix timestamp (seconds), or `None` if
/// the file has no usable date tag.
pub fn read_taken_ts(path: &Path) -> Option<i64> {
    let file = std::fs::File::open(path).ok()?;
    let mut buf = std::io::BufReader::new(file);
    let reader = exif::Reader::new();
    let exif = reader.read_from_container(&mut buf).ok()?;

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
