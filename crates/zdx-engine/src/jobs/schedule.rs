//! Job schedules: manual only, recurring (5-field cron evaluated in the
//! daemon's local time), or once (one instant with an explicit UTC offset).
//!
//! Each scheduled run is an *occurrence* with a stable key. The key is what a
//! claim records, so it decides identity across ticks, restarts, and manual
//! runs:
//! - once: the instant in UTC (`20261020T120000Z`);
//! - recurring: the local wall-clock minute (`20261020T0900`), so a local time
//!   repeated by a DST fall-back runs once, and a local time skipped by a
//!   spring-forward never matches.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, TimeDelta, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

/// Default grace for a once job: how late it may still start.
pub const DEFAULT_ONCE_GRACE_SECS: u32 = 6 * 60 * 60;
/// Default grace for a recurring job (latest missed occurrence only).
pub const DEFAULT_RECURRING_GRACE_SECS: u32 = 10 * 60;
/// Smallest accepted `grace_secs`; below one daemon poll it would miss runs.
pub const MIN_GRACE_SECS: u32 = 60;
/// Largest accepted `grace_secs`.
pub const MAX_GRACE_SECS: u32 = 7 * 24 * 60 * 60;

/// How a job is triggered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSchedule {
    /// Runs only with `zdx jobs run`.
    Manual,
    /// Runs every time the cron expression matches the daemon's local time.
    Recurring {
        cron: String,
        #[serde(skip)]
        parsed: Cron,
    },
    /// Runs once at `run_at`, then the job file is archived.
    Once { run_at: DateTime<FixedOffset> },
}

/// The kind of a [`JobSchedule`], for records and filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    Manual,
    Recurring,
    Once,
}

impl ScheduleKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Recurring => "recurring",
            Self::Once => "once",
        }
    }
}

impl JobSchedule {
    /// Builds the schedule from the `schedule` and `run_at` frontmatter keys.
    ///
    /// # Errors
    /// Returns an error when both keys are set, the cron expression is
    /// invalid, or `run_at` is not RFC 3339 with an offset.
    pub fn parse(schedule: Option<&str>, run_at: Option<&str>) -> Result<Self> {
        match (schedule, run_at) {
            (Some(_), Some(_)) => {
                bail!("set either `schedule` (recurring) or `run_at` (once), not both")
            }
            (Some(cron), None) => Ok(Self::Recurring {
                parsed: Cron::parse(cron)?,
                cron: cron.to_string(),
            }),
            (None, Some(raw)) => Ok(Self::Once {
                run_at: parse_run_at(raw)?,
            }),
            (None, None) => Ok(Self::Manual),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ScheduleKind {
        match self {
            Self::Manual => ScheduleKind::Manual,
            Self::Recurring { .. } => ScheduleKind::Recurring,
            Self::Once { .. } => ScheduleKind::Once,
        }
    }

    /// Short human label: the cron expression, `once <run_at>`, or `manual`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Manual => "manual".to_string(),
            Self::Recurring { cron, .. } => cron.clone(),
            Self::Once { run_at } => format!("once {}", run_at.to_rfc3339()),
        }
    }

    /// Grace that applies when `grace_secs` is not declared; `None` for
    /// manual jobs, which are never scheduled.
    #[must_use]
    pub const fn default_grace_secs(&self) -> Option<u32> {
        match self {
            Self::Manual => None,
            Self::Recurring { .. } => Some(DEFAULT_RECURRING_GRACE_SECS),
            Self::Once { .. } => Some(DEFAULT_ONCE_GRACE_SECS),
        }
    }
}

fn parse_run_at(raw: &str) -> Result<DateTime<FixedOffset>> {
    let run_at = DateTime::parse_from_rfc3339(raw.trim()).with_context(|| {
        format!(
            "invalid run_at '{raw}': use RFC 3339 with an explicit offset, for example \
             2026-10-20T09:00:00-03:00 or 2026-10-20T12:00:00Z"
        )
    })?;
    // Occurrence keys have whole-second precision.
    if run_at.timestamp_subsec_nanos() != 0 {
        bail!("invalid run_at '{raw}': use whole seconds");
    }
    Ok(run_at)
}

/// One scheduled run of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// Scheduled instant.
    pub at: DateTime<Utc>,
    /// Stable identity of this occurrence (see the module docs).
    pub key: String,
}

/// The single occurrence of a once job.
#[must_use]
pub fn once_occurrence(run_at: DateTime<FixedOffset>) -> Occurrence {
    let at = run_at.with_timezone(&Utc);
    Occurrence {
        key: at.format("%Y%m%dT%H%M%SZ").to_string(),
        at,
    }
}

