//! The local time zone offset recorded in Meta timestamps (SPEC 4.3.3, 11.6; P28).
//!
//! The offset comes from the `TZ` environment variable when set (a TZif file name or path,
//! optionally prefixed with `:`, or a POSIX TZ string such as `CET-1CEST,M3.5.0,M10.5.0/3`),
//! otherwise from `/etc/localtime`. TZif files are read with their 64-bit data block (version
//! 2 and later) and the footer TZ string for instants after the last transition. Anything
//! unreadable falls back to UTC (offset 0). Pure computation: no unsafe libc calls.

use std::path::{Path, PathBuf};

const ZONEINFO_DIRS: &[&str] = &[
    "/usr/share/zoneinfo",
    "/var/db/timezone/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
];

/// Local UTC offset in minutes (east positive) at Unix time `t` (SPEC 4.3.3 timestamps).
pub(crate) fn local_offset_minutes(t: i64) -> i64 {
    let tz = std::env::var("TZ").ok();
    offset_seconds(tz.as_deref(), t).unwrap_or(0) / 60
}

fn offset_seconds(tz: Option<&str>, t: i64) -> Option<i64> {
    match tz {
        None => tzif_offset(&std::fs::read("/etc/localtime").ok()?, t),
        Some(tz) => from_tz_var(tz, t),
    }
}

fn from_tz_var(tz: &str, t: i64) -> Option<i64> {
    let (name, file_only) = match tz.strip_prefix(':') {
        Some(n) => (n, true),
        None => (tz, false),
    };
    if name.is_empty() {
        return Some(0);
    }
    for p in zone_files(name) {
        if let Ok(b) = std::fs::read(&p)
            && let Some(off) = tzif_offset(&b, t)
        {
            return Some(off);
        }
    }
    if file_only {
        None
    } else {
        Posix::parse(name).map(|p| p.offset_at(t))
    }
}

fn zone_files(name: &str) -> Vec<PathBuf> {
    if name.starts_with('/') {
        return vec![PathBuf::from(name)];
    }
    if name.split('/').any(|c| c == ".." || c.is_empty()) {
        return Vec::new();
    }
    ZONEINFO_DIRS
        .iter()
        .map(|d| Path::new(d).join(name))
        .collect()
}

// ---- TZif (RFC 8536) -----------------------------------------------------------------------

struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Counts {
    fn block_len(&self, tsize: usize) -> usize {
        self.timecnt * tsize
            + self.timecnt
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (tsize + 4)
            + self.isstdcnt
            + self.isutcnt
    }
}

fn be_u32(b: &[u8], at: usize) -> Option<usize> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(s)).ok()
}

fn be_i32(b: &[u8], at: usize) -> Option<i64> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(i64::from(i32::from_be_bytes(s)))
}

fn be_i64(b: &[u8], at: usize) -> Option<i64> {
    let s: [u8; 8] = b.get(at..at + 8)?.try_into().ok()?;
    Some(i64::from_be_bytes(s))
}

fn header(b: &[u8], at: usize) -> Option<(u8, Counts)> {
    if b.get(at..at + 4)? != b"TZif" {
        return None;
    }
    let version = *b.get(at + 4)?;
    let c = at + 20;
    Some((
        version,
        Counts {
            isutcnt: be_u32(b, c)?,
            isstdcnt: be_u32(b, c + 4)?,
            leapcnt: be_u32(b, c + 8)?,
            timecnt: be_u32(b, c + 12)?,
            typecnt: be_u32(b, c + 16)?,
            charcnt: be_u32(b, c + 20)?,
        },
    ))
}

const HEADER_LEN: usize = 44;

fn tzif_offset(b: &[u8], t: i64) -> Option<i64> {
    let (version, v1) = header(b, 0)?;
    if version >= b'2' {
        let start = HEADER_LEN + v1.block_len(4);
        let (_, v2) = header(b, start)?;
        let data = start + HEADER_LEN;
        let footer_at = data + v2.block_len(8);
        let footer = b.get(footer_at..).and_then(|f| {
            let f = f.strip_prefix(b"\n")?;
            let end = f.iter().position(|c| *c == b'\n')?;
            std::str::from_utf8(&f[..end]).ok()
        });
        block_offset(b, data, &v2, 8, footer, t)
    } else {
        block_offset(b, HEADER_LEN, &v1, 4, None, t)
    }
}

