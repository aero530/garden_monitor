//! Gardyn's own maintenance procedures, carried in the binary.
//!
//! Two of the tasks this system raises are not "add 30 mL of plant food" — they are
//! twenty-minute physical jobs involving a full tank of water, a mains-powered device
//! and a bowl of citric acid. A reminder that says *deep clean* and nothing else is not
//! much of a reminder, so the procedure travels with the task.
//!
//! Transcribed **verbatim** from Gardyn's help centre rather than summarised, for the
//! same reason the variety prose is: these steps concern live plants and household
//! chemicals, and a paraphrase is how "clean with baking soda, no soap needed" turns
//! into something that kills a root system. Each guide keeps the URL it came from so a
//! reader can check the original, which may since have been updated.
//!
//! Some sections describe *Gardyn's* app — its water widget, its reminders, logging a
//! refresh there. Those are kept, because they carry facts worth having (the 28-day
//! reset among them), but they are flagged with [`GuideSection::about_vendor_app`] so
//! this system never presents another product's notification system as instructions for
//! using this one.

use crate::task::TaskKind;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Embedded, so the binary needs no data files alongside it.
const GUIDES: &str = include_str!("../data/maintenance-guides.json");

/// The slug of the tank-refresh guide.
pub const TANK_REFRESH: &str = "tank-refresh";
/// The slug of the deep-clean guide.
pub const DEEP_CLEAN: &str = "deep-clean";

/// One transcribed procedure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guide {
    /// Stable identifier, used in URLs.
    pub slug: String,
    pub title: String,
    /// The article this was taken from.
    pub source: String,
    /// Anything before the first heading.
    pub lede: Vec<String>,
    pub sections: Vec<GuideSection>,
}

impl Guide {
    /// The steps, in order, skipping the sections that are about Gardyn's own app.
    ///
    /// What somebody standing at the garden with wet hands actually needs.
    pub fn procedure(&self) -> impl Iterator<Item = &GuideSection> {
        self.sections.iter().filter(|s| !s.about_vendor_app)
    }
}

/// One heading and the prose under it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GuideSection {
    /// Gardyn's own anchor for the heading. Reused as our fragment id, so a link into
    /// the middle of a guide survives a re-transcription.
    pub anchor: String,
    pub title: String,
    pub body: Vec<String>,
    /// Whether the source marked this up as a list.
    ///
    /// Carried through because it changes how it should be read: a numbered procedure
    /// rendered as a wall of prose is much harder to follow one-handed.
    #[serde(default)]
    pub list: bool,
    /// Whether this section is about Gardyn's app rather than the physical task.
    #[serde(default)]
    pub about_vendor_app: bool,
}

/// Every guide, keyed by slug.
#[derive(Debug, Clone, Default)]
pub struct GuideBook(BTreeMap<String, Guide>);

impl GuideBook {
    /// Gardyn's published maintenance guides.
    ///
    /// Panics on malformed JSON, which is an authoring error in an embedded file rather
    /// than anything a running system can encounter.
    pub fn published() -> Self {
        let file: GuideFile =
            serde_json::from_str(GUIDES).expect("embedded maintenance guides are valid JSON");
        Self(
            file.guides
                .into_iter()
                .map(|(slug, g)| {
                    let guide = Guide {
                        slug: slug.clone(),
                        title: g.title,
                        source: g.source,
                        lede: g.lede,
                        sections: g.sections,
                    };
                    (slug, guide)
                })
                .collect(),
        )
    }

    pub fn get(&self, slug: &str) -> Option<&Guide> {
        self.0.get(slug)
    }

    /// The guide for a task, if the task has one.
    pub fn for_task(&self, kind: TaskKind) -> Option<&Guide> {
        kind.guide_slug().and_then(|slug| self.get(slug))
    }