/// Occurrences of `cron` in the UTC window `(after, until]`, matched against
/// local time in `tz`, oldest first, at most one per local minute key.
#[must_use]
pub fn recurring_occurrences<Tz: TimeZone>(
    cron: &Cron,
    after: DateTime<Utc>,
    until: DateTime<Utc>,
    tz: &Tz,
) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut at = floor_minute(after) + TimeDelta::minutes(1);
    while at <= until {
        let local = at.with_timezone(tz).naive_local();
        if cron.matches(local) {
            let key = local.format("%Y%m%dT%H%M").to_string();
            if seen.insert(key.clone()) {
                out.push(Occurrence { at, key });
            }
        }
        at += TimeDelta::minutes(1);
    }
    out
}

/// Next occurrence of `cron` strictly after `after`, looking up to a year
/// ahead. `None` when the expression never matches in that span.
#[must_use]
pub fn next_recurring_after<Tz: TimeZone>(
    cron: &Cron,
    after: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    let limit = after + TimeDelta::days(367);
    let mut at = floor_minute(after) + TimeDelta::minutes(1);
    while at <= limit {
        let local = at.with_timezone(tz).naive_local();
        if !cron.matches_day(local) || !cron.matches_hour(local) {
            // Step to the next local hour boundary. Offsets change by whole
            // hours here, so no matching minute is skipped.
            at += TimeDelta::minutes(60 - i64::from(local.minute()));
            continue;
        }
        if cron.matches(local) {
            return Some(at);
        }
        at += TimeDelta::minutes(1);
    }
    None
}

fn floor_minute(at: DateTime<Utc>) -> DateTime<Utc> {
    let ts = at.timestamp();
    DateTime::from_timestamp(ts - ts.rem_euclid(60), 0).unwrap_or(at)
}

/// A parsed 5-field cron expression: minute hour day-of-month month
/// day-of-week. Each field accepts `*`, `N`, `*/N`, `A-B`, and comma lists of
/// those. Day-of-week is 0-6 (Sunday is 0; 7 is also Sunday). Day-of-month and
/// day-of-week must both match.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
}

impl Cron {
    /// # Errors
    /// Returns an error for a wrong field count, an invalid value, or a value
    /// out of range.
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            bail!(
                "Invalid schedule '{expr}': expected 5 cron fields (minute hour day month weekday)"
            );
        }
        let parse = |field: &str, min, max, is_dow| {
            parse_field(field, min, max, is_dow)
                .with_context(|| format!("Invalid schedule '{expr}'"))
        };
        Ok(Self {
            minutes: parse(fields[0], 0, 59, false)?,
            hours: parse(fields[1], 0, 23, false)?,
            days: parse(fields[2], 1, 31, false)?,
            months: parse(fields[3], 1, 12, false)?,
            weekdays: parse(fields[4], 0, 6, true)?,
        })
    }

    /// Whether the expression matches this local wall-clock minute.
    #[must_use]
    pub fn matches(&self, local: NaiveDateTime) -> bool {
        self.matches_day(local) && self.matches_hour(local) && bit(self.minutes, local.minute())
    }

    fn matches_day(&self, local: NaiveDateTime) -> bool {
        bit(self.days, local.day())
            && bit(self.months, local.month())
            && bit(self.weekdays, local.weekday().num_days_from_sunday())
    }

    fn matches_hour(&self, local: NaiveDateTime) -> bool {
        bit(self.hours, local.hour())
    }
}

const fn bit(mask: u64, value: u32) -> bool {
    value < 64 && mask & (1 << value) != 0
}

fn parse_field(expr: &str, min: u32, max: u32, is_dow: bool) -> Result<u64> {
    // Day-of-week accepts 7 as Sunday in single values and ranges (`1-7`);
    // it is folded onto 0 after expansion.
    let value_max = if is_dow { 7 } else { max };
    let mut mask = 0u64;
    let parts: Vec<&str> = expr.split(',').map(str::trim).collect();
    if parts.iter().all(|part| part.is_empty()) {
        bail!("Invalid empty cron field");
    }
    for part in parts.into_iter().filter(|part| !part.is_empty()) {
        if part == "*" {
            for value in min..=max {
                mask |= 1 << value;
            }
        } else if let Some(step) = part.strip_prefix("*/") {
            let step = parse_value(step, 1, value_max)?;
            for value in (min..=max).step_by(step as usize) {
                mask |= 1 << value;
            }
        } else if let Some((start, end)) = part.split_once('-') {
            let start = parse_value(start, min, value_max)?;
            let end = parse_value(end, min, value_max)?;
            if start > end {
                bail!("Invalid range '{part}': start must be <= end");
            }
            for value in start..=end {
                mask |= 1 << value;
            }
        } else {
            mask |= 1 << parse_value(part, min, value_max)?;
        }
    }
    if is_dow && mask & (1 << 7) != 0 {
        mask = (mask & !(1 << 7)) | 1;
    }
    Ok(mask)
}

