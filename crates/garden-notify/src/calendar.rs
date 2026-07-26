//! An iCal feed of scheduled work.
//!
//! Subscribed once in Google or Apple Calendar, this puts "refresh the tank" next to
//! everything else you have on Saturday. It is the right home for the predictable
//! cadence work that a push notification would only nag about.
//!
//! Hand-rolled rather than pulled from a crate: RFC 5545 for a read-only VTODO feed is
//! a few dozen lines, and the line-folding and escaping rules are the only fiddly part.

use garden_core::{Severity, Timestamp};

/// One entry in the feed.
pub struct CalendarTask {
    /// Stable across regenerations, so an existing entry updates rather than
    /// duplicating every time the feed is fetched.
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub due: Timestamp,
    pub severity: Severity,
}

/// RFC 5545 wants CRLF, and readers are less forgiving about it than you would hope.
const CRLF: &str = "\r\n";

/// Escape the characters that would otherwise break a property value.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// Fold lines to 75 octets, as the spec requires.
///
/// Counted in bytes, not characters: a rationale containing "°C" or an em dash would
/// otherwise be split mid-codepoint and arrive as mojibake.
fn fold(line: &str) -> String {
    const LIMIT: usize = 73;
    if line.len() <= LIMIT {
        return line.to_string();
    }

    let mut out = String::with_capacity(line.len() + line.len() / LIMIT * 3);
    let mut used = 0;
    for ch in line.chars() {
        let width = ch.len_utf8();
        if used + width > LIMIT {
            out.push_str(CRLF);
            out.push(' ');
            used = 1;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// `20260726T140000Z`
fn stamp(at: Timestamp) -> String {
    at.to_string()
        .replace(['-', ':'], "")
        .split('.')
        .next()
        .map(|s| {
            if s.ends_with('Z') {
                s.to_string()
            } else {
                format!("{s}Z")
            }
        })
        .unwrap_or_default()
}

fn priority(severity: Severity) -> u8 {
    // RFC 5545 priority runs 1 (highest) to 9.
    match severity {
        Severity::Critical => 1,
        Severity::Urgent => 2,
        Severity::Important => 4,
        Severity::Advisory => 6,
        Severity::Info => 9,
    }
}

/// Render a feed.
pub fn render(garden_name: &str, tasks: &[CalendarTask], now: Timestamp) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//garden//garden tasks//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "METHOD:PUBLISH".to_string(),
        fold(&format!("X-WR-CALNAME:{}", escape(garden_name))),
        // Without this most clients poll far more often than a garden changes.
        "X-PUBLISHED-TTL:PT1H".to_string(),
        "REFRESH-INTERVAL;VALUE=DURATION:PT1H".to_string(),
    ];

    for task in tasks {
        lines.push("BEGIN:VTODO".into());
        lines.push(fold(&format!("UID:{}", escape(&task.uid))));
        lines.push(format!("DTSTAMP:{}", stamp(now)));
        lines.push(format!("DUE:{}", stamp(task.due)));
        lines.push(fold(&format!("SUMMARY:{}", escape(&task.summary))));
        lines.push(fold(&format!("DESCRIPTION:{}", escape(&task.description))));
        lines.push(format!("PRIORITY:{}", priority(task.severity)));
        lines.push("STATUS:NEEDS-ACTION".into());
        lines.push("END:VTODO".into());
    }

    lines.push("END:VCALENDAR".into());
    // Trailing CRLF: some parsers drop the final component without it.
    format!("{}{CRLF}", lines.join(CRLF))
}

pub const CONTENT_TYPE: &str = "text/calendar; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn task() -> CalendarTask {
        CalendarTask {
            uid: "tankrefresh-garden@kitchen".into(),
            summary: "Refresh tank".into(),
            description: "32 days since the last tank refresh".into(),
            due: t0(),
            severity: Severity::Advisory,
        }
    }

    #[test]
    fn a_feed_is_well_formed() {
        let ics = render("Kitchen", &[task()], t0());
        assert!(ics.starts_with("BEGIN:VCALENDAR"));
        assert!(ics.trim_end().ends_with("END:VCALENDAR"));
        assert_eq!(ics.matches("BEGIN:VTODO").count(), 1);
        assert_eq!(ics.matches("END:VTODO").count(), 1);
    }

    #[test]
    fn lines_are_crlf_terminated() {
        // Plenty of readers are strict about this and will reject a bare-LF feed.
        let ics = render("Kitchen", &[task()], t0());
        assert!(ics.contains("\r\n"));
        assert!(!ics.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn an_empty_garden_still_produces_a_valid_feed() {
        // A subscribed calendar that 500s when there is nothing to do is worse than
        // an empty one.
        let ics = render("Kitchen", &[], t0());
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(!ics.contains("BEGIN:VTODO"));
    }

    #[test]
    fn special_characters_are_escaped() {
        let mut t = task();
        t.description = "water; then feed, and note: 22% full".into();
        let ics = render("Kitchen", &[t], t0());
        assert!(ics.contains("\\;"));
        assert!(ics.contains("\\,"));
    }

    #[test]
    fn timestamps_are_utc_basic_format() {
        let ics = render("Kitchen", &[task()], t0());
        assert!(ics.contains("DUE:20231114T221320Z"), "{ics}");
    }

    #[test]
    fn severity_maps_onto_calendar_priority() {
        assert_eq!(priority(Severity::Critical), 1);
        assert!(priority(Severity::Advisory) > priority(Severity::Urgent));
    }

    #[test]
    fn long_lines_are_folded_without_splitting_a_character() {
        // The failure this guards: folding by byte index through a multi-byte glyph
        // produces mojibake in the calendar entry.
        let mut t = task();
        t.description = "reservoir at 26.5 °C — ".repeat(8);
        let ics = render("Kitchen", &[t], t0());

        for line in ics.split("\r\n") {
            assert!(line.len() <= 75, "unfolded line of {} bytes", line.len());
        }
        // Still decodes, and the glyphs survived.
        assert!(ics.contains("°C") || ics.contains("°"), "degrees sign lost");
    }

    #[test]
    fn a_folded_continuation_starts_with_a_space() {
        let mut t = task();
        t.summary = "x".repeat(200);
        let ics = render("Kitchen", &[t], t0());
        let folded: Vec<&str> = ics.split("\r\n").filter(|l| l.starts_with(' ')).collect();
        assert!(!folded.is_empty(), "nothing was folded");
    }

    #[test]
    fn the_uid_is_stable_so_entries_update_rather_than_duplicate() {
        let first = render("Kitchen", &[task()], t0());
        let later = render("Kitchen", &[task()], garden_core::time::add_days(t0(), 1.0));
        let uid = "UID:tankrefresh-garden@kitchen";
        assert!(first.contains(uid) && later.contains(uid));
    }
}
