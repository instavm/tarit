use std::fmt::{self, Write};
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::{Date, OffsetDateTime, SignedDuration, UtcOffset};

/// A UTC instant with Tarit's stable JSON and SQLite representations.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp {
    seconds: i64,
    // RFC 3339 leap seconds follow second 59, with fractions in [1e9, 2e9).
    nanos: u32,
}

const BILLION: u32 = 1_000_000_000;

impl Timestamp {
    pub const MIN: Self = Self {
        seconds: time::macros::datetime!(-262143-01-01 00:00 UTC).unix_timestamp(),
        nanos: 0,
    };
    const MAX_SECONDS: i64 = time::macros::datetime!(+262142-12-31 23:59:59 UTC).unix_timestamp();

    pub fn now() -> Self {
        Self::from_datetime(OffsetDateTime::now_utc())
    }

    fn from_datetime(value: OffsetDateTime) -> Self {
        Self {
            seconds: value.unix_timestamp(),
            nanos: value.nanosecond(),
        }
    }

    pub fn from_timestamp(seconds: i64, nanos: u32) -> Option<Self> {
        let max_nanos = if seconds.rem_euclid(60) == 59 {
            2 * BILLION
        } else {
            BILLION
        };
        ((Self::MIN.seconds..=Self::MAX_SECONDS).contains(&seconds) && nanos < max_nanos)
            .then_some(Self { seconds, nanos })
    }

    fn utc_second(self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(self.seconds)
            .expect("timestamp seconds are in the supported range")
    }

    pub fn unix_timestamp(self) -> i64 {
        self.seconds
    }

    pub fn unix_timestamp_nanos(self) -> i128 {
        i128::from(self.seconds) * i128::from(BILLION) + i128::from(self.nanos)
    }

    pub fn nanosecond(self) -> u32 {
        self.nanos
    }

    pub fn to_microsecond_precision(self) -> Self {
        Self {
            nanos: self.nanos / 1_000 * 1_000,
            ..self
        }
    }

    fn checked_add(self, duration: SignedDuration) -> Option<Self> {
        let nanos = i128::from(BILLION);
        let elapsed = i128::from(self.nanos) + duration.whole_nanoseconds();
        // Count the inserted second when crossing it from a known leap instant.
        let elapsed = if self.nanos >= BILLION && elapsed >= 2 * nanos {
            elapsed - nanos
        } else if self.nanos >= BILLION && elapsed >= nanos {
            return Self::from_timestamp(self.seconds, elapsed as u32);
        } else {
            elapsed
        };
        let seconds = i128::from(self.seconds) + elapsed.div_euclid(nanos);
        Self::from_timestamp(seconds.try_into().ok()?, elapsed.rem_euclid(nanos) as u32)
    }

    pub fn checked_sub(self, duration: SignedDuration) -> Option<Self> {
        self.checked_add(duration.checked_neg()?)
    }

    /// SQLite persists UTC with a numeric offset. JSON uses the Z suffix.
    pub fn to_rfc3339(&self) -> String {
        let mut result = String::with_capacity(35);
        self.write_timestamp(&mut result, 'T', "+00:00")
            .expect("writing to a string");
        result
    }

