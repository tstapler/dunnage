//! Sizes, ages and times as the report prints them (`design/ux.md` 3.4): binary units, one decimal, `0 B`,
//! a leading `-` for negatives, never a locale separator.

const KIB: f64 = 1024.0;
const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

fn scaled(bytes: u64) -> (f64, usize) {
    let mut v = bytes as f64;
    let mut i = 0;
    while i + 1 < UNITS.len() && ((v * 10.0).round() / 10.0 >= 1000.0 || v >= KIB) {
        v /= KIB;
        i += 1;
    }
    (v, i)
}

/// `12.3 GiB`, `0 B`, `512 B`. Steps up a unit once the rounded value would reach 1000, so a figure is at
/// most 9 characters.
pub fn size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".into();
    }
    let (v, i) = scaled(bytes);
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `-99.7 GiB`, `+2.8 GiB`, `+0 B`: a size with its sign always shown.
pub fn signed(bytes: i64) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{}", size(bytes.unsigned_abs()))
}

/// A size that may be negative without a plus sign: `-3.0 GiB`, `3.0 GiB`.
pub fn size_i(bytes: i64) -> String {
    if bytes < 0 {
        format!("-{}", size(bytes.unsigned_abs()))
    } else {
        size(bytes.unsigned_abs())
    }
}

/// The Filesystem block keeps two decimals for TiB (`1.62 TiB`).
pub fn fs_size(bytes: u64) -> String {
    if bytes >= 1 << 40 {
        format!("{:.2} TiB", bytes as f64 / (KIB * KIB * KIB * KIB))
    } else {
        size(bytes)
    }
}

/// `at least 18.1 GiB` for a total taken from a partial scan.
pub fn size_lower_bound(bytes: u64, partial: bool) -> String {
    if partial {
        format!("at least {}", size(bytes))
    } else {
        size(bytes)
    }
}

/// An age or a duration in at most 4 characters: `41d`, `36h`, `59m`, `n/a`.
pub fn age(secs: Option<i64>) -> String {
    let Some(s) = secs else {
        return "n/a".into();
    };
    let s = s.max(0);
    let (m, h, d) = (s / 60, s / 3600, s / 86_400);
    if d >= 2 {
        if d >= 1000 {
            format!("{}y", d / 365)
        } else {
            format!("{d}d")
        }
    } else if h >= 1 {
        format!("{h}h")
    } else if m >= 1 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// Elapsed time of one item: `0.4s`, `4.1s`, `12s`, `3m`.
pub fn elapsed(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 100_000 {
        format!("{}s", ms / 1000)
    } else {
        age(Some(i64::try_from(ms / 1000).unwrap_or(i64::MAX)))
    }
}

/// `1 item`, `2 items`.
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