fn parse_value(raw: &str, min: u32, max: u32) -> Result<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Invalid empty cron value");
    }
    let value = trimmed
        .parse::<u32>()
        .with_context(|| format!("Invalid cron value '{trimmed}'"))?;
    if !(min..=max).contains(&value) {
        bail!("Cron value '{trimmed}' out of range {min}..{max}");
    }
    Ok(value)
}

#[cfg(test)]
pub(crate) mod test_zones {
    //! A test time zone with one DST change, so DST behavior is tested
    //! without touching the process `TZ`.

    use chrono::{
        FixedOffset, LocalResult, MappedLocalTime, NaiveDate, NaiveDateTime, Offset, TimeZone,
    };

    /// UTC-3 before `switch_utc`, `after_offset_hours` from then on.
    #[derive(Debug, Clone, Copy)]
    pub struct OneSwitch {
        pub switch_utc: NaiveDateTime,
        pub before_hours: i32,
        pub after_hours: i32,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct SwitchOffset(FixedOffset);

    impl Offset for SwitchOffset {
        fn fix(&self) -> FixedOffset {
            self.0
        }
    }

    impl std::fmt::Display for SwitchOffset {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl OneSwitch {
        fn offset(hours: i32) -> FixedOffset {
            FixedOffset::east_opt(hours * 3600).unwrap()
        }
    }

    impl TimeZone for OneSwitch {
        type Offset = SwitchOffset;

        fn from_offset(_offset: &SwitchOffset) -> Self {
            unreachable!("not used by the scheduler")
        }

