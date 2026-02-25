use crate::cron::Schedule;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cron::Schedule as CronExprSchedule;
use std::str::FromStr;

pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let normalized = normalize_expression(expr)?;
            let cron = CronExprSchedule::from_str(&normalized)
                .with_context(|| format!("Invalid cron expression: {expr}"))?;

            if let Some(tz_name) = tz {
                let timezone = chrono_tz::Tz::from_str(tz_name)
                    .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
                let localized_from = from.with_timezone(&timezone);
                let next_local = cron.after(&localized_from).next().ok_or_else(|| {
                    anyhow::anyhow!("No future occurrence for expression: {expr}")
                })?;
                Ok(next_local.with_timezone(&Utc))
            } else {
                cron.after(&from)
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))
            }
        }
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = ChronoDuration::milliseconds(ms);
            from.checked_add_signed(delta)
                .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
        }
    }
}

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                anyhow::bail!("Invalid schedule: 'at' must be in the future");
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        // The `cron` crate uses 1-based DOW (1=Sun..7=Sat) while standard
        // crontab uses 0-based (0=Sun..6=Sat).  Shift the DOW field so that
        // user-supplied crontab expressions behave as expected.
        5 => {
            let mut fields: Vec<&str> = expression.split_whitespace().collect();
            let shifted_dow = shift_dow_field(fields[4])?;
            fields[4] = &shifted_dow;
            Ok(format!("0 {}", fields.join(" ")))
        }
        // crate-native syntax includes seconds (+ optional year) — pass through
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"
        ),
    }
}

/// Shift a crontab DOW field from 0-based (0=Sun) to the `cron` crate's
/// 1-based (1=Sun) numbering.  Handles `*`, single values, ranges, steps,
/// and comma-separated lists.
fn shift_dow_field(field: &str) -> Result<String> {
    // Wildcards need no adjustment.
    if field == "*" || field == "?" {
        return Ok(field.to_string());
    }

    // Split on commas first to handle lists like "1,3,5".
    let parts: Vec<&str> = field.split(',').collect();
    let mut shifted_parts = Vec::with_capacity(parts.len());

    for part in parts {
        // Handle step suffix: "1-5/2" → base="1-5", step="/2"
        let (base, step) = if let Some(idx) = part.find('/') {
            (&part[..idx], &part[idx..])
        } else {
            (part, "")
        };

        let shifted_base = if base == "*" {
            "*".to_string()
        } else if let Some((lo, hi)) = base.split_once('-') {
            let lo_n = shift_dow_value(lo)?;
            let hi_n = shift_dow_value(hi)?;
            format!("{lo_n}-{hi_n}")
        } else {
            let n = shift_dow_value(base)?;
            n.to_string()
        };

        shifted_parts.push(format!("{shifted_base}{step}"));
    }

    Ok(shifted_parts.join(","))
}

fn shift_dow_value(val: &str) -> Result<u8> {
    let n: u8 = val
        .parse()
        .with_context(|| format!("Invalid day-of-week value: {val}"))?;
    if n > 7 {
        anyhow::bail!("Day-of-week value out of range: {n}");
    }
    // Standard crontab: 0=Sun, 7=Sun.  Crate: 1=Sun, 7=Sat.
    // 0 → 1, 1 → 2, ..., 6 → 7, 7 → 1
    Ok(if n == 0 || n == 7 { 1 } else { n + 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn next_run_for_schedule_supports_every_and_at() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);

        let at = now + ChronoDuration::minutes(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_for_schedule_supports_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };

        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
    }

    #[test]
    fn crontab_dow_1_5_means_mon_fri() {
        // Feb 20 2026 is a FRIDAY.  Crontab "1-5" = Mon-Fri.
        // Start from Thursday 23:59 UTC; next should be Friday.
        let thursday_late = Utc.with_ymd_and_hms(2026, 2, 19, 23, 59, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "30 7 * * 1-5".into(),
            tz: Some("America/Chicago".into()),
        };
        let next = next_run_for_schedule(&schedule, thursday_late).unwrap();
        // Friday Feb 20 at 07:30 CST = 13:30 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 20, 13, 30, 0).unwrap());
    }

    #[test]
    fn crontab_dow_0_is_sunday() {
        // Crontab "0" = Sunday.  Feb 22 2026 is Sunday.
        let saturday = Utc.with_ymd_and_hms(2026, 2, 21, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 12 * * 0".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, saturday).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 22, 12, 0, 0).unwrap());
    }

    #[test]
    fn crontab_dow_6_is_saturday() {
        // Crontab "6" = Saturday.  Feb 21 2026 is Saturday.
        let friday = Utc.with_ymd_and_hms(2026, 2, 20, 23, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * 6".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, friday).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 21, 9, 0, 0).unwrap());
    }

    #[test]
    fn shift_dow_field_handles_lists_and_steps() {
        assert_eq!(shift_dow_field("*").unwrap(), "*");
        assert_eq!(shift_dow_field("0").unwrap(), "1"); // Sun
        assert_eq!(shift_dow_field("5").unwrap(), "6"); // Fri
        assert_eq!(shift_dow_field("1-5").unwrap(), "2-6"); // Mon-Fri
        assert_eq!(shift_dow_field("0,6").unwrap(), "1,7"); // Sun,Sat
        assert_eq!(shift_dow_field("*/2").unwrap(), "*/2"); // every 2nd day
        assert_eq!(shift_dow_field("1-5/2").unwrap(), "2-6/2");
        assert_eq!(shift_dow_field("7").unwrap(), "1"); // 7=Sun alias
    }
}
