//! Notification settings, and the calendar feed.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use garden_auth::{Permission, SecretToken};
use garden_notify::{CalendarTask, QuietHours, render_calendar};
use garden_store::notifications::NotificationPrefs;
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/account/notifications", get(page).post(save))
        .route("/account/notifications/calendar", post(issue_calendar))
        .route(
            "/account/notifications/calendar/revoke",
            post(revoke_calendar),
        )
        .route("/calendar/{token}/feed.ics", get(calendar_feed))
}

async fn page(State(state): State<AppState>, Auth(actor): Auth) -> Result<Markup, AppError> {
    let prefs = state.store.notification_prefs(actor.id()).await?;
    Ok(render(&state, &actor, &prefs, None))
}

fn render(
    state: &AppState,
    actor: &garden_auth::Actor,
    prefs: &NotificationPrefs,
    fresh_feed: Option<&str>,
) -> Markup {
    let configured = state.notifier.is_some();
    let suggested_topic = format!(
        "garden-{}",
        actor
            .user
            .email
            .as_str()
            .split('@')
            .next()
            .unwrap_or("me")
            .replace(|c: char| !c.is_ascii_alphanumeric(), "")
    );

    ui::page(
        "Notifications",
        Some(actor),
        html! {
            h1 { "Notifications" }

            @if !configured {
                div.card {
                    h3 { "No channels configured on this server" }
                    p.muted.small style="margin:0" {
                        "The administrator has not set " code { "GARDEN_NTFY_URL" }
                        " or SMTP details, so nothing will be sent however this page is "
                        "filled in. See NOTIFICATIONS.md."
                    }
                }
            }

            form.card method="post" action="/account/notifications" {
                h3 { "Push" }
                p.small.muted {
                    "Install the ntfy app, point it at this server, and subscribe to a "
                    "topic. Anyone who knows the topic name can publish to it, so pick "
                    "something nobody would guess."
                }
                label for="ntfy_topic" { "ntfy topic" }
                input #ntfy_topic type="text" name="ntfy_topic"
                      value=(prefs.ntfy_topic.clone().unwrap_or_default())
                      placeholder=(format!("{suggested_topic}-8f3a2c"));
                p.small.muted { "Leave blank for no push notifications." }

                h3 style="margin-top:1.5rem" { "Email" }
                label {
                    input type="checkbox" name="email_enabled" value="1"
                          checked[prefs.email_enabled] style="width:auto; margin-right:0.4rem";
                    "Email me at " (actor.user.email)
                }
                p.small.muted {
                    "Urgent and critical only. Self-hosted outbound mail is unreliable, "
                    "so treat push as the channel that actually works."
                }

                h3 style="margin-top:1.5rem" { "Quiet hours" }
                p.small.muted {
                    "Nothing below critical is delivered during this window. A tank about "
                    "to run dry still wakes you."
                }
                div.row {
                    div style="flex:1; min-width:7rem" {
                        label for="quiet_from" { "From" }
                        input #quiet_from type="number" name="quiet_from" min="0" max="23"
                              value=(prefs.quiet.from_hour);
                    }
                    div style="flex:1; min-width:7rem" {
                        label for="quiet_to" { "Until" }
                        input #quiet_to type="number" name="quiet_to" min="0" max="23"
                              value=(prefs.quiet.to_hour);
                    }
                    div style="flex:1; min-width:9rem" {
                        label for="utc_offset" { "Your UTC offset (minutes)" }
                        input #utc_offset type="number" name="utc_offset" min="-840" max="840"
                              step="15" value=(prefs.utc_offset_minutes);
                    }
                }
                p.small.muted {
                    "Offset is yours, not the garden's — you might not live where it does. "
                    "US Mountain is −420 in summer, −360 in winter."
                }

                p style="margin-top:1rem" { button.primary type="submit" { "Save" } }
            }

            div.card {
                h3 { "Calendar feed" }
                p.small.muted {
                    "Subscribe once in Google or Apple Calendar and scheduled work shows "
                    "up beside everything else. Read-only, and it covers every garden you "
                    "can see."
                }
                @if let Some(url) = fresh_feed {
                    p.small { "Copy this now — it is shown once:" }
                    p.token { (url) }
                }
                div.row {
                    form method="post" action="/account/notifications/calendar" {
                        button type="submit" {
                            @if prefs.has_calendar_feed { "Replace the link" } @else { "Create a link" }
                        }
                    }
                    @if prefs.has_calendar_feed {
                        form method="post" action="/account/notifications/calendar/revoke" {
                            button.link.danger type="submit" { "Revoke" }
                        }
                    }
                }
                @if prefs.has_calendar_feed && fresh_feed.is_none() {
                    p.small.muted style="margin-top:0.6rem" {
                        "A link exists. Only its digest is stored, so it cannot be shown \
                         again — replace it if you lost it."
                    }
                }
            }

            p.small.muted { a href="/account" { "Back to your account" } }
        },
    )
}