fn block_offset(
    b: &[u8],
    data: usize,
    c: &Counts,
    tsize: usize,
    footer: Option<&str>,
    t: i64,
) -> Option<i64> {
    let time_at = |i: usize| -> Option<i64> {
        let at = data + i * tsize;
        if tsize == 8 {
            be_i64(b, at)
        } else {
            be_i32(b, at)
        }
    };
    let idx_start = data + c.timecnt * tsize;
    let types_start = idx_start + c.timecnt;
    let utoff = |ty: usize| -> Option<i64> {
        if ty >= c.typecnt {
            return None;
        }
        be_i32(b, types_start + ty * 6)
    };
    let posix = footer.filter(|f| !f.is_empty()).and_then(Posix::parse);
    if c.timecnt == 0 {
        return match posix {
            Some(p) => Some(p.offset_at(t)),
            None => utoff(0),
        };
    }
    if t < time_at(0)? {
        return utoff(0);
    }
    // The last transition at or before `t` (binary search; transitions ascend).
    let (mut lo, mut hi) = (0usize, c.timecnt);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if time_at(mid)? <= t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    if lo == c.timecnt - 1
        && let Some(p) = posix
    {
        return Some(p.offset_at(t));
    }
    utoff(usize::from(*b.get(idx_start + lo)?))
}

// ---- POSIX TZ strings ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month { m: u32, w: u32, d: u32, time: i64 },
    /// `Jn`: Julian day 1..=365, February 29 never counted.
    Julian { n: u32, time: i64 },
    /// `n`: zero-based day of the year, counting February 29.
    Day { n: u32, time: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Posix {
    /// Standard offset, seconds east of UTC.
    std_off: i64,
    /// Daylight offset and the start/end rules.
    dst: Option<(i64, Rule, Rule)>,
}

struct Cursor<'a> {
    s: &'a [u8],
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.first().copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.s = &self.s[1..];
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Option<()> {
        if self.eat(b'<') {
            let end = self.s.iter().position(|c| *c == b'>')?;
            self.s = &self.s[end + 1..];
            return Some(());
        }
        let n = self
            .s
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if n < 3 {
            return None;
        }
        self.s = &self.s[n..];
        Some(())
    }

    fn number(&mut self) -> Option<i64> {
        let n = self.s.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return None;
        }
        let v = std::str::from_utf8(&self.s[..n])
            .ok()?
            .parse::<i64>()
            .ok()?;
        self.s = &self.s[n..];
        Some(v)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn time(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut secs = self.number()? * 3600;
        if self.eat(b':') {
            secs += self.number()? * 60;
            if self.eat(b':') {
                secs += self.number()?;
            }
        }
        Some(sign * secs)
    }

    fn rule(&mut self) -> Option<Rule> {
        let at_time =
            |c: &mut Cursor<'_>| -> Option<i64> { if c.eat(b'/') { c.time() } else { Some(7200) } };
        let small = |v: i64| u32::try_from(v).ok();
        if self.eat(b'M') {
            let m = small(self.number()?)?;
            if !self.eat(b'.') {
                return None;
            }
            let w = small(self.number()?)?;
            if !self.eat(b'.') {
                return None;
            }
            let d = small(self.number()?)?;
            if !(1..=12).contains(&m) || !(1..=5).contains(&w) || d > 6 {
                return None;
            }
            let time = at_time(self)?;
            Some(Rule::Month { m, w, d, time })
        } else if self.eat(b'J') {
            let n = small(self.number()?)?;
            if !(1..=365).contains(&n) {
                return None;
            }
            let time = at_time(self)?;
            Some(Rule::Julian { n, time })
        } else {
            let n = small(self.number()?)?;
            if n > 365 {
                return None;
            }
            let time = at_time(self)?;
            Some(Rule::Day { n, time })
        }
    }
}

impl Posix {
    fn parse(s: &str) -> Option<Posix> {
        let mut c = Cursor { s: s.as_bytes() };
        c.name()?;
        let std_off = -c.time()?;
        if c.peek().is_none() {
            return Some(Posix { std_off, dst: None });
        }
        c.name()?;
        let dst_off = match c.peek() {
            Some(x) if x.is_ascii_digit() || x == b'+' || x == b'-' => -c.time()?,
            _ => std_off + 3600,
        };
        let (start, end) = if c.eat(b',') {
            let start = c.rule()?;
            if !c.eat(b',') {
                return None;
            }
            (start, c.rule()?)
        } else {
            // POSIX leaves the default rules implementation-defined; use the US rules.
            (
                Rule::Month {
                    m: 3,
                    w: 2,
                    d: 0,
                    time: 7200,
                },
                Rule::Month {
                    m: 11,
                    w: 1,
                    d: 0,
                    time: 7200,
                },
            )
        };
        if c.peek().is_some() {
            return None;
        }
        Some(Posix {
            std_off,
            dst: Some((dst_off, start, end)),
        })
    }