    /// Guides in a stable order: refresh before clean, matching how often each is done.
    pub fn all(&self) -> impl Iterator<Item = &Guide> {
        // Frequency order rather than alphabetical. A monthly job belongs above an
        // occasional one, and alphabetical would put deep clean first.
        [TANK_REFRESH, DEEP_CLEAN]
            .into_iter()
            .filter_map(|slug| self.get(slug))
            .chain(
                self.0
                    .values()
                    .filter(|g| g.slug != TANK_REFRESH && g.slug != DEEP_CLEAN),
            )
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Deserialize)]
struct GuideFile {
    guides: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
struct Entry {
    title: String,
    source: String,
    #[serde(default)]
    lede: Vec<String>,
    #[serde(default)]
    sections: Vec<GuideSection>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_published_guides_are_embedded() {
        let book = GuideBook::published();
        assert_eq!(book.len(), 2);
        assert!(book.get(TANK_REFRESH).is_some());
        assert!(book.get(DEEP_CLEAN).is_some());
    }

    #[test]
    fn every_guide_cites_the_article_it_came_from() {
        // The prose is somebody else's. Showing it without the link is both discourteous
        // and unhelpful, since Gardyn revises these articles.
        for guide in GuideBook::published().all() {
            assert!(
                guide.source.starts_with("https://help.mygardyn.com/"),
                "{} cites {}",
                guide.slug,
                guide.source
            );
            assert!(!guide.title.is_empty());
            assert!(!guide.sections.is_empty(), "{} has no sections", guide.slug);
        }
    }

    #[test]
    fn the_two_maintenance_tasks_reach_their_guides() {
        // The point of the whole module: a reminder can link to the procedure.
        let book = GuideBook::published();
        assert_eq!(
            book.for_task(TaskKind::TankRefresh)
                .map(|g| g.slug.as_str()),
            Some(TANK_REFRESH)
        );
        assert_eq!(
            book.for_task(TaskKind::DeepClean).map(|g| g.slug.as_str()),
            Some(DEEP_CLEAN)
        );
        assert!(book.for_task(TaskKind::AddWater).is_none());
    }

    #[test]
    fn the_refresh_guide_states_the_four_week_cadence_the_rules_use() {
        // The rule's constant and this prose have to agree, or the reminder contradicts
        // the instructions printed directly beneath it.
        let book = GuideBook::published();
        let guide = book.get(TANK_REFRESH).unwrap();
        let text = guide
            .sections
            .iter()
            .flat_map(|s| s.body.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("every 4 weeks"),
            "cadence missing from the transcription"
        );
        assert!(
            text.contains("28 days"),
            "reset interval missing from the transcription"
        );
    }

    #[test]
    fn the_clean_guide_lists_the_conditions_that_call_for_one() {
        // Gardyn publishes no fixed cleaning cadence — it is condition-driven, and
        // `DeepCleanByFoulingRule` leans on exactly these conditions.
        let text = GuideBook::published()
            .get(DEEP_CLEAN)
            .unwrap()
            .sections
            .iter()
            .flat_map(|s| s.body.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        for reason in ["algae", "root pieces", "salt deposits", "pests"] {
            assert!(
                text.contains(reason),
                "{reason} missing from the transcription"
            );
        }
    }

    #[test]
    fn the_procedure_leaves_out_the_sections_about_gardyns_own_app() {
        // Our reminder is the reminder. Telling a reader to tap Gardyn's water widget,
        // in a page reached from our own task list, is just confusing.
        let book = GuideBook::published();
        let guide = book.get(TANK_REFRESH).unwrap();
        assert!(
            guide.procedure().count() < guide.sections.len(),
            "no vendor-app sections were flagged, so the flag is not being applied"
        );
        assert!(guide.procedure().all(|s| !s.about_vendor_app));
    }

    #[test]
    fn numbered_steps_survive_as_lists() {
        // If every section came through as prose the `list` flag would be dead weight
        // and the page would render a procedure as an essay.
        let book = GuideBook::published();
        for slug in [TANK_REFRESH, DEEP_CLEAN] {
            let guide = book.get(slug).unwrap();
            assert!(
                guide.sections.iter().any(|s| s.list),
                "{slug} transcribed with no lists at all"
            );
        }
    }

    #[test]
    fn refresh_is_listed_before_clean() {
        let book = GuideBook::published();
        let order: Vec<&str> = book.all().map(|g| g.slug.as_str()).collect();
        assert_eq!(order, vec![TANK_REFRESH, DEEP_CLEAN]);
    }
}
