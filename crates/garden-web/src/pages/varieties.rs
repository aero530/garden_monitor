//! The plant book: browse every yCube Gardyn sells, and read up on one.
//!
//! Reachable from the nav and from any planted slot, because the moment you want to
//! know how to look after something is the moment you are looking at it.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, Query, State};
use axum::{Router, routing::get};
use garden_core::{
    CanopyClass, Category, HarvestStyle, LightZone, Variety, VarietyBook, VarietyId,
};
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/varieties", get(index))
        .route("/varieties/{id}", get(detail))
}

#[derive(Deserialize, Default)]
pub struct Filter {
    /// Free-text match on the name.
    q: Option<String>,
    /// `high`, `medium`, `low`.
    zone: Option<String>,
    /// `herb`, `leafy_green`, `fruiting`, `flower`.
    category: Option<String>,
}

fn category_slug(category: Category) -> &'static str {
    match category {
        Category::Herb => "herb",
        Category::LeafyGreen => "leafy_green",
        Category::Fruiting => "fruiting",
        Category::Flower => "flower",
    }
}

fn matches(variety: &Variety, filter: &Filter) -> bool {
    if let Some(q) = filter.q.as_deref().map(str::trim).filter(|q| !q.is_empty())
        && !variety.name.to_lowercase().contains(&q.to_lowercase())
    {
        return false;
    }
    if let Some(zone) = filter.zone.as_deref().filter(|z| !z.is_empty())
        && variety.light_zone.slug() != zone
    {
        return false;
    }
    if let Some(category) = filter.category.as_deref().filter(|c| !c.is_empty())
        && category_slug(variety.category) != category
    {
        return false;
    }
    true
}

async fn index(Auth(actor): Auth, Query(filter): Query<Filter>) -> Markup {
    let book = VarietyBook::catalogue();
    let shown: Vec<&Variety> = book.iter().filter(|v| matches(v, &filter)).collect();
    let described = book.described_count();

    ui::page(
        "Plant book",
        Some(&actor),
        html! {
            h1 { "Plant book" }
            p.muted.small {
                (book.len()) " varieties from Gardyn's catalogue. "
                (described) " quote Gardyn's own write-up"
                @if described < book.len() {
                    " — the other " (book.len() - described)
                    " have no live article and show their growing figures only"
                }
                "."
            }

            form.card method="get" action="/varieties" {
                div.row {
                    div style="flex:2; min-width:12rem" {
                        label for="q" { "Search" }
                        input #q type="search" name="q" value=(filter.q.clone().unwrap_or_default())
                              placeholder="kale, pepper, basil…";
                    }
                    div style="flex:1; min-width:9rem" {
                        label for="zone" { "Light zone" }
                        select #zone name="zone" {
                            option value="" { "any" }
                            @for zone in LightZone::ALL {
                                option value=(zone.slug())
                                       selected[filter.zone.as_deref() == Some(zone.slug())] {
                                    (zone.label())
                                }
                            }
                        }
                    }
                    div style="flex:1; min-width:9rem" {
                        label for="category" { "Type" }
                        select #category name="category" {
                            option value="" { "any" }
                            @for (slug, name) in [
                                ("leafy_green", "greens"),
                                ("herb", "herbs"),
                                ("fruiting", "fruiting"),
                                ("flower", "flowers"),
                            ] {
                                option value=(slug) selected[filter.category.as_deref() == Some(slug)] {
                                    (name)
                                }
                            }
                        }
                    }
                }
                p style="margin-top:0.75rem" {
                    button.primary type="submit" { "Filter" }
                    " "
                    a.button href="/varieties" { "Clear" }
                }
            }

            p.muted.small { (shown.len()) " shown" }

            @if shown.is_empty() {
                div.card { p.muted style="margin:0" { "Nothing matches that." } }
            }

            div.grid {
                @for variety in &shown {
                    a.card href=(format!("/varieties/{}", variety.id))
                      style="text-decoration:none; color:inherit; display:block" {
                        h3 style="margin:0 0 0.2rem" { (variety.name) }
                        div.row style="gap:0.3rem" {
                            span.pill class=(format!("zone-pill-{}", variety.light_zone.slug())) {
                                (variety.light_zone.label())
                            }
                            span.pill.sev-info { (variety.category.label()) }
                        }
                        p.small.muted style="margin:0.45rem 0 0" {
                            (variety.days_to_first_harvest) " days to harvest · "
                            (variety.care_level.label())
                        }
                    }
                }
            }
        },
    )
}

