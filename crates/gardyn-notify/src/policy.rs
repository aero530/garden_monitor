//! When to interrupt someone, and how loudly.
//!
//! This is the part that decides whether the system gets muted. The rules already
//! know *what* needs doing; the whole job here is to say it the right number of times.
//! Pure functions over stored state so the whole policy is testable without sending
//! anything anywhere.

use gardyn_core::Severity;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Which channels a severity reaches.
///
/// SMS was ruled out, so the top of the ladder is ntfy priority 5 (`max`), which
/// bypasses Do Not Disturb on both iOS and Android. That covers "the tank is dry in
/// twelve hours" without a Twilio bill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reach {
    pub push: bool,
    pub email: bool,
    /// ntfy priority, 1-5.
    pub priority: u8,
    /// Whether this may fire outside the daily brief at all.
    pub interrupts: bool,
}

pub fn reach_for(severity: Severity) -> Reach {
    match severity {
        // Never pushed. Shows up in the app and the daily brief only.
        Severity::Info => Reach {
            push: false,
            email: false,
            priority: 1,
            interrupts: false,
        },
        // Batched into the morning brief rather than pinged.
        Severity::Advisory => Reach {
            push: false,
            email: false,
            priority: 2,
            interrupts: false,
        },
        Severity::Important => Reach {
            push: true,
            email: false,
            priority: 3,
            interrupts: true,
        },
        Severity::Urgent => Reach {
            push: true,
            email: true,
            priority: 4,
            interrupts: true,
        },
        Severity::Critical => Reach {
            push: true,
            email: true,
            priority: 5,
            interrupts: true,
        },
    }
}

/// A window during which non-critical notifications are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Local hour the quiet window opens, 0-23.
    pub from_hour: u8,
    /// Local hour it closes.
    pub to_hour: u8,
}

impl Default for QuietHours {
    fn default() -> Self {
        Self {
            from_hour: 21,
            to_hour: 7,
        }
    }
}

impl QuietHours {
    /// Whether `hour` falls inside the window.
    ///
    /// Handles the wrap across midnight, which is the normal case: 21:00 to 07:00 is
    /// one window, not an empty one.
    pub fn contains(&self, hour: u8) -> bool {
        if self.from_hour == self.to_hour {
            // Degenerate; treat as never quiet rather than always.
            return false;
        }
        if self.from_hour < self.to_hour {
            (self.from_hour..self.to_hour).contains(&hour)
        } else {
            hour >= self.from_hour || hour < self.to_hour
        }
    }

    /// Whether a notification of this severity may be delivered now.
    ///
    /// Critical always goes through. The point of a quiet window is to stop the
    /// system waking you about a root check; it is not to let the tank run dry
    /// overnight because it happened to cross the threshold at 2am.
    pub fn permits(&self, severity: Severity, local_hour: u8) -> bool {
        severity == Severity::Critical || !self.contains(local_hour)
    }
}

/// What the dispatcher last did about a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastNotified {
    pub at: Timestamp,
    pub severity: Severity,
}

/// Whether a task warrants sending something right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing has been sent about this task before.
    First,
    /// It got worse since the last send.
    Escalated,
    /// Still outstanding after the re-nag interval.
    Reminder,
    /// Say nothing.
    Hold(HoldReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// Too quiet to interrupt; it belongs in the daily brief.
    NotInterrupting,
    /// Already told them, and nothing has changed.
    AlreadySent,
    /// Inside quiet hours.
    QuietHours,
}

impl Decision {
    pub fn should_send(self) -> bool {
        !matches!(self, Decision::Hold(_))
    }
}

/// How long an outstanding task stays quiet before it is raised again.
///
/// Long, deliberately. The rules re-emit every tick; without this the same root check
/// would ping every evaluation until it was done, which is precisely how a
/// notification system teaches someone to ignore it.
pub const REMINDER_INTERVAL_HOURS: f64 = 24.0;

