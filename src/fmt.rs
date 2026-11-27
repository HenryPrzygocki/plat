// SPDX-License-Identifier: GPL-3.0-or-later
//! Number, size and time formatting.

use std::time::{SystemTime, UNIX_EPOCH};

const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];

fn scaled(bytes: u64) -> (f64, usize) {
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1023.95 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    (v, unit)
}

/// Size with a binary unit, such as `12.4 GiB` or `812 B`.
pub fn size(bytes: u64) -> String {
    let (v, unit) = scaled(bytes);
    if unit == 0 {
        format!("{bytes} B")
    } else if v >= 99.95 {
        format!("{v:.0} {}iB", UNITS[unit])
    } else {
        format!("{v:.1} {}iB", UNITS[unit])
    }
}

/// Compact size for tight spaces, such as `12.4G` or `812B`.
pub fn size_short(bytes: u64) -> String {
    let (v, unit) = scaled(bytes);
    if unit == 0 {
        format!("{bytes}B")
    } else if v >= 99.95 {
        format!("{v:.0}{}", UNITS[unit])
    } else {
        format!("{v:.1}{}", UNITS[unit])
    }
}

/// Integer with thousands separators.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Compact count, such as `1.2M` or `834`.
pub fn count_short(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 999_950 {
        let v = n as f64 / 1e3;
        if v >= 99.95 {
            format!("{v:.0}k")
        } else {
            format!("{v:.1}k")
        }
    } else {
        let v = n as f64 / 1e6;
        if v >= 99.95 {
            format!("{v:.0}M")
        } else {
            format!("{v:.1}M")
        }
    }
}

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How long ago a timestamp was, such as `3 mo` or `2 y`.
pub fn age(mtime: i64, now: i64) -> String {
    let d = now - mtime;
    if d < 0 {
        return "future".into();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86_400;
    if d < MIN {
        "now".into()
    } else if d < HOUR {
        format!("{} min", d / MIN)
    } else if d < DAY {
        format!("{} h", d / HOUR)
    } else if d < 60 * DAY {
        format!("{} d", d / DAY)
    } else if d < 730 * DAY {
        format!("{} mo", d / (30 * DAY))
    } else {
        format!("{} y", d / (365 * DAY))
    }
}

/// Calendar date (UTC) of a timestamp, as `YYYY-MM-DD`.
pub fn date(mtime: i64) -> String {
    // Civil-from-days, after Howard Hinnant.
    let days = mtime.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Elapsed time, such as `4.2 s` or `1 m 05 s`.
pub fn duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let s = secs as u64;
        format!("{} m {:02} s", s / 60, s % 60)
    }
}

/// Make arbitrary name bytes safe to put in a terminal cell: lossy UTF-8, with
/// control characters replaced so a file name cannot move the cursor or emit
/// escape sequences.
pub fn display_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(1024 * 1024 * 150), "150 MiB");
        assert_eq!(
            size(1024 * 1024 * 1024 * 12 + 1024 * 1024 * 410),
            "12.4 GiB"
        );
        assert_eq!(size_short(1024 * 1024 * 1024), "1.0G");
        // Must roll over to the next unit rather than print "1024 KiB".
        assert_eq!(size(1024 * 1024 - 1), "1.0 MiB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count_short(1234), "1.2k");
        assert_eq!(count_short(2_500_000), "2.5M");
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_795_737_600), "2026-11-27");
    }

    #[test]
    fn names_are_sanitised() {
        assert_eq!(display_name(b"plain.txt"), "plain.txt");
        assert_eq!(display_name(b"a\x1b[31mb\n"), "a\u{fffd}[31mb\u{fffd}");
        assert_eq!(display_name(b"bad\xffutf8"), "bad\u{fffd}utf8");
    }
}
