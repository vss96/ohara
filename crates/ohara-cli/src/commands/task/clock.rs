//! Parsing of `--for` / `--until` and the budget-in-`detail` encoding.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Local, NaiveTime, Utc};

/// How long a task may run, as the user stated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// A duration counted from approval (spec section 6).
    For(Duration),
    /// An absolute deadline.
    Until(DateTime<Utc>),
}

const FOR_PREFIX: &str = "budget_for_secs=";

/// Parse `90m`, `2h`, `1h30m`, `45s`, `1d`. Units: `s`, `m`, `h`, `d`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let mut total = Duration::zero();
    let mut digits = String::new();
    let mut any = false;
    for ch in s.trim().chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let n: i64 = digits
            .parse()
            .with_context(|| format!("invalid duration {s:?}: expected e.g. 90m, 2h, 1h30m"))?;
        digits.clear();
        let unit = match ch {
            's' => Duration::seconds(n),
            'm' => Duration::minutes(n),
            'h' => Duration::hours(n),
            'd' => Duration::days(n),
            other => bail!("invalid duration {s:?}: unknown unit {other:?}"),
        };
        total += unit;
        any = true;
    }
    if !digits.is_empty() || !any {
        bail!("invalid duration {s:?}: expected e.g. 90m, 2h, 1h30m");
    }
    if total <= Duration::zero() {
        bail!("invalid duration {s:?}: must be positive");
    }
    Ok(total)
}

/// Parse `--until`: an RFC 3339 timestamp, or `HH:MM` today in local time.
/// A time that is not after `now` is an error.
pub fn parse_until(s: &str, now: DateTime<Utc>) -> Result<Budget> {
    let at = match DateTime::parse_from_rfc3339(s) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => {
            let t = NaiveTime::parse_from_str(s, "%H:%M")
                .with_context(|| format!("invalid --until {s:?}: expected RFC 3339 or HH:MM"))?;
            let local = Local::now().date_naive().and_time(t);
            let zoned = local
                .and_local_timezone(Local)
                .single()
                .with_context(|| format!("--until {s:?} is ambiguous in the local time zone"))?;
            zoned.with_timezone(&Utc)
        }
    };
    if at <= now {
        bail!("--until {s} is in the past");
    }
    Ok(Budget::Until(at))
}

/// Encode a `--for` duration for `TaskRecord.detail` until approval.
pub fn encode_for(d: Duration, note: Option<&str>) -> String {
    let head = format!("{FOR_PREFIX}{}", d.num_seconds());
    match note {
        Some(n) => format!("{head}; {n}"),
        None => head,
    }
}

/// Split `detail` into the pending `--for` duration and any free-text note.
pub fn split_detail(detail: Option<&str>) -> (Option<Duration>, Option<String>) {
    let Some(text) = detail else {
        return (None, None);
    };
    let Some(rest) = text.strip_prefix(FOR_PREFIX) else {
        return (None, Some(text.to_string()));
    };
    let (secs, note) = match rest.split_once("; ") {
        Some((s, n)) => (s, Some(n.to_string())),
        None => (rest, None),
    };
    (secs.parse().ok().map(Duration::seconds), note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90m").unwrap(), Duration::minutes(90));
        assert_eq!(parse_duration("2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_duration("1d").unwrap(), Duration::days(1));
        for bad in ["", "h", "5", "0m", "1x", "1h5"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hh_mm_in_the_past_or_malformed_errors() {
        assert!(parse_until("25:99", Utc::now()).is_err());
        let far = Utc::now() + Duration::days(2);
        assert!(parse_until("00:00", far).is_err());
    }

    #[test]
    fn detail_round_trip() {
        let d = encode_for(Duration::hours(2), Some("fell back"));
        let (dur, note) = split_detail(Some(&d));
        assert_eq!(dur, Some(Duration::hours(2)));
        assert_eq!(note.as_deref(), Some("fell back"));
        assert_eq!(split_detail(Some("plain")), (None, Some("plain".into())));
        assert_eq!(split_detail(None), (None, None));
    }
}