async fn detail(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let book = VarietyBook::catalogue();
    let variety = book.get(&VarietyId::new(&id)).ok_or(AppError::NotFound)?;

    // Where this plant is growing right now, across every garden the caller can see.
    let mut growing = Vec::new();
    for listing in state.store.gardens_for_user(actor.id()).await? {
        for planting in state.store.active_plantings(listing.garden.id).await? {
            if planting.variety == variety.id {
                growing.push((listing.garden.clone(), planting.slot));
            }
        }
    }

    Ok(ui::page(
        &variety.name,
        Some(&actor),
        html! {
            p.muted.small { a href="/varieties" { "← Plant book" } }
            h1 style="margin-bottom:0.3rem" { (variety.name) }
            div.row style="gap:0.3rem; margin-bottom:1rem" {
                span.pill class=(format!("zone-pill-{}", variety.light_zone.slug())) {
                    (variety.light_zone.label())
                }
                span.pill.sev-info { (variety.category.label()) }
                span.pill.sev-info { (variety.care_level.label()) }
                @if variety.needs_pollination {
                    span.pill.sev-advisory { "needs pollinating" }
                }
            }

            @if variety.zone_disputed() {
                p.small.muted {
                    "Gardyn's placement guide groups this under "
                    (variety.guide_zone.map(|z| z.label()).unwrap_or("another zone"))
                    ", but the plant's own page says " (variety.light_zone.label())
                    ". The plant page is used here."
                }
            }

            (facts(variety))

            @if !variety.has_description() {
                div.card {
                    h3 { "No description yet" }
                    p.muted.small style="margin:0" {
                        "Gardyn's help centre has no live article for this variety, so "
                        "there is no write-up to show. The growing figures above come "
                        "from the placement guide."
                    }
                }
            }
            @if !variety.qualities.is_empty() {
                div.card {
                    h2 style="margin-top:0" { "Qualities" }
                    (prose(&variety.qualities))
                }
            }
            @if !variety.care.is_empty() {
                div.card {
                    h2 style="margin-top:0" { "Care & harvest" }
                    (prose(&variety.care))
                }
            }

            @if let Some(note) = &variety.placement_note {
                div.card {
                    h3 { "Placement" }
                    p.small style="margin:0" { (note) }
                }
            }

            @if !growing.is_empty() {
                h2 { "You are growing this" }
                div.card {
                    @for (garden, slot) in &growing {
                        p.small style="margin:0 0 0.3rem" {
                            a href=(format!("/gardens/{}/slots", garden.id)) { (garden.name) }
                            " · " (slot.to_string())
                        }
                    }
                }
            }

            @if variety.estimated {
                p.small.muted {
                    "Gardyn's article for this variety carries no data block, so the "
                    "figures above are category defaults rather than published numbers."
                }
            }

            @if let Some(url) = &variety.article_url {
                p.small.muted {
                    "Text quoted from " a href=(url) rel="noreferrer" { "Gardyn's article" }
                    " for this variety."
                }
            }
        },
    ))
}

/// Render Gardyn's paragraphs, splitting their labelled entries into a term/detail
/// pair so "Temperature", "Pruning" and "Harvest" can be found at a glance.
fn prose(paragraphs: &[String]) -> Markup {
    html! {
        @for paragraph in paragraphs {
            @match split_label(paragraph) {
                Some((label, rest)) => p style="margin:0 0 0.6rem" {
                    strong { (label) } ": " (rest)
                },
                None => p style="margin:0 0 0.6rem" { (paragraph) },
            }
        }
    }
}

/// Split "💡 Temperature: prefers 70-85°F" into its label and the rest.
///
/// Only a short leading label counts — a colon deep inside a sentence is punctuation,
/// not a heading, and bolding half a paragraph looks like a bug.
fn split_label(paragraph: &str) -> Option<(&str, &str)> {
    let (head, rest) = paragraph.split_once(": ")?;
    let label = head.trim();
    (!label.is_empty() && label.chars().count() <= 28 && !rest.trim().is_empty())
        .then_some((label, rest.trim()))
}