#[derive(Deserialize)]
pub struct PrefsForm {
    ntfy_topic: Option<String>,
    email_enabled: Option<String>,
    quiet_from: Option<i64>,
    quiet_to: Option<i64>,
    utc_offset: Option<i64>,
}

async fn save(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Form(form): Form<PrefsForm>,
) -> Result<Response, AppError> {
    let existing = state.store.notification_prefs(actor.id()).await?;
    let prefs = NotificationPrefs {
        user: actor.id(),
        ntfy_topic: form
            .ntfy_topic
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        // An unchecked checkbox is simply absent from the form body.
        email_enabled: form.email_enabled.is_some(),
        quiet: QuietHours {
            from_hour: form.quiet_from.unwrap_or(21).clamp(0, 23) as u8,
            to_hour: form.quiet_to.unwrap_or(7).clamp(0, 23) as u8,
        },
        utc_offset_minutes: form.utc_offset.unwrap_or(0).clamp(-840, 840) as i32,
        has_calendar_feed: existing.has_calendar_feed,
    };
    state.store.save_notification_prefs(&prefs).await?;
    Ok(Redirect::to("/account/notifications").into_response())
}

async fn issue_calendar(
    State(state): State<AppState>,
    Auth(actor): Auth,
) -> Result<Markup, AppError> {
    let token = state.store.issue_calendar_feed(actor.id()).await?;
    let url = format!(
        "{}/calendar/{}/feed.ics",
        state.config.base_url,
        token.expose()
    );
    let prefs = state.store.notification_prefs(actor.id()).await?;
    // Rendered into the POST response rather than redirected to: a secret in a URL
    // ends up in browser history and in every proxy log on the way.
    Ok(render(&state, &actor, &prefs, Some(&url)))
}

async fn revoke_calendar(
    State(state): State<AppState>,
    Auth(actor): Auth,
) -> Result<Response, AppError> {
    state.store.revoke_calendar_feed(actor.id()).await?;
    Ok(Redirect::to("/account/notifications").into_response())
}

/// The feed itself.
///
/// The absolute URL of the guide for a stored task kind, if it has one.
fn guide_url(base_url: &str, kind_label: &str) -> Option<String> {
    let slug = garden_core::TaskKind::from_label(kind_label)?.guide_slug()?;
    Some(format!("{base_url}/guides/{slug}"))
}

/// The only route where a bearer secret substitutes for a session, because a calendar
/// client cannot log in. It is scoped tightly: read-only, and only the tasks of
/// gardens that person is already a member of.
async fn calendar_feed(
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Result<Response, AppError> {
    let now = state.now();
    let token = SecretToken::from_client(&raw).ok_or(AppError::NotFound)?;
    let user = state
        .store
        .user_for_calendar_token(&token)
        .await?
        .ok_or(AppError::NotFound)?;

    let listings = state.store.gardens_for_user(user).await?;
    let mut entries = Vec::new();
    let mut names = Vec::new();

    for listing in &listings {
        if !listing.role.grants(Permission::CompleteTask) {
            continue;
        }
        names.push(listing.garden.name.clone());
        for task in state.store.tasks_for(listing.garden.id).await? {
            if !task.is_actionable(now) {
                continue;
            }
            entries.push(CalendarTask {
                // Stable, so a client updates the entry instead of duplicating it on
                // every refresh.
                uid: format!("{}-{}@garden", task.key, listing.garden.id),
                summary: match &task.detail {
                    Some(detail) => format!("{} ({}) — {}", task.kind, detail, listing.garden.name),
                    None => format!("{} — {}", task.kind, listing.garden.name),
                },
                // A calendar entry has no buttons, so the link to the procedure has to
                // go in the text. Worth the two lines: a refresh reminder that surfaces
                // in a calendar a week out is exactly when you want the steps.
                description: match guide_url(&state.config.base_url, &task.kind) {
                    Some(url) => format!("{}\n\nHow to do this: {url}", task.rationale),
                    None => task.rationale.clone(),
                },
                due: task.due_at,
                severity: task.severity,
            });
        }
    }

    let title = match names.len() {
        0 => "Gardyn".to_string(),
        1 => format!("Gardyn — {}", names[0]),
        n => format!("Gardyn — {n} gardens"),
    };

    Ok((
        [
            (CONTENT_TYPE, garden_notify::calendar::CONTENT_TYPE),
            // Calendar clients poll aggressively; a garden does not change that fast.
            (CACHE_CONTROL, "private, max-age=900"),
        ],
        render_calendar(&title, &entries, now),
    )
        .into_response())
}
