//! Turning a task into something worth reading on a lock screen.

use garden_core::{Severity, TaskKind};

/// A single button on a push notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationAction {
    pub label: String,
    /// A one-shot signed link. See `garden_auth::action`.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// ntfy priority, 1-5.
    pub priority: u8,
    pub tags: Vec<String>,
    pub actions: Vec<NotificationAction>,
    /// Where tapping the notification itself goes.
    pub open_url: Option<String>,
}

/// An emoji ntfy renders beside the title.
///
/// Worth the effort: on a lock screen the icon is read before the words, and it is
/// the difference between glancing at a phone and unlocking it.
pub fn tag_for(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::AddWater => "droplet",
        TaskKind::AddPlantFood => "green_salad",
        TaskKind::AddConditioner => "test_tube",
        TaskKind::PruneRoots => "scissors",
        TaskKind::PrunePlant => "scissors",
        TaskKind::Harvest => "basket",
        TaskKind::Thin => "seedling",
        TaskKind::Pollinate => "honeybee",
        TaskKind::TankRefresh => "bathtub",
        TaskKind::DeepClean => "sponge",
        TaskKind::Replant => "recycle",
        TaskKind::Inspect => "eyes",
    }
}

/// What a task looks like as a notification.
///
/// The title says what and where; the body is the rule's own rationale, verbatim.
/// That last part matters — a notification that says "add water" invites "why?", and
/// a notification that says "tank at 22%, using 0.5 L/day, reserve in 1.8 days" does
/// not.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    kind: TaskKind,
    target: &str,
    garden_name: &str,
    rationale: &str,
    detail: Option<&str>,
    severity: Severity,
    priority: u8,
    open_url: Option<String>,
    actions: Vec<NotificationAction>,
) -> Notification {
    let quantity = detail.map(|d| format!(" ({d})")).unwrap_or_default();
    let title = format!("{kind}{quantity} — {garden_name}");

    let mut body = rationale.to_string();
    // Whole-garden tasks read badly with "— garden" tacked on; per-plant ones need it.
    if target != "garden" {
        body.push_str(&format!("\n{target}"));
    }
    if severity >= Severity::Urgent {
        body.push_str("\n⚠ overdue soon");
    }

    Notification {
        title,
        body,
        priority,
        tags: vec![tag_for(kind).to_string()],
        actions,
        open_url,
    }
}

/// The morning brief: everything outstanding, in one message.
///
/// Exists so advisories have somewhere to go other than a push. Without it the only
/// choices are "interrupt for a root check" or "never mention it", and both are wrong.
pub fn compose_brief(garden_name: &str, lines: &[String], open_url: Option<String>) -> Notification {
    let title = if lines.len() == 1 {
        format!("1 thing to do — {garden_name}")
    } else {
        format!("{} things to do — {garden_name}", lines.len())
    };

    Notification {
        title,
        body: lines.join("\n"),
        // Deliberately quiet. The brief is something you read when you pick the phone
        // up, not something that makes it buzz.
        priority: 2,
        tags: vec!["seedling".into()],
        actions: Vec::new(),
        open_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn water() -> Notification {
        compose(
            TaskKind::AddWater,
            "garden",
            "Kitchen",
            "tank at 22% (3.4 L), using 0.50 L/day — reserve reached in 1.8 days",
            Some("4.2 L"),
            Severity::Urgent,
            4,
            Some("https://brain/gardens/1".into()),
            vec![NotificationAction {
                label: "Done".into(),
                url: "https://brain/a/abc".into(),
            }],
        )
    }

    #[test]
    fn the_title_says_what_how_much_and_where() {
        let note = water();
        assert!(note.title.contains("add water"));
        assert!(note.title.contains("4.2 L"));
        assert!(note.title.contains("Kitchen"));
    }

    #[test]
    fn the_body_is_the_rules_own_reasoning() {
        // The whole point: a notification you can act on without opening the app.
        let note = water();
        assert!(note.body.contains("22%"));
        assert!(note.body.contains("0.50 L/day"));
        assert!(note.body.contains("1.8 days"));
    }

    #[test]
    fn a_whole_garden_task_does_not_say_dash_garden() {
        assert!(!water().body.contains("\ngarden"));
    }

    #[test]
    fn a_per_plant_task_says_which_plant() {
        let note = compose(
            TaskKind::Harvest,
            "planting 3",
            "Kitchen",
            "Lacinato Kale is due for harvest",
            None,
            Severity::Important,
            3,
            None,
            Vec::new(),
        );
        assert!(note.body.contains("planting 3"));
    }

    #[test]
    fn urgent_and_above_carry_a_visible_warning() {
        assert!(water().body.contains('⚠'));
        let mild = compose(
            TaskKind::PruneRoots,
            "planting 1",
            "Kitchen",
            "22 days since the last root check",
            None,
            Severity::Important,
            3,
            None,
            Vec::new(),
        );
        assert!(!mild.body.contains('⚠'));
    }

    #[test]
    fn every_task_kind_has_an_icon() {
        for kind in [
            TaskKind::AddWater,
            TaskKind::AddPlantFood,
            TaskKind::AddConditioner,
            TaskKind::PruneRoots,
            TaskKind::PrunePlant,
            TaskKind::Harvest,
            TaskKind::Thin,
            TaskKind::Pollinate,
            TaskKind::TankRefresh,
            TaskKind::DeepClean,
            TaskKind::Replant,
            TaskKind::Inspect,
        ] {
            assert!(!tag_for(kind).is_empty(), "{kind} has no icon");
        }
    }

    #[test]
    fn the_brief_counts_correctly_and_stays_quiet() {
        let one = compose_brief("Kitchen", &["add water".into()], None);
        assert!(one.title.starts_with("1 thing"));

        let several = compose_brief(
            "Kitchen",
            &["add water".into(), "prune roots".into(), "harvest".into()],
            None,
        );
        assert!(several.title.starts_with("3 things"));
        // A brief must never buzz; that is what makes advisories tolerable.
        assert!(several.priority <= 2);
        assert!(several.actions.is_empty());
    }
}