/// Decide whether to notify about a task.
pub fn decide(
    severity: Severity,
    last: Option<LastNotified>,
    quiet: QuietHours,
    local_hour: u8,
    now: Timestamp,
) -> Decision {
    if !reach_for(severity).interrupts {
        return Decision::Hold(HoldReason::NotInterrupting);
    }
    if !quiet.permits(severity, local_hour) {
        return Decision::Hold(HoldReason::QuietHours);
    }

    let Some(last) = last else {
        return Decision::First;
    };

    if severity > last.severity {
        return Decision::Escalated;
    }

    let hours = gardyn_core::time::days_between(last.at, now) * 24.0;
    if hours >= REMINDER_INTERVAL_HOURS {
        Decision::Reminder
    } else {
        Decision::Hold(HoldReason::AlreadySent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn hours_later(h: f64) -> Timestamp {
        gardyn_core::time::add_days(t0(), h / 24.0)
    }

    const DAYTIME: u8 = 14;
    const NIGHT: u8 = 2;

    #[test]
    fn quiet_hours_wrap_across_midnight() {
        let quiet = QuietHours::default(); // 21:00-07:00
        assert!(quiet.contains(22));
        assert!(quiet.contains(3));
        assert!(quiet.contains(6));
        assert!(!quiet.contains(7));
        assert!(!quiet.contains(14));
        assert!(!quiet.contains(20));
    }

    #[test]
    fn a_daytime_only_window_does_not_wrap() {
        let quiet = QuietHours {
            from_hour: 9,
            to_hour: 17,
        };
        assert!(quiet.contains(12));
        assert!(!quiet.contains(20));
        assert!(!quiet.contains(3));
    }

    #[test]
    fn an_empty_window_is_never_quiet_rather_than_always() {
        // Someone setting both ends the same almost certainly means "no quiet hours",
        // and the opposite reading would silence the system permanently.
        let quiet = QuietHours {
            from_hour: 9,
            to_hour: 9,
        };
        for hour in 0..24 {
            assert!(!quiet.contains(hour));
        }
    }

    #[test]
    fn a_dry_tank_wakes_you_up_but_a_root_check_does_not() {
        let quiet = QuietHours::default();
        assert!(quiet.permits(Severity::Critical, NIGHT));
        assert!(!quiet.permits(Severity::Urgent, NIGHT));
        assert!(!quiet.permits(Severity::Important, NIGHT));
    }

    #[test]
    fn advisories_never_interrupt_at_any_hour() {
        // They belong in the morning brief. Pushing them is how the app gets muted.
        assert!(!reach_for(Severity::Advisory).interrupts);
        assert!(!reach_for(Severity::Info).push);
        assert_eq!(
            decide(Severity::Advisory, None, QuietHours::default(), DAYTIME, t0()),
            Decision::Hold(HoldReason::NotInterrupting)
        );
    }

    #[test]
    fn email_is_reserved_for_urgent_and_above() {
        assert!(!reach_for(Severity::Important).email);
        assert!(reach_for(Severity::Urgent).email);
        assert!(reach_for(Severity::Critical).email);
    }

    #[test]
    fn critical_uses_the_priority_that_bypasses_do_not_disturb() {
        assert_eq!(reach_for(Severity::Critical).priority, 5);
    }

    #[test]
    fn the_first_time_a_task_appears_it_is_sent() {
        assert_eq!(
            decide(Severity::Important, None, QuietHours::default(), DAYTIME, t0()),
            Decision::First
        );
    }

    #[test]
    fn the_same_task_is_not_sent_twice_in_a_row() {
        // The rules re-emit every tick; without this the same task pings forever.
        let last = LastNotified {
            at: t0(),
            severity: Severity::Important,
        };
        assert_eq!(
            decide(
                Severity::Important,
                Some(last),
                QuietHours::default(),
                DAYTIME,
                hours_later(1.0)
            ),
            Decision::Hold(HoldReason::AlreadySent)
        );
    }

    #[test]
    fn getting_worse_re_notifies_immediately() {
        // "Top the tank up sometime" becoming "the tank is nearly dry" must not wait
        // for the reminder interval.
        let last = LastNotified {
            at: t0(),
            severity: Severity::Important,
        };
        assert_eq!(
            decide(
                Severity::Critical,
                Some(last),
                QuietHours::default(),
                NIGHT,
                hours_later(0.5)
            ),
            Decision::Escalated
        );
    }

    #[test]
    fn getting_better_does_not_re_notify() {
        let last = LastNotified {
            at: t0(),
            severity: Severity::Critical,
        };
        assert_eq!(
            decide(
                Severity::Urgent,
                Some(last),
                QuietHours::default(),
                DAYTIME,
                hours_later(1.0)
            ),
            Decision::Hold(HoldReason::AlreadySent)
        );
    }

    #[test]
    fn a_task_left_undone_is_raised_again_the_next_day() {
        let last = LastNotified {
            at: t0(),
            severity: Severity::Important,
        };
        assert_eq!(
            decide(
                Severity::Important,
                Some(last),
                QuietHours::default(),
                DAYTIME,
                hours_later(REMINDER_INTERVAL_HOURS + 1.0)
            ),
            Decision::Reminder
        );
    }

    #[test]
    fn quiet_hours_hold_an_escalation_that_is_not_critical() {
        let last = LastNotified {
            at: t0(),
            severity: Severity::Important,
        };
        assert_eq!(
            decide(
                Severity::Urgent,
                Some(last),
                QuietHours::default(),
                NIGHT,
                hours_later(1.0)
            ),
            Decision::Hold(HoldReason::QuietHours)
        );
    }

    #[test]
    fn every_send_decision_reports_itself_as_sendable() {
        for decision in [Decision::First, Decision::Escalated, Decision::Reminder] {
            assert!(decision.should_send());
        }
        for reason in [
            HoldReason::AlreadySent,
            HoldReason::QuietHours,
            HoldReason::NotInterrupting,
        ] {
            assert!(!Decision::Hold(reason).should_send());
        }
    }
}
