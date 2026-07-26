//! Servers and applications.
//!
//! Deliberately administrator-only, and deliberately *only* infrastructure. An admin
//! can see that a Pi is offline or the broker is down; they cannot see what anyone is
//! growing. That separation is enforced in `Actor` — `require_admin` and `require`
//! are different questions and never consult each other.

use crate::app::{AdminAuth, AppState};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Router, routing::get, routing::post};
use gardyn_store::fleet::Health;
use maud::{Markup, html};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/system", get(page))
        .route("/system/components/{id}/forget", post(forget))
}

async fn page(
    State(state): State<AppState>,
    AdminAuth(actor): AdminAuth,
) -> Result<Markup, AppError> {
    let now = state.now();
    let components = state.store.components(now).await?;
    let users = state.store.user_count().await?;

    let up = components.iter().filter(|c| c.health(now) == Health::Up).count();
    let problems = components.iter().filter(|c| c.health(now).is_problem()).count();
    let unknown = components
        .iter()
        .filter(|c| c.health(now) == Health::Unknown)
        .count();

    Ok(ui::page(
        "System",
        Some(&actor),
        html! {
            h1 { "System" }
            p.muted.small {
                "Infrastructure health. Garden contents are not shown here — being a \
                 server administrator does not grant access to anyone's garden."
            }

            div.grid {
                div.card {
                    div.stat-label { "Up" }
                    div.stat style="color:var(--up)" { (up) }
                }
                div.card {
                    div.stat-label { "Problems" }
                    div.stat style=(if problems > 0 { "color:var(--down)" } else { "" }) { (problems) }
                }
                div.card {
                    div.stat-label { "Never reported" }
                    div.stat { (unknown) }
                }
                div.card {
                    div.stat-label { "Accounts" }
                    div.stat { (users) }
                }
            }

            h2 { "Components" }
            @if components.is_empty() {
                div.card {
                    p { "Nothing has registered yet." }
                    p.muted.small {
                        "Agents register themselves on first run by posting to "
                        code { "/api/components/register" } " with the agent token."
                    }
                }
            } @else {
                div.table-wrap {
                    table {
                        thead {
                            tr {
                                th { "Component" } th { "Kind" } th { "Version" }
                                th { "Health" } th { "Last seen" } th {}
                            }
                        }
                        tbody {
                            @for component in &components {
                                @let health = component.health(now);
                                tr {
                                    td {
                                        strong { (component.name) }
                                        @if let Some(endpoint) = &component.endpoint {
                                            br; span.muted.small { (endpoint) }
                                        }
                                        @if let Some(detail) = &component.detail {
                                            @if health.is_problem() {
                                                br; span.small.error { (detail) }
                                            }
                                        }
                                    }
                                    td.small.muted { (component.kind) }
                                    td.small.muted { (component.version.as_deref().unwrap_or("—")) }
                                    td { (ui::health_pill(health)) }
                                    td.small.muted {
                                        @match component.seconds_since_seen(now) {
                                            Some(seconds) => (ui::relative(seconds)),
                                            None => "never",
                                        }
                                    }
                                    td {
                                        form method="post"
                                             action=(format!("/system/components/{}/forget", component.id))
                                             onsubmit="return confirm('Forget this component?')" {
                                            button.link type="submit" { "Forget" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            h2 { "Agent registration" }
            div.card {
                p.small.muted { "Point an agent at this server:" }
                p.token {
                    "curl -X POST " (state.config.base_url) "/api/components/register \\" br;
                    "  -H 'Authorization: Bearer $GARDYN_AGENT_TOKEN' \\" br;
                    "  -H 'Content-Type: application/json' \\" br;
                    r#"  -d '{"name":"kitchen-edge","kind":"edge-agent","heartbeat_seconds":60}'"#
                }
            }
        },
    ))
}

async fn forget(
    State(state): State<AppState>,
    AdminAuth(_): AdminAuth,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id = Uuid::parse_str(&id).map_err(|_| AppError::NotFound)?;
    state.store.delete_component(id).await?;
    Ok(Redirect::to("/system").into_response())
}