    fn write_timestamp(&self, out: &mut impl Write, separator: char, offset: &str) -> fmt::Result {
        let utc = self.utc_second();
        let (hour, minute, second) = utc.to_hms();
        let second = second + u8::from(self.nanos >= BILLION);
        let nanos = self.nanos % BILLION;
        write!(
            out,
            "{}{separator}{hour:02}:{minute:02}:{second:02}",
            utc.date()
        )?;
        if nanos != 0 {
            if nanos.is_multiple_of(1_000_000) {
                write!(out, ".{:03}", nanos / 1_000_000)?;
            } else if nanos.is_multiple_of(1_000) {
                write!(out, ".{:06}", nanos / 1_000)?;
            } else {
                write!(out, ".{nanos:09}")?;
            }
        }
        out.write_str(offset)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_timestamp(f, ' ', " UTC")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid UTC timestamp")]
pub struct ParseTimestampError;

impl FromStr for Timestamp {
    type Err = ParseTimestampError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_timestamp(value).ok_or(ParseTimestampError)
    }
}

// Older JSON clients and SQLite rows use both RFC 3339 and its relaxed forms
// (space separator, signed years, compact offsets and UTC). Validate calendar
// components with time, while retaining the fractional part of leap seconds.
fn parse_timestamp(value: &str) -> Option<Timestamp> {
    let mut source = value.trim();
    let signed = source.starts_with(['+', '-']);
    let negative = source.starts_with('-');
    if signed {
        source = &source[1..];
    }
    let year: i32 = digits(&mut source, if signed { usize::MAX } else { 4 })?
        .parse()
        .ok()?;
    let year = if negative { -year } else { year };
    source = source.trim_start().strip_prefix('-')?;
    let month: u8 = digits(&mut source, 2)?.parse().ok()?;
    source = source.trim_start().strip_prefix('-')?;
    let day = digits(&mut source, 2)?.parse().ok()?;
    let date = Date::from_calendar_date(year, month.try_into().ok()?, day).ok()?;
    source = source.strip_prefix(['T', 't', ' '])?;
    let hour = digits(&mut source, 2)?.parse().ok()?;
    source = source.trim_start().strip_prefix(':')?;
    let minute = digits(&mut source, 2)?.parse().ok()?;
    source = source.trim_start().strip_prefix(':')?;
    let second: u8 = digits(&mut source, 2)?.parse().ok()?;
    if second > 60 {
        return None;
    }
    let fraction = if let Some(rest) = source.strip_prefix('.') {
        source = rest;
        let len = source.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        let (digits, tail) = source.split_at(len);
        source = tail;
        Some(&digits[..len.min(9)])
    } else {
        None
    };
    let offset = parse_offset(source.trim_start())?;
    let mut nanos = 0;
    if let Some(fraction) = fraction {
        nanos = fraction.parse::<u32>().ok()? * 10u32.pow(9 - fraction.len() as u32);
    }
    let local = date.with_hms(hour, minute, second.min(59)).ok()?;
    Timestamp::from_timestamp(
        local.assume_offset(offset).unix_timestamp(),
        nanos + u32::from(second == 60) * BILLION,
    )
}

fn digits<'a>(source: &mut &'a str, max_digits: usize) -> Option<&'a str> {
    let s = source.trim_start();
    let length = s
        .bytes()
        .take(max_digits)
        .take_while(u8::is_ascii_digit)
        .count();
    let (digits, rest) = s.split_at(length);
    *source = rest;
    (!digits.is_empty()).then_some(digits)
}

