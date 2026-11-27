use fuzukue_proto::UsageWindow;

pub struct UsageOutlook {
    /// None when the reset is less than one minute away.
    pub reset_minutes: Option<i64>,
    pub percent_per_hour: Option<String>,
    pub estimate: Option<UsageEstimate>,
}

pub struct UsageEstimate {
    pub minutes: i64,
    pub reaches_reset: bool,
}

/// Display calculations for a window with a known, future reset.
pub fn outlook(window: &UsageWindow, now: i64) -> Option<UsageOutlook> {
    let reset_at = window.resets_at?;
    let reset_in = reset_at.saturating_sub(now);
    if reset_in <= 0 {
        return None;
    }

    let mut outlook = UsageOutlook {
        reset_minutes: (reset_in >= 60).then(|| crate::minutes_until(reset_at, now)),
        percent_per_hour: None,
        estimate: None,
    };
    if reset_in < 60 || (window.rejected && window.used < 1.0)
        || !window.used.is_finite() || window.used < 0.0
    {
        return Some(outlook);
    }

    let remaining = (1.0 - f64::from(window.used)).max(0.0);
    let percent_per_hour = remaining * 360_000.0 / reset_in as f64;
    outlook.percent_per_hour = Some(if percent_per_hour > 0.0 && percent_per_hour < 0.1 {
        "<0.1".into()
    } else {
        format!("{percent_per_hour:.1}").trim_end_matches(".0").to_owned()
    });

    if !window.rejected {
        outlook.estimate = window.recent_pace
            .and_then(|pace| pace.runway_seconds(window.used, now))
            .map(|seconds| UsageEstimate {
                minutes: if seconds > 0.0 { (seconds / 60.0).round().max(1.0) as i64 } else { 0 },
                reaches_reset: seconds >= reset_in as f64,
            });
    }
    Some(outlook)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_proto::UsagePace;

    const NOW: i64 = 100_000;

    fn window(used: f32, reset_in: Option<i64>) -> UsageWindow {
        UsageWindow {
            agent: "codex".into(),
            name: "weekly".into(),
            used,
            resets_at: reset_in.map(|seconds| NOW + seconds),
            rejected: false,
            recent_pace: None,
        }
    }

    #[test]
    fn allowance_uses_time_left_and_omits_short_or_unknown_windows() {
        let look = |used, seconds| outlook(&window(used, seconds), NOW);
        assert_eq!(look(0.89, Some(900)).unwrap().percent_per_hour.as_deref(), Some("44"));
        assert_eq!(look(1.01, Some(900)).unwrap().percent_per_hour.as_deref(), Some("0"));
        assert_eq!(look(0.99, Some(7 * 86_400)).unwrap().percent_per_hour.as_deref(), Some("<0.1"));
        assert!(look(0.89, Some(59)).unwrap().percent_per_hour.is_none());
        assert!(look(0.89, Some(0)).is_none());
        assert!(look(0.89, Some(-1)).is_none());
        assert!(look(0.89, None).is_none());
    }

    #[test]
    fn estimate_compares_against_reset_and_expires() {
        let mut window = window(0.75, Some(900));
        window.recent_pace = Some(UsagePace { observed_at: NOW, used_per_hour: 0.5 });
        let estimate = outlook(&window, NOW).unwrap().estimate.unwrap();
        assert_eq!(estimate.minutes, 30);
        assert!(estimate.reaches_reset);

        window.recent_pace = Some(UsagePace { observed_at: NOW, used_per_hour: 2.0 });
        let estimate = outlook(&window, NOW).unwrap().estimate.unwrap();
        assert_eq!(estimate.minutes, 8);
        assert!(!estimate.reaches_reset);

        window.used = 1.0;
        let estimate = outlook(&window, NOW).unwrap().estimate.unwrap();
        assert_eq!(estimate.minutes, 0);
        assert!(!estimate.reaches_reset);

        window.resets_at = Some(NOW + 7200);
        assert!(outlook(&window, NOW + UsagePace::MAX_AGE_SECS + 1).unwrap().estimate.is_none());
        assert!(outlook(&window, NOW - 1).unwrap().estimate.is_none());
        window.used = 0.75;
        window.rejected = true;
        assert!(outlook(&window, NOW).unwrap().percent_per_hour.is_none());
    }
}
