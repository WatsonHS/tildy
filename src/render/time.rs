//! Local wall-clock time for the `time` module, with the strftime subset in
//! `config::TIME_SPECIFIERS`. The renderer takes an optional injected value so
//! tests are deterministic.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalTime {
    /// Current local time (honours `TZ`). None if the C library fails.
    pub fn now() -> Option<LocalTime> {
        // SAFETY: time(NULL) has no preconditions; localtime_r writes only into
        // the zeroed `tm` we own and returns NULL on failure.
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return None;
            }
            Some(LocalTime {
                year: tm.tm_year + 1900,
                month: (tm.tm_mon + 1) as u32,
                day: tm.tm_mday as u32,
                hour: tm.tm_hour as u32,
                minute: tm.tm_min as u32,
                second: tm.tm_sec as u32,
            })
        }
    }

    /// Seconds since the Unix epoch, now (None if the clock is unusable).
    pub fn unix_now() -> Option<i64> {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
    }

    /// The UTC wall clock `secs` seconds after the epoch (civil calendar,
    /// no C library involved).
    pub fn utc_from_unix(secs: i64) -> LocalTime {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        LocalTime { year, month, day, hour: (rem / 3600) as u32, minute: (rem % 3600 / 60) as u32, second: (rem % 60) as u32 }
    }

    /// This wall-clock value read as UTC, in seconds since the epoch (the
    /// inverse of [`LocalTime::utc_from_unix`]).
    pub fn as_utc_unix(&self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 + i64::from(self.second)
    }

    /// Seconds since midnight.
    pub fn seconds_of_day(&self) -> u32 {
        self.hour * 3600 + self.minute * 60 + self.second
    }

    /// Parse `YYYY-MM-DDTHH:MM:SS` (tests and golden fixtures).
    pub fn parse(s: &str) -> Option<LocalTime> {
        let (d, t) = s.split_once('T')?;
        let mut d = d.splitn(3, '-');
        let mut t = t.splitn(3, ':');
        let lt = LocalTime {
            year: d.next()?.parse().ok()?,
            month: d.next()?.parse().ok()?,
            day: d.next()?.parse().ok()?,
            hour: t.next()?.parse().ok()?,
            minute: t.next()?.parse().ok()?,
            second: t.next()?.parse().ok()?,
        };
        let ok = (1..=12).contains(&lt.month) && (1..=31).contains(&lt.day) && lt.hour < 24 && lt.minute < 60 && lt.second < 61;
        ok.then_some(lt)
    }

    /// Format with `%R %H %M %S %I %p %Y %m %d %F %T %r %%`; other `%x` stay literal.
    pub fn format(&self, f: &str) -> String {
        let mut out = String::new();
        let mut it = f.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let h12 = if self.hour.is_multiple_of(12) { 12 } else { self.hour % 12 };
            match it.next() {
                Some('R') => out.push_str(&format!("{:02}:{:02}", self.hour, self.minute)),
                Some('T') => out.push_str(&format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)),
                Some('r') => {
                    let p = if self.hour < 12 { "AM" } else { "PM" };
                    out.push_str(&format!("{h12:02}:{:02}:{:02} {p}", self.minute, self.second))
                }
                Some('H') => out.push_str(&format!("{:02}", self.hour)),
                Some('M') => out.push_str(&format!("{:02}", self.minute)),
                Some('S') => out.push_str(&format!("{:02}", self.second)),
                Some('I') => out.push_str(&format!("{h12:02}")),
                Some('p') => out.push_str(if self.hour < 12 { "AM" } else { "PM" }),
                Some('Y') => out.push_str(&format!("{:04}", self.year)),
                Some('m') => out.push_str(&format!("{:02}", self.month)),
                Some('d') => out.push_str(&format!("{:02}", self.day)),
                Some('F') => out.push_str(&format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)),
                Some('%') => out.push('%'),
                Some(o) => {
                    out.push('%');
                    out.push(o);
                }
                None => out.push('%'),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let t = LocalTime::parse("2026-10-19T09:05:07").unwrap();
        assert_eq!(t.format("%R"), "09:05");
        assert_eq!(t.format("%T|%H%M%S"), "09:05:07|090507");
        assert_eq!(t.format("%I %p"), "09 AM");
        assert_eq!(t.format("%F %Y/%m/%d 100%%"), "2026-10-19 2026/10/19 100%");
        let pm = LocalTime { hour: 0, ..t };
        assert_eq!(pm.format("%I%p"), "12AM");
        assert_eq!(LocalTime { hour: 13, ..t }.format("%I%p"), "01PM");
        assert!(LocalTime::parse("2026-13-01T00:00:00").is_none());
        assert!(LocalTime::now().is_some());
        assert_eq!(t.format("%r"), "09:05:07 AM");
        assert_eq!(LocalTime { hour: 13, ..t }.format("%r"), "01:05:07 PM");
    }

    #[test]
    fn utc_round_trip() {
        let t = LocalTime::parse("2026-10-19T14:05:09").unwrap();
        assert_eq!(t.as_utc_unix(), 1_792_418_709);
        assert_eq!(LocalTime::utc_from_unix(t.as_utc_unix()), t);
        assert_eq!(LocalTime::utc_from_unix(0), LocalTime::parse("1970-01-01T00:00:00").unwrap());
        let leap = LocalTime::parse("2024-02-29T23:59:59").unwrap();
        assert_eq!(LocalTime::utc_from_unix(leap.as_utc_unix() + 1), LocalTime::parse("2024-03-01T00:00:00").unwrap());
        assert_eq!(LocalTime::utc_from_unix(-1), LocalTime::parse("1969-12-31T23:59:59").unwrap());
        assert_eq!(t.seconds_of_day(), 14 * 3600 + 5 * 60 + 9);
    }
}
