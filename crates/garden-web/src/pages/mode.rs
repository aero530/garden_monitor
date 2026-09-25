//! Choosing how much of a garden to keep track of.
//!
//! The setting is one enum and the page is mostly prose, which is the right ratio: the
//! choice is cheap to make, instantly reversible, and easy to get wrong in a way that
//! is silent. Someone who picks simple mode expecting harvest reminders will not get
//! an error — they will get nothing, for weeks — so the page says plainly what each
//! mode will and will not tell them before they choose.
//!
//! Switching never edits a planting. See [`garden_store::Store::set_garden_mode`].

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Form, Path, Query, State};
use axum::response::Redirect;
use axum::{Router, routing::get, routing::post};
use garden_auth::Permission;
use garden_core::{Garden, GardenId, GardenMode};
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/mode", get(page))
        .route("/gardens/{id}/mode", post(update))
        .route("/gardens/{id}/mode/display", post(issue_display))
        .route("/gardens/{id}/mode/display/revoke", post(revoke_display))
}

#[derive(Deserialize)]
pub struct ModeForm {
    mode: String,
}

async fn load(state: &AppState, actor: &garden_auth::Actor, id: &str) -> Result<Garden, AppError> {
    let garden: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    actor.require(garden, Permission::ConfigureGarden)?;
    state.store.find_garden(garden).await?.ok_or(AppError::NotFound)
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    /// The display URL, shown once immediately after minting it.
    feed: Option<String>,
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Markup, AppError> {
    let garden = load(&state, &actor, &id).await?;
    let planted = state.store.active_plantings(garden.id).await?.len();
    let has_display = state.store.has_display_token(garden.id).await?;

    Ok(ui::page(
        "Mode",
        Some(&actor),
        html! {
            p.muted.small { a href=(format!("/gardens/{}", garden.id)) { "← " (garden.name) } }
            h1 { "How much to track" }
            p.small.muted {
                "Both modes watch the water, the nutrients and the tank. The difference "
                "is whether this garden keeps a record of what is in each slot."
            }

            form method="post" action=(format!("/gardens/{}/mode", garden.id)) {
                (choice(
                    GardenMode::Simple,
                    garden.mode,
                    "The garden as one thing",
                    &[
                        "Top up the water, with plant food and conditioner",
                        "Check the roots every few weeks",
                        "Refresh the tank, and deep clean it",
                    ],
                    "Nothing to keep up to date. No record of what is planted where, and \
                     so no harvest windows, thinning, germination checks or replanting \
                     suggestions — those need to know what is in each slot.",
                ))
                (choice(
                    GardenMode::Advanced,
                    garden.mode,
                    "Every plant, individually",
                    &[
                        "Everything simple mode does",
                        "Harvest timing per plant, from its variety and its age",
                        "Thinning, germination checks, pruning and pollination",
                        "Succession planning for what to put in a freed slot",
                    ],
                    "Worth it only if you keep the slot list current. The advice is \
                     derived from planting dates, so a record that has drifted produces \
                     advice that has drifted with it.",
                ))

                p { button.primary type="submit" { "Save" } }
            }

            (display_card(&garden, has_display, query.feed.as_deref()))

            // The single most likely worry, answered where it is felt rather than in a
            // doc nobody opens.
            div.card {
                h3 style="margin-top:0" { "Switching is reversible" }
                p.small.muted style="margin:0" {
                    @if planted == 0 {
                        "Nothing is recorded as planted, so there is nothing to lose either way."
                    } @else {
                        "This garden has " (planted)
                        @if planted == 1 { " planting recorded." } @else { " plantings recorded." }
                        " Simple mode hides them and stops asking about them; it does not "
                        "delete them. Switch back and they are exactly as you left them, and "
                        "the record of what has grown well here is kept either way."
                    }
                }
            }
        },
    ))
}

/// One mode, with what it will and will not tell you.
///
/// The caveat is not small print. Choosing a mode that says less is a reasonable thing
/// to want; discovering *which* things it stopped saying six weeks later, when a
/// harvest went by unmentioned, is not.
fn choice(
    mode: GardenMode,
    current: GardenMode,
    summary: &str,
    gives: &[&str],
    caveat: &str,
) -> Markup {
    html! {
        label.card style="display:block; cursor:pointer" {
            div.row {
                input type="radio" name="mode" value=(mode.slug())
                      checked[mode == current] style="width:auto; margin-right:0.6rem";
                div {
                    strong { (mode.label()) }
                    " — " (summary)
                }
            }
            ul.small style="margin:0.6rem 0 0.4rem 1.6rem" {
                @for line in gives { li { (line) } }
            }
            p.small.muted style="margin:0 0 0 1.6rem" { (caveat) }
        }
    }
}

/// Mint, show and revoke the counter display's URL.
///
/// Separated from the mode form so that saving a mode cannot mint a secret by
/// accident, and so that the URL is shown exactly once — at the moment it is created,
/// which is the only moment anyone is ready to copy it.
fn display_card(garden: &Garden, has_token: bool, fresh: Option<&str>) -> Markup {
    html! {
        div.card {
            h3 style="margin-top:0" { "Counter display" }
            p.small.muted {
                "A read-only address a small screen can poll for the garden-level work "
                "outstanding. It carries no session and cannot change anything — it "
                "reads this one garden and nothing else."
            }
            @if let Some(url) = fresh {
                p.small { "Copy this now. It is shown once:" }
                p.token { (url) }
            }
            div.row {
                form method="post" action=(format!("/gardens/{}/mode/display", garden.id)) {
                    button type="submit" {
                        @if has_token { "Replace the address" } @else { "Create an address" }
                    }
                }
                @if has_token {
                    form method="post"
                         action=(format!("/gardens/{}/mode/display/revoke", garden.id)) {
                        button type="submit" { "Revoke" }
                    }
                }
            }
            p.small style="margin:0.6rem 0 0" {
                a href=(format!("/gardens/{}/display/preview", garden.id)) {
                    "See what the screen will show →"
                }
            }
            @if has_token && fresh.is_none() {
                p.small.muted style="margin:0.6rem 0 0" {
                    "An address exists. Only its digest is stored, so a lost one is "
                    "replaced rather than recovered — which also revokes the old one."
                }
            }
        }
    }
}

async fn issue_display(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Redirect, AppError> {
    let garden = load(&state, &actor, &id).await?;
    let token = state.store.issue_display_token(garden.id, state.now()).await?;
    let url = format!(
        "{}/display/{}/status.json",
        state.config.base_url,
        token.expose()
    );
    Ok(Redirect::to(&format!(
        "/gardens/{}/mode?feed={}",
        garden.id,
        urlencode(&url)
    )))
}

async fn revoke_display(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Redirect, AppError> {
    let garden = load(&state, &actor, &id).await?;
    state.store.revoke_display_token(garden.id).await?;
    Ok(Redirect::to(&format!("/gardens/{}/mode", garden.id)))
}

/// Minimal percent-encoding for a query value.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn update(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Form(form): Form<ModeForm>,
) -> Result<Redirect, AppError> {
    let garden = load(&state, &actor, &id).await?;
    let mode = GardenMode::parse(&form.mode)
        .ok_or_else(|| AppError::BadRequest(format!("unknown mode {:?}", form.mode)))?;
    state.store.set_garden_mode(garden.id, mode, state.now()).await?;

    // Straight to the garden rather than back here. The change is visible there — the
    // slot list appears or disappears — and a settings page that stares back at you
    // after saving gives no sign anything happened.
    Ok(Redirect::to(&format!("/gardens/{}", garden.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_value_round_trips_through_the_slug() {
        // The radio posts `mode.slug()` and `update` parses it back. A mismatch would
        // reject every submission with a 400 that says nothing about why.
        for mode in [GardenMode::Simple, GardenMode::Advanced] {
            assert_eq!(GardenMode::parse(mode.slug()), Some(mode));
        }
    }

    #[test]
    fn an_unknown_mode_is_refused_rather_than_defaulted() {
        // Defaulting here would silently put a garden in advanced mode on a typo,
        // which is the failure the whole page is written to avoid.
        assert_eq!(GardenMode::parse("neither"), None);
        assert_eq!(GardenMode::parse(""), None);
    }

    #[test]
    fn exactly_one_mode_is_preselected() {
        // Two checked radios in one group, or none, both mean the form does not show
        // what the garden is currently set to.
        let rendered = html! {
            (choice(GardenMode::Simple, GardenMode::Simple, "s", &["a"], "c"))
            (choice(GardenMode::Advanced, GardenMode::Simple, "a", &["b"], "c"))
        }
        .into_string();
        assert_eq!(rendered.matches("checked").count(), 1, "{rendered}");
    }
}
