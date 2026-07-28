//! How to actually do the two maintenance jobs.
//!
//! A reminder that says "deep clean" and nothing else is not much of a reminder. These
//! pages carry Gardyn's own procedure, quoted, so the step you need is on the phone
//! already in your hand rather than behind a search of their help centre with wet gloves
//! on.
//!
//! Not scoped to a garden, and readable by any signed-in user: the procedure is the same
//! for every Studio 2, and needing `ConfigureGarden` to read instructions would be
//! absurd. Each maintenance task card links straight here.

use crate::app::Auth;
use crate::error::AppError;
use crate::ui;
use axum::extract::Path;
use axum::{Router, routing::get};
use garden_core::GuideBook;
use garden_core::guide::{Guide, GuideSection};
use maud::{Markup, html};

pub fn routes() -> Router<crate::app::AppState> {
    Router::new()
        .route("/guides", get(index))
        .route("/guides/{slug}", get(detail))
}

async fn index(Auth(actor): Auth) -> Markup {
    let book = GuideBook::published();

    ui::page(
        "Maintenance",
        Some(&actor),
        html! {
            h1 { "Maintenance" }
            p.muted.small {
                "The two jobs that are physical work rather than a measured dose. Both "
                "procedures are quoted from Gardyn's help centre; each is linked from its "
                "task when the reminder comes round."
            }

            @for guide in book.all() {
                div.card {
                    h2 style="margin-top:0; margin-bottom:0.3rem" {
                        a href=(format!("/guides/{}", guide.slug)) { (guide.title) }
                    }
                    div.row style="gap:0.3rem; margin-bottom:0.6rem" {
                        span.pill.sev-info { (cadence(&guide.slug)) }
                        span.pill.sev-info { (guide.procedure().count()) " steps" }
                    }
                    p.small style="margin:0 0 0.6rem" { (summary(&guide.slug)) }
                    p style="margin:0" {
                        a.button href=(format!("/guides/{}", guide.slug)) { "Read the steps" }
                    }
                }
            }

            div.card {
                h3 style="margin-top:0" { "How these are scheduled" }
                p.small style="margin:0 0 0.5rem" {
                    "A refresh runs on the calendar: Gardyn's interval is at least every "
                    "four weeks, so the reminder appears a week ahead and becomes a push "
                    "notification on the due date. Widespread yellowing across several "
                    "plants pulls it forward, which is Gardyn's own \"sooner if you "
                    "notice\" guidance, acted on from measurements rather than memory."
                }
                p.small style="margin:0" {
                    "A deep clean has no fixed interval, because Gardyn does not publish "
                    "one — it is driven by conditions. Rising pump current means the lines "
                    "are restricted, which is the measurable version of \"root pieces and "
                    "salt deposits\". A yearly nudge is the backstop for a garden with no "
                    "pump sensor fitted."
                }
            }
        },
    )
}

async fn detail(Auth(actor): Auth, Path(slug): Path<String>) -> Result<Markup, AppError> {
    let book = GuideBook::published();
    let guide = book.get(&slug).ok_or(AppError::NotFound)?;
    // Sections about Gardyn's own app are worth keeping but do not belong in the middle
    // of a procedure being followed from this system's reminder, so they go last.
    let aside: Vec<&GuideSection> = guide
        .sections
        .iter()
        .filter(|s| s.about_vendor_app)
        .collect();

    Ok(ui::page(
        &guide.title,
        Some(&actor),
        html! {
            p.muted.small { a href="/guides" { "← Maintenance" } }
            h1 style="margin-bottom:0.3rem" { (guide.title) }
            div.row style="gap:0.3rem; margin-bottom:1rem" {
                span.pill.sev-info { (cadence(&guide.slug)) }
            }

            @if !guide.lede.is_empty() {
                @for line in &guide.lede {
                    p.small { (line) }
                }
            }

            (contents(guide))

            @for section in guide.procedure() {
                (section_card(section))
            }

            @if !aside.is_empty() {
                h2 { "In Gardyn's own app" }
                p.muted.small {
                    "Kept for reference. The reminders in this system are independent of "
                    "Gardyn's app, so there is nothing to log there — marking the task "
                    "done here is what resets the clock."
                }
                @for section in aside {
                    (section_card(section))
                }
            }

            p.small.muted {
                "Quoted from " a href=(&guide.source) rel="noreferrer" { "Gardyn's article" }
                ", which may have been revised since this was transcribed."
            }
        },
    ))
}

/// A jump list, because a procedure read on a phone mid-job is scrolled, not read.
fn contents(guide: &Guide) -> Markup {
    html! {
        div.card {
            h3 style="margin-top:0" { "Steps" }
            @for section in guide.procedure() {
                p.small style="margin:0 0 0.25rem" {
                    a href=(format!("#{}", section.anchor)) { (section.title) }
                }
            }
        }
    }
}

fn section_card(section: &GuideSection) -> Markup {
    html! {
        div.card id=(section.anchor) {
            h2 style="margin-top:0" { (section.title) }
            // Gardyn's own markup decides this. A numbered procedure rendered as a
            // paragraph is much harder to hold your place in one-handed.
            @if section.list {
                ul style="margin:0; padding-left:1.2rem" {
                    @for line in &section.body {
                        li.small style="margin-bottom:0.3rem" { (line) }
                    }
                }
            } @else {
                @for line in &section.body {
                    p.small style="margin:0 0 0.5rem" { (line) }
                }
            }
        }
    }
}