fn parse_offset(zone: &str) -> Option<UtcOffset> {
    if zone.eq_ignore_ascii_case("Z") || zone.eq_ignore_ascii_case("UTC") {
        return Some(UtcOffset::UTC);
    }
    let negative = zone.starts_with(['-', '−']);
    let rest = zone.strip_prefix(['+', '-', '−'])?;
    let (hours, rest) = rest.split_at_checked(2)?;
    let minutes = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    if minutes.len() != 2
        || !hours
            .bytes()
            .chain(minutes.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let hours: i8 = hours.parse().ok()?;
    let minutes: i8 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = UtcOffset::from_hms(hours, minutes, 0).ok()?;
    Some(if negative { -offset } else { offset })
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&fmt::from_fn(|f| self.write_timestamp(f, 'T', "Z")))
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Add<SignedDuration> for Timestamp {
    type Output = Self;
    fn add(self, rhs: SignedDuration) -> Self {
        self.checked_add(rhs)
            .expect("timestamp addition out of range")
    }
}

impl Sub<SignedDuration> for Timestamp {
    type Output = Self;
    fn sub(self, rhs: SignedDuration) -> Self {
        self.checked_sub(rhs)
            .expect("timestamp subtraction out of range")
    }
}

impl Sub for Timestamp {
    type Output = SignedDuration;
    fn sub(self, rhs: Self) -> SignedDuration {
        let mut nanos = self.unix_timestamp_nanos() - rhs.unix_timestamp_nanos();
        if self.seconds > rhs.seconds && rhs.nanos >= BILLION {
            nanos += i128::from(BILLION);
        } else if self.seconds < rhs.seconds && self.nanos >= BILLION {
            nanos -= i128::from(BILLION);
        }
        SignedDuration::nanoseconds_i128(nanos)
    }
}

impl AddAssign<SignedDuration> for Timestamp {
    fn add_assign(&mut self, rhs: SignedDuration) {
        *self = *self + rhs;
    }
}

impl SubAssign<SignedDuration> for Timestamp {
    fn sub_assign(&mut self, rhs: SignedDuration) {
        *self = *self - rhs;
    }
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use bytes::BytesMut;
    use postgres_types::{FromSql, IsNull, ToSql, Type};
    use std::error::Error;

    impl<'a> FromSql<'a> for Timestamp {
        fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
            let ts = OffsetDateTime::from_sql(ty, raw)?;
            Self::from_timestamp(ts.unix_timestamp(), ts.nanosecond())
                .ok_or_else(|| "timestamp out of range".into())
        }
        postgres_types::accepts!(TIMESTAMPTZ);
    }

    impl ToSql for Timestamp {
        fn to_sql(
            &self,
            ty: &Type,
            out: &mut BytesMut,
        ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
            let ts = self.utc_second() + SignedDuration::nanoseconds(i64::from(self.nanos));
            ts.to_sql(ty, out)
        }
        postgres_types::accepts!(TIMESTAMPTZ);
        postgres_types::to_sql_checked!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_timestamp_strings_round_trip_at_each_precision() {
        for suffix in [
            "",
            ".100",
            ".001",
            ".123400",
            ".000001",
            ".123456700",
            ".000000001",
        ] {
            let json = format!("\"1969-12-31T23:59:59{suffix}Z\"");
            let sqlite = format!("1969-12-31T23:59:59{suffix}+00:00");
            let timestamp: Timestamp = serde_json::from_str(&json).unwrap();
            assert_eq!(timestamp.to_rfc3339(), sqlite);
            assert_eq!(sqlite.parse::<Timestamp>().unwrap(), timestamp);
            assert_eq!(serde_json::to_string(&timestamp).unwrap(), json);
            assert_eq!(timestamp.unix_timestamp(), -1);
        }
    }

    #[test]
    fn legacy_dates_offsets_and_leap_seconds_round_trip() {
        for (source, expected) in [
            ("+10000-01-01T00:00:00Z", "+10000-01-01T00:00:00Z"),
            ("-0001-12-31 23:59:59 UTC", "-0001-12-31T23:59:59Z"),
            ("2012-  12-12T12:  12:12Z", "2012-12-12T12:12:12Z"),
            ("2026-1-2 3:4:5−01:30", "2026-01-02T04:34:05Z"),
            ("2026-1-2 3:4:5-0130", "2026-01-02T04:34:05Z"),
            (
                "2017-01-01T05:29:60.123400+05:30",
                "2016-12-31T23:59:60.123400Z",
            ),
        ] {
            assert_eq!(
                serde_json::to_value(source.parse::<Timestamp>().unwrap()).unwrap(),
                expected
            );
        }
        let leap: Timestamp = "2016-12-31T23:59:60.100Z".parse().unwrap();
        let after: Timestamp = "2017-01-01T00:00:00.100Z".parse().unwrap();
        let before: Timestamp = "2016-12-31T23:59:59.100Z".parse().unwrap();
        assert!(before < leap && leap < after);
        assert_eq!(leap - before, SignedDuration::SECOND);
        assert_eq!(after - leap, SignedDuration::SECOND);
        assert_eq!(leap + SignedDuration::SECOND, after);
        assert_eq!(leap - SignedDuration::SECOND, before);
        assert_eq!(
            Timestamp::MIN.to_string().parse::<Timestamp>().unwrap(),
            Timestamp::MIN
        );
    }

    #[test]
    fn offsets_cross_midnight_and_normalize_to_utc() {
        let instant: Timestamp = "2026-01-01T01:02:03.1+05:30".parse().unwrap();
        assert_eq!(instant.to_string(), "2025-12-31 19:32:03.100 UTC");
        assert_eq!(
            serde_json::to_value(instant).unwrap(),
            "2025-12-31T19:32:03.100Z"
        );
        for invalid in [
            "2026-02-30T00:00:00Z",
            "2026-01-01T00:00:00U0000",
            "2026-01-01T00:00:00+24:00",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00:61Z",
        ] {
            assert!(invalid.parse::<Timestamp>().is_err(), "{invalid}");
        }
    }
}