        fn offset_from_local_date(&self, _local: &NaiveDate) -> MappedLocalTime<SwitchOffset> {
            unreachable!("not used by the scheduler")
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<SwitchOffset> {
            let before = Self::offset(self.before_hours);
            let after = Self::offset(self.after_hours);
            let valid = |offset: FixedOffset| {
                let utc = *local - offset;
                (utc < self.switch_utc) == (offset == before)
            };
            match (valid(before), valid(after)) {
                (true, true) => LocalResult::Ambiguous(SwitchOffset(before), SwitchOffset(after)),
                (true, false) => LocalResult::Single(SwitchOffset(before)),
                (false, true) => LocalResult::Single(SwitchOffset(after)),
                (false, false) => LocalResult::None,
            }
        }

        fn offset_from_utc_date(&self, _utc: &NaiveDate) -> SwitchOffset {
            unreachable!("not used by the scheduler")
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> SwitchOffset {
            if *utc < self.switch_utc {
                SwitchOffset(Self::offset(self.before_hours))
            } else {
                SwitchOffset(Self::offset(self.after_hours))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::test_zones::OneSwitch;
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn cron_matches_like_before() {
        let daily = Cron::parse("0 8 * * *").unwrap();
        assert!(daily.matches(local(2026, 2, 11, 8, 0)));
        assert!(!daily.matches(local(2026, 2, 11, 8, 1)));
        let stepped = Cron::parse("*/15 8-10 * * *").unwrap();
        assert!(stepped.matches(local(2026, 2, 11, 8, 30)));
        assert!(
            !Cron::parse("*/20 8-10 * * *")
                .unwrap()
                .matches(local(2026, 2, 11, 8, 30))
        );
        let sunday = Cron::parse("0 9 * * 7").unwrap();
        assert!(sunday.matches(local(2026, 10, 18, 9, 0)));
        let week = Cron::parse("0 9 * * 1-7").unwrap();
        assert!(
            week.matches(local(2026, 10, 18, 9, 0)),
            "Sunday as 7 in a range"
        );
        assert!(week.matches(local(2026, 10, 19, 9, 0)));
        let everyday = Cron::parse("0 9 * * 0-7").unwrap();
        assert!(everyday.matches(local(2026, 10, 21, 9, 0)));
    }

    #[test]
    fn cron_rejects_bad_expressions() {
        for (expr, message) in [
            ("0 8 * *", "expected 5 cron fields"),
            ("60 8 * * *", "out of range"),
            ("*/0 * * * *", "out of range"),
            ("5-1 * * * *", "start must be <= end"),
            ("x * * * *", "Invalid cron value"),
        ] {
            let err = Cron::parse(expr).unwrap_err();
            assert!(format!("{err:#}").contains(message), "{expr}: {err:#}");
        }
    }

    #[test]
    fn schedule_keys_are_exclusive_and_run_at_needs_an_offset() {
        assert_eq!(JobSchedule::parse(None, None).unwrap(), JobSchedule::Manual);
        assert!(matches!(
            JobSchedule::parse(None, Some("2026-10-20T09:00:00-03:00")).unwrap(),
            JobSchedule::Once { .. }
        ));
        assert!(JobSchedule::parse(None, Some("2026-10-20T12:00:00Z")).is_ok());
        let err = JobSchedule::parse(None, Some("2026-10-20T09:00:00.5Z")).unwrap_err();
        assert!(format!("{err:#}").contains("whole seconds"), "{err:#}");
        let err = JobSchedule::parse(None, Some("2026-10-20T09:00:00")).unwrap_err();
        assert!(format!("{err:#}").contains("explicit offset"), "{err:#}");
        let err = JobSchedule::parse(Some("0 9 * * *"), Some("2026-10-20T09:00:00Z")).unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    fn once_key_is_the_utc_instant() {
        let JobSchedule::Once { run_at } =
            JobSchedule::parse(None, Some("2026-10-20T09:00:00-03:00")).unwrap()
        else {
            panic!("once");
        };
        let occurrence = once_occurrence(run_at);
        assert_eq!(occurrence.key, "20261020T120000Z");
        assert_eq!(occurrence.at, utc(2026, 10, 20, 12, 0));
    }

    #[test]
    fn occurrences_cover_a_half_open_window_in_local_time() {
        let tz = FixedOffset::west_opt(3 * 3600).unwrap();
        let cron = Cron::parse("*/30 9 * * *").unwrap();
        let found = recurring_occurrences(
            &cron,
            utc(2026, 10, 20, 12, 0),
            utc(2026, 10, 20, 13, 0),
            &tz,
        );
        let keys: Vec<_> = found.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            ["20261020T0930"],
            "start is exclusive, 10:00 local does not match"
        );
        assert_eq!(found[0].at, utc(2026, 10, 20, 12, 30));
    }

    #[test]
    fn dst_fall_back_runs_a_repeated_local_minute_once() {
        // Local clocks go from UTC-2 back to UTC-3 at 02:00 UTC, so local
        // 23:00-23:59 happens twice.
        let tz = OneSwitch {
            switch_utc: local(2026, 2, 22, 2, 0),
            before_hours: -2,
            after_hours: -3,
        };
        let cron = Cron::parse("30 23 * * *").unwrap();
        let found =
            recurring_occurrences(&cron, utc(2026, 2, 22, 0, 0), utc(2026, 2, 22, 4, 0), &tz);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "20260221T2330");
    }

    #[test]
    fn dst_spring_forward_skips_a_missing_local_minute() {
        // Local clocks go from UTC-3 to UTC-2 at 03:00 UTC: 00:00-00:59 local
        // does not exist that day.
        let tz = OneSwitch {
            switch_utc: local(2026, 11, 1, 3, 0),
            before_hours: -3,
            after_hours: -2,
        };
        let cron = Cron::parse("30 0 * * *").unwrap();
        let found =
            recurring_occurrences(&cron, utc(2026, 11, 1, 0, 0), utc(2026, 11, 1, 6, 0), &tz);
        assert!(found.is_empty(), "{found:?}");
        let next = next_recurring_after(&cron, utc(2026, 11, 1, 0, 0), &tz).unwrap();
        assert_eq!(next, utc(2026, 11, 2, 2, 30));
    }

    #[test]
    fn next_occurrence_skips_ahead_and_gives_up_on_impossible_dates() {
        let tz = FixedOffset::west_opt(3 * 3600).unwrap();
        let weekly = Cron::parse("0 9 * * 1").unwrap();
        let next = next_recurring_after(&weekly, utc(2026, 10, 13, 12, 0), &tz).unwrap();
        assert_eq!(next, utc(2026, 10, 19, 12, 0));
        let never = Cron::parse("0 9 30 2 *").unwrap();
        assert!(next_recurring_after(&never, utc(2026, 10, 13, 12, 0), &tz).is_none());
    }
}
