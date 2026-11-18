//! Timestamp formatting, parsing, and the effective-date rule. See README "Item records".

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration, NaiveDate, SecondsFormat, TimeZone, Utc};

/// Current time, truncated to whole seconds so stored values round-trip exactly.
pub fn now() -> DateTime<Utc> {
    let t = Utc::now();
    Utc.timestamp_opt(t.timestamp(), 0).single().unwrap_or(t)
}

/// UTC RFC 3339 with second precision: `2026-11-17T12:00:00Z`. Sorts lexically.
pub fn fmt(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_stored(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Parse a duration like `90s`, `30m`, `12h`, `3d`, `2w`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow!("invalid duration {s:?} (expected e.g. 30m, 12h, 3d, 1w)"))?;
    let d = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => bail!("invalid duration {s:?} (units: s, m, h, d, w)"),
    };
    Ok(d)
}

/// Parse an absolute cutoff: RFC 3339, or a bare `YYYY-MM-DD` meaning UTC midnight.
pub fn parse_timestamp(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
    }
    bail!("invalid timestamp {s:?} (expected RFC 3339, e.g. 2026-11-17T12:00:00Z, or YYYY-MM-DD)")
}

/// How far past discovery a publisher date may be before it is treated as bogus.
const FUTURE_TOLERANCE_HOURS: i64 = 1;

fn credible(t: DateTime<Utc>, first_seen: DateTime<Utc>) -> bool {
    let floor = Utc.with_ymd_and_hms(1995, 1, 1, 0, 0, 0).unwrap();
    t >= floor && t <= first_seen + Duration::hours(FUTURE_TOLERANCE_HOURS)
}

/// The effective story date and its basis: `published`, `updated`, or `discovered`.
pub fn effective(
    published: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
    first_seen: DateTime<Utc>,
) -> (DateTime<Utc>, &'static str) {
    match (published, updated) {
        (Some(p), _) if credible(p, first_seen) => (p, "published"),
        (None, Some(u)) if credible(u, first_seen) => (u, "updated"),
        _ => (first_seen, "discovered"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        parse_timestamp(s).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3d").unwrap(), Duration::days(3));
        assert_eq!(parse_duration("90s").unwrap(), Duration::seconds(90));
        assert_eq!(parse_duration("2w").unwrap(), Duration::days(14));
        assert!(parse_duration("3").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("3y").is_err());
    }

    #[test]
    fn timestamps() {
        assert_eq!(fmt(t("2026-11-17")), "2026-11-17T00:00:00Z");
        assert_eq!(fmt(t("2026-11-17T14:00:00+02:00")), "2026-11-17T12:00:00Z");
        assert!(parse_timestamp("yesterday").is_err());
    }

    #[test]
    fn effective_date_rules() {
        let seen = t("2026-11-17T12:00:00Z");
        let p = t("2026-11-16T08:00:00Z");
        let u = t("2026-11-16T09:00:00Z");
        assert_eq!(effective(Some(p), Some(u), seen), (p, "published"));
        assert_eq!(effective(None, Some(u), seen), (u, "updated"));
        assert_eq!(effective(None, None, seen), (seen, "discovered"));
        // Slightly in the future (clock skew) is tolerated.
        let skew = t("2026-11-17T12:30:00Z");
        assert_eq!(effective(Some(skew), None, seen), (skew, "published"));
        // Implausibly future-dated or ancient dates fall back to discovery.
        assert_eq!(
            effective(Some(t("2027-01-01")), None, seen),
            (seen, "discovered")
        );
        assert_eq!(
            effective(Some(t("1970-01-01")), None, seen),
            (seen, "discovered")
        );
        // A bogus publish date does not fall through to `updated`.
        assert_eq!(
            effective(Some(t("2027-01-01")), Some(u), seen),
            (seen, "discovered")
        );
    }
}