/// How often the job comes round, in our own words.
///
/// Ours rather than Gardyn's, because this describes what *this* system will actually
/// remind you about — see `garden_rules::maintenance`.
fn cadence(slug: &str) -> &'static str {
    match slug {
        garden_core::guide::TANK_REFRESH => "every 4 weeks",
        garden_core::guide::DEEP_CLEAN => "when conditions call for it",
        _ => "occasional",
    }
}

fn summary(slug: &str) -> &'static str {
    match slug {
        garden_core::guide::TANK_REFRESH => {
            "Drain the tank, wipe it out, and refill a gallon at a time with fresh water \
             and plant food. Resets the nutrient baseline that weekly top-offs slowly \
             drift away from."
        }
        garden_core::guide::DEEP_CLEAN => {
            "Strip the columns, soak the yPods in citric acid, and run the solution \
             through the lines. For algae, root pieces, salt deposits or pests — or \
             before a break from growing."
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> garden_auth::Actor {
        garden_auth::Actor::new(
            garden_auth::User {
                id: garden_auth::UserId::new(),
                email: garden_auth::EmailAddress::parse("gardener@example.com")
                    .expect("a valid address"),
                display_name: "Gardener".into(),
                is_admin: false,
                created_at: jiff::Timestamp::from_second(1_700_000_000).unwrap(),
                disabled_at: None,
            },
            [],
        )
    }

    fn render(page: Markup) -> String {
        page.into_string()
    }

    #[tokio::test]
    async fn the_refresh_page_carries_the_steps_and_the_numbers() {
        // The whole point of the feature: the amounts and the actual procedure are on
        // the page, not a paraphrase of them.
        let html = render(
            detail(Auth(actor()), Path(garden_core::guide::TANK_REFRESH.into()))
                .await
                .expect("the refresh guide renders"),
        );

        assert!(
            html.contains("Drain and Clean Your Tank"),
            "missing a step heading"
        );
        assert!(html.contains("baking soda"), "missing the cleaning agent");
        assert!(
            html.contains("No soap needed"),
            "missing the warning that matters"
        );
        assert!(html.contains("1 tsp per gallon"), "missing the mature dose");
        assert!(
            html.contains("1/2 tsp per gallon"),
            "missing the seedling dose"
        );
        assert!(html.contains("4 gallons"), "missing the Studio tank size");
        assert!(
            html.contains("help.mygardyn.com/en/articles/1788097"),
            "the source article is not cited"
        );
    }

    #[tokio::test]
    async fn the_clean_page_carries_the_materials_and_the_safety_warning() {
        let html = render(
            detail(Auth(actor()), Path(garden_core::guide::DEEP_CLEAN.into()))
                .await
                .expect("the clean guide renders"),
        );

        assert!(html.contains("citric acid"), "missing the cleaning agent");
        assert!(
            html.contains("wear gloves"),
            "the safety warning was dropped"
        );
        assert!(
            html.contains("Empty your Gardyn Columns"),
            "missing a step heading"
        );
        assert!(html.contains("help.mygardyn.com/en/articles/6166337"));
    }

    #[tokio::test]
    async fn an_unknown_guide_is_a_404_rather_than_a_blank_page() {
        let error = detail(Auth(actor()), Path("compost-the-cat".into())).await;
        assert!(matches!(error, Err(AppError::NotFound)));
    }

    #[tokio::test]
    async fn the_index_links_both_guides() {
        let html = render(index(Auth(actor())).await);
        assert!(html.contains("/guides/tank-refresh"));
        assert!(html.contains("/guides/deep-clean"));
        assert!(html.contains("every 4 weeks"), "the cadence is not stated");
    }

    #[test]
    fn every_published_guide_has_a_cadence_and_a_summary() {
        // The index renders both for each guide. A third guide added to the JSON without
        // touching this file would show a blank card.
        for guide in GuideBook::published().all() {
            assert_ne!(cadence(&guide.slug), "occasional", "{}", guide.slug);
            assert!(!summary(&guide.slug).is_empty(), "{}", guide.slug);
        }
    }

    #[test]
    fn the_jump_list_and_the_body_hold_the_same_steps() {
        // Every anchor in the contents must exist as an id further down, or a tap on a
        // step goes nowhere.
        let book = GuideBook::published();
        for guide in book.all() {
            let listed: Vec<&str> = guide.procedure().map(|s| s.anchor.as_str()).collect();
            assert!(!listed.is_empty(), "{} has no procedure", guide.slug);
            for anchor in &listed {
                assert!(
                    guide.sections.iter().any(|s| s.anchor == *anchor),
                    "{anchor} is linked but not rendered"
                );
            }
        }
    }

    #[test]
    fn nothing_about_gardyns_app_is_mixed_into_the_procedure() {
        let book = GuideBook::published();
        let guide = book.get(garden_core::guide::TANK_REFRESH).unwrap();
        assert!(guide.procedure().all(|s| !s.about_vendor_app));
        // ...but it is not thrown away either.
        assert!(guide.sections.iter().any(|s| s.about_vendor_app));
    }
}