    fn offset_at(&self, t: i64) -> i64 {
        let Some((dst_off, start, end)) = self.dst else {
            return self.std_off;
        };
        let year = civil_from_days((t + self.std_off).div_euclid(86_400)).0;
        // Start is given in local standard time, end in local daylight time.
        let s = rule_local_seconds(year, start) - self.std_off;
        let e = rule_local_seconds(year, end) - dst_off;
        let in_dst = if s < e {
            s <= t && t < e
        } else {
            !(e <= t && t < s)
        };
        if in_dst { dst_off } else { self.std_off }
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: i64) -> i64 {
    let m = i64::from(m);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month, day)` of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Seconds since the epoch, in local wall time of the rule's reference offset, at which
/// `rule` fires in `year`.
fn rule_local_seconds(year: i64, rule: Rule) -> i64 {
    match rule {
        Rule::Month { m, w, d, time } => {
            let first = days_from_civil(year, m, 1);
            let wd = (first + 4).rem_euclid(7); // 1970-01-01 was a Thursday
            let mut day = 1 + (i64::from(d) - wd).rem_euclid(7) + (i64::from(w) - 1) * 7;
            while day > days_in_month(year, m) {
                day -= 7;
            }
            days_from_civil(year, m, day) * 86_400 + time
        }
        Rule::Julian { n, time } => {
            let mut doy = i64::from(n) - 1;
            if is_leap(year) && n >= 60 {
                doy += 1;
            }
            (days_from_civil(year, 1, 1) + doy) * 86_400 + time
        }
        Rule::Day { n, time } => (days_from_civil(year, 1, 1) + i64::from(n)) * 86_400 + time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: i64, h: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600
    }

    #[test]
    fn civil_round_trip() {
        for z in [-800_000, -1, 0, 1, 11_016, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, i64::from(d)), z);
        }
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn posix_strings() {
        let utc = Posix::parse("UTC0").unwrap();
        assert_eq!(utc.offset_at(0), 0);
        let india = Posix::parse("<+0530>-5:30").unwrap();
        assert_eq!(india.offset_at(1_700_000_000), 19_800);
        let us = Posix::parse("PST8PDT,M3.2.0,M11.1.0").unwrap();
        // 2024: DST from 2024-03-10 10:00 UTC to 2024-11-03 09:00 UTC.
        assert_eq!(us.offset_at(at(2024, 1, 15, 12)), -8 * 3600);
        assert_eq!(us.offset_at(at(2024, 7, 1, 12)), -7 * 3600);
        assert_eq!(us.offset_at(at(2024, 3, 10, 9)), -8 * 3600);
        assert_eq!(us.offset_at(at(2024, 3, 10, 10)), -7 * 3600);
        assert_eq!(us.offset_at(at(2024, 11, 3, 8)), -7 * 3600);
        assert_eq!(us.offset_at(at(2024, 11, 3, 9)), -8 * 3600);
        let eu = Posix::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        // 2024: DST from 2024-03-31 01:00 UTC to 2024-10-27 01:00 UTC.
        assert_eq!(eu.offset_at(at(2024, 3, 31, 0)), 3600);
        assert_eq!(eu.offset_at(at(2024, 3, 31, 1)), 7200);
        assert_eq!(eu.offset_at(at(2024, 10, 27, 0)), 7200);
        assert_eq!(eu.offset_at(at(2024, 10, 27, 1)), 3600);
        // Southern hemisphere: DST spans the new year.
        let au = Posix::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(au.offset_at(at(2024, 1, 10, 0)), 11 * 3600);
        assert_eq!(au.offset_at(at(2024, 7, 10, 0)), 10 * 3600);
        let j = Posix::parse("XXX0YYY,J60,300").unwrap();
        assert_eq!(j.offset_at(at(2024, 6, 1, 0)), 3600);
        assert_eq!(j.offset_at(at(2024, 1, 1, 0)), 0);
        for bad in ["", "AB1", "EST", "EST5EDT,M13.1.0,M1.1.0", "EST5EDT,M3.2.0"] {
            assert!(Posix::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tz_variable_forms() {
        assert_eq!(from_tz_var("", 0), Some(0));
        assert_eq!(from_tz_var(":", 0), Some(0));
        assert_eq!(from_tz_var("JST-9", 0), Some(9 * 3600));
        assert_eq!(from_tz_var(":../../etc/passwd", 0), None);
        assert_eq!(offset_seconds(Some("UTC0"), 5), Some(0));
    }

    #[test]
    fn zoneinfo_files_when_present() {
        let ny = ZONEINFO_DIRS
            .iter()
            .map(|d| Path::new(d).join("America/New_York"))
            .find(|p| p.exists());
        let Some(ny) = ny else {
            eprintln!("SKIP: no zoneinfo database");
            return;
        };
        let b = std::fs::read(ny).unwrap();
        assert_eq!(tzif_offset(&b, at(2024, 1, 15, 12)), Some(-5 * 3600));
        assert_eq!(tzif_offset(&b, at(2024, 7, 1, 12)), Some(-4 * 3600));
        // Far future: the footer rule (or the fat table) still yields DST in July.
        assert_eq!(tzif_offset(&b, at(2090, 7, 1, 12)), Some(-4 * 3600));
        assert_eq!(
            from_tz_var("America/New_York", at(2024, 1, 15, 12)),
            Some(-5 * 3600)
        );
        assert!(tzif_offset(b"not a tzif file", 0).is_none());
    }
}
