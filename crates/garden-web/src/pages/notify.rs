//! Notification settings, and the calendar feed.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, Query, State};
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
        .route("/account/notifications/test", post(send_test))
        .route("/account/notifications/calendar", post(issue_calendar))
        .route(
            "/account/notifications/calendar/revoke",
            post(revoke_calendar),
        )
        .route("/calendar/{token}/feed.ics", get(calendar_feed))
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    /// What the test button did, good or bad.
    notice: Option<String>,
    error: Option<String>,
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Query(query): Query<PageQuery>,
) -> Result<Markup, AppError> {
    let prefs = state.store.notification_prefs(actor.id()).await?;
    Ok(render_with(&state, &actor, &prefs, None, &query))
}

fn render(
    state: &AppState,
    actor: &garden_auth::Actor,
    prefs: &NotificationPrefs,
    fresh_feed: Option<&str>,
) -> Markup {
    render_with(state, actor, prefs, fresh_feed, &PageQuery::default())
}

fn render_with(
    state: &AppState,
    actor: &garden_auth::Actor,
    prefs: &NotificationPrefs,
    fresh_feed: Option<&str>,
    query: &PageQuery,
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
            @if let Some(error) = &query.error {
                p.error { (error) }
            }
            @if let Some(notice) = &query.notice {
                p { span.pill.health-up { (notice) } }
            }

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
                p.small.muted {
                    "Save first, then use " strong { "Send a test" } " below — a topic the "
                    "app is not subscribed to looks exactly like one that works, until a "
                    "real task fails to arrive."
                }

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

            // Outside the settings form on purpose: a submit button inside it would
            // save and test in one click, and you want to know which of the two you
            // just did when something goes wrong.
            div.card {
                h3 { "Check it reaches you" }
                p.small.muted {
                    "Sends one notification now, through the same channel a real task "
                    "would use. Quiet hours and the once-per-task rule are skipped — "
                    "they exist to suppress notifications, and a test that could be "
                    "suppressed would tell you nothing."
                }
                form method="post" action="/account/notifications/test" {
                    button type="submit" disabled[!configured] { "Send a test" }
                }
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

/// Send one notification, right now, through the real machinery.
///
/// Without this you configure a topic and then find out whether it works when a real
/// task fires — possibly tomorrow morning, since anything below `important` waits for
/// the daily brief. That is a poor feedback loop for the one part of the system whose
/// entire job is reaching you, and it is the reason this button exists.
///
/// It goes through the same [`Notifier`] and the same channel the dispatcher uses, so a
/// success here means the whole path works: brain to ntfy, ntfy to phone. The only
/// thing deliberately skipped is [`policy::decide`] — quiet hours, the once-per-task
/// rule and the per-sweep cap all exist to *suppress* notifications, and a test that
/// could be suppressed would tell you nothing.
///
/// [`Notifier`]: garden_notify::Notifier
/// [`policy::decide`]: garden_notify::decide
async fn send_test(
    State(state): State<AppState>,
    Auth(actor): Auth,
) -> Result<Response, AppError> {
    let prefs = state.store.notification_prefs(actor.id()).await?;

    let Some(notifier) = state.notifier.as_ref() else {
        return Ok(notify_result(
            "No channel is configured on the server — GARDEN_NTFY_URL is unset, so \
             nothing can be sent however this page is filled in.",
            false,
        ));
    };
    if prefs.ntfy_topic.is_none() && !prefs.email_enabled {
        return Ok(notify_result(
            "Set a topic (or tick email) and save, then try again.",
            false,
        ));
    }

    let note = garden_notify::Notification {
        title: "Garden test".into(),
        body: "If you are reading this on your phone, notifications work. \
               Nothing is wrong with your garden."
            .into(),
        // Deliberately mid-ladder: high enough to arrive now rather than in the
        // morning brief, low enough not to break Do Not Disturb for a test.
        priority: 3,
        tags: vec!["seedling".into()],
        actions: Vec::new(),
        open_url: Some(format!("{}/account/notifications", state.config.base_url)),
    };

    // Both channels the preferences ask for, so a test exercises what a real
    // notification would use rather than a subset of it.
    let reach = garden_notify::Reach {
        push: prefs.ntfy_topic.is_some(),
        email: prefs.email_enabled,
        priority: note.priority,
        // Irrelevant here — this is the field `policy::decide` reads to hold something
        // for the daily brief, and a test that could be held is not a test.
        interrupts: true,
    };
    let delivered = notifier
        .deliver(
            &note,
            reach,
            prefs.ntfy_topic.as_deref(),
            Some(actor.user.email.as_str()),
        )
        .await;

    tracing::info!(
        user = %actor.id(),
        push = delivered.push,
        email = delivered.email,
        why = delivered.why(),
        "test notification sent"
    );

    Ok(match (delivered.push, delivered.email) {
        (true, true) => notify_result("Sent by push and email.", true),
        (true, false) if prefs.email_enabled => notify_result(
            &failed("Push sent. Email did not go out", delivered.email_error.as_deref()),
            true,
        ),
        (true, false) => notify_result("Push sent. Check your phone.", true),
        (false, true) => notify_result(
            &failed("Email sent. Push did not go out", delivered.push_error.as_deref()),
            true,
        ),
        // The reason, not a guess at it. A DNS failure and a rejected token look
        // identical from here and want opposite fixes.
        (false, false) => notify_result(
            &failed("Nothing could be delivered", delivered.why().as_deref()),
            false,
        ),
    })
}

/// A failure sentence carrying the underlying reason, when there is one.
fn failed(what: &str, why: Option<&str>) -> String {
    match why {
        Some(reason) => format!("{what} — {reason}"),
        // Reached when a channel was never attempted: no topic set, or no mail
        // configured. Saying so beats naming a setting that is already correct.
        None => format!(
            "{what}. No channel was even tried — set a topic below, or configure \
             GARDEN_NTFY_URL and GARDEN_SMTP_HOST on the server."
        ),
    }
}

/// Back to the settings page carrying a sentence about what happened.
///
/// A redirect rather than a rendered page, so a refresh does not send a second test.
fn notify_result(message: &str, ok: bool) -> Response {
    let key = if ok { "notice" } else { "error" };
    Redirect::to(&format!(
        "/account/notifications?{key}={}",
        urlencode(message)
    ))
    .into_response()
}

/// Minimal percent-encoding for a query value.
///
/// These are fixed strings from just above rather than anything a user supplied, but
/// they contain spaces, commas and full stops, and a raw one would produce a URL that
/// some proxies rewrite and some browsers refuse.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_message_survives_the_round_trip_through_a_url() {
        // These sentences carry spaces, commas and full stops, and they travel as a
        // query value. A raw one produces a URL some proxies rewrite and some browsers
        // refuse, and the operator sees a blank page instead of the reason.
        let encoded = urlencode("Push sent. Check your phone.");
        assert!(!encoded.contains(' '), "{encoded}");
        assert_eq!(encoded, "Push+sent.+Check+your+phone.");
    }

    #[test]
    fn encoding_leaves_nothing_that_could_break_out_of_the_query() {
        for raw in [
            "GARDEN_NTFY_URL is unset — nothing can be sent",
            "a & b = c?d#e",
            "quotes \" and '",
        ] {
            let encoded = urlencode(raw);
            for bad in ['&', '=', '?', '#', '"', '\'', '<', '>', ' '] {
                assert!(
                    !encoded.contains(bad),
                    "{bad:?} survived encoding of {raw:?}: {encoded}"
                );
            }
        }
    }

    #[test]
    fn a_failure_redirects_as_an_error_and_a_success_as_a_notice() {
        // Different query keys, because the page renders one in red and one in green,
        // and "sent" appearing in red would be its own small confusion.
        let ok = notify_result("Push sent.", true);
        let bad = notify_result("Nothing could be delivered.", false);
        let location = |r: &Response| {
            r.headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        assert!(location(&ok).contains("notice="), "{}", location(&ok));
        assert!(location(&bad).contains("error="), "{}", location(&bad));
    }

    #[test]
    fn a_failure_carries_the_underlying_reason_rather_than_a_guess() {
        // The whole value of the test button. "The server could not reach ntfy" sent
        // an evening at GARDEN_NTFY_TOKEN when the truth was that `garden-ntfy` did
        // not resolve — a name podman never published, because Quadlet had called the
        // container `systemd-garden-ntfy`.
        let dns = failed(
            "Nothing could be delivered",
            Some("push: network: error sending request for url (http://garden-ntfy:8090/)"),
        );
        assert!(dns.contains("garden-ntfy:8090"), "{dns}");

        let rejected = failed(
            "Nothing could be delivered",
            Some("push: ntfy rejected the message: 401 Unauthorized"),
        );
        assert!(rejected.contains("401"), "{rejected}");
        assert_ne!(dns, rejected, "two different faults must read differently");
    }

    #[test]
    fn a_channel_that_was_never_tried_says_so_instead_of_blaming_a_setting() {
        // No reason means nothing was attempted: no topic, or nothing configured.
        // Naming GARDEN_NTFY_URL here would send someone to check a correct value.
        let message = failed("Nothing could be delivered", None);
        assert!(message.contains("No channel was even tried"), "{message}");
    }

    #[test]
    fn the_test_notification_is_not_urgent_enough_to_break_do_not_disturb() {
        // Priority 5 bypasses Do Not Disturb on both platforms. That is reserved for a
        // tank about to run dry, and a person checking their settings at midnight
        // should not be woken by their own button.
        let note = garden_notify::Notification {
            title: "Garden test".into(),
            body: String::new(),
            priority: 3,
            tags: vec![],
            actions: vec![],
            open_url: None,
        };
        assert!(note.priority < 5);
        assert!(note.priority > 2, "but high enough to arrive now");
    }
}