/// The published growing figures, as a plain table.
fn facts(variety: &Variety) -> Markup {
    let harvest = match variety.harvest_style {
        HarvestStyle::Single => "one harvest, then replace".to_string(),
        HarvestStyle::CutAndComeAgain { interval_days } => {
            format!("cut and come again, about every {interval_days} days")
        }
        HarvestStyle::ContinuousFruiting { interval_days } => {
            format!("fruits in waves, about every {interval_days} days")
        }
    };
    let canopy = match variety.canopy {
        CanopyClass::Compact => "compact",
        CanopyClass::Medium => "medium",
        CanopyClass::Large => "large",
        CanopyClass::Vining => "vining",
    };

    html! {
        div.table-wrap {
            table {
                tbody {
                    tr { th { "Days to sprout" } td { (variety.germination_days) } }
                    tr {
                        th { "Days to harvest" }
                        td { (variety.days_to_first_harvest) " after sprouting" }
                    }
                    tr { th { "Harvest style" } td { (harvest) } }
                    tr { th { "Thin to" } td { (variety.thin_to) " per yCube" } }
                    tr { th { "Plant size" } td { (variety.plant_size) " · " (canopy) } }
                    tr {
                        th { "Pruning" }
                        td { @if variety.needs_pruning { "required" } @else { "minimal" } }
                    }
                    tr {
                        th { "Pollination" }
                        td {
                            @if variety.needs_pollination {
                                "by hand — shake the plant or brush the blossoms"
                            } @else { "not needed" }
                        }
                    }
                    tr {
                        th { "Productive life" }
                        td {
                            (variety.productive_life_days) " days "
                            span.muted.small { "(estimated — Gardyn does not publish this)" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> VarietyBook {
        VarietyBook::catalogue()
    }

    fn find(book: &VarietyBook, id: &str) -> Variety {
        book.get(&VarietyId::new(id)).unwrap().clone()
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        let book = book();
        let filter = Filter::default();
        assert_eq!(book.iter().filter(|v| matches(v, &filter)).count(), book.len());
    }

    #[test]
    fn search_is_case_insensitive_and_partial() {
        let book = book();
        let filter = Filter {
            q: Some("KALE".into()),
            ..Default::default()
        };
        let hits: Vec<_> = book.iter().filter(|v| matches(v, &filter)).collect();
        assert!(hits.len() >= 2, "expected Kale and Lacinato Kale");
        assert!(hits.iter().all(|v| v.name.to_lowercase().contains("kale")));
    }

    #[test]
    fn blank_filter_values_are_ignored_rather_than_matching_nothing() {
        // An empty select submits "", which must not filter everything out.
        let book = book();
        let filter = Filter {
            q: Some("  ".into()),
            zone: Some(String::new()),
            category: Some(String::new()),
        };
        assert_eq!(book.iter().filter(|v| matches(v, &filter)).count(), book.len());
    }

    #[test]
    fn zone_and_category_filters_combine() {
        let book = book();
        let filter = Filter {
            q: None,
            zone: Some("high".into()),
            category: Some("fruiting".into()),
        };
        let hits: Vec<_> = book.iter().filter(|v| matches(v, &filter)).collect();
        assert!(!hits.is_empty());
        for v in hits {
            assert_eq!(v.light_zone, LightZone::High);
            assert_eq!(v.category, Category::Fruiting);
        }
    }

    #[test]
    fn a_transcribed_variety_carries_its_prose() {
        let basil = find(&book(), "basil");
        assert!(basil.has_description());
        assert!(basil.qualities.concat().contains("vitamin K"));
        assert!(basil.care.concat().contains("bolting"));
        assert!(basil.article_url.is_some());
    }

    #[test]
    fn an_untranscribed_variety_is_still_usable() {
        // Four varieties have no live article on Gardyn's side; those pages must
        // still render from the figures alone.
        let book = book();
        let bare = book.iter().find(|v| !v.has_description());
        if let Some(v) = bare {
            assert!(v.days_to_first_harvest > 0);
            assert!(!v.name.is_empty());
            assert!(v.article_url.is_none());
        }
    }

    #[test]
    fn a_leading_label_is_split_out_but_mid_sentence_punctuation_is_not() {
        assert_eq!(
            split_label("💡 Temperature: Prefers warmer temperatures (70-85°F)."),
            Some(("💡 Temperature", "Prefers warmer temperatures (70-85°F).")),
        );
        // Long enough to be a sentence, so bolding it would look like a bug.
        assert_eq!(
            split_label("Pollination is an art, and this is the reason: uneven work."),
            None,
        );
        assert_eq!(split_label("No colon here at all"), None);
        assert_eq!(split_label("Trailing colon: "), None);
    }

    #[test]
    fn gardens_own_care_labels_all_survive_the_split() {
        // If the extractor or the split ever drifts, the care sections silently turn
        // into undifferentiated prose. Catch that here rather than in the browser.
        let book = book();
        let labelled = book
            .iter()
            .flat_map(|v| v.care.iter())
            .filter(|p| split_label(p).is_some())
            .count();
        let total: usize = book.iter().map(|v| v.care.len()).sum();
        assert!(total > 400, "expected the transcribed care text, got {total}");
        assert!(
            labelled * 2 > total,
            "only {labelled} of {total} care paragraphs kept their label",
        );
    }

    #[test]
    fn category_slugs_round_trip_through_the_filter() {
        for category in [
            Category::Herb,
            Category::LeafyGreen,
            Category::Fruiting,
            Category::Flower,
        ] {
            let filter = Filter {
                category: Some(category_slug(category).into()),
                ..Default::default()
            };
            let hits = book().iter().filter(|v| matches(v, &filter)).count();
            assert!(hits > 0, "{category:?} matched nothing");
        }
    }
}
