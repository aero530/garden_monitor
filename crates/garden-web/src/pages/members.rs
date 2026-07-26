//! Sharing a garden: members, invitations, ownership.

use crate::app::{AppState, Auth};
use crate::error::AppError;
use crate::pages::gardens::authorize;
use crate::ui;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use garden_auth::{Actor, EmailAddress, Invitation, Membership, Permission, Role, UserId};
use garden_core::{Garden, GardenId};
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gardens/{id}/members", get(page))
        .route("/gardens/{id}/members/invite", post(invite))
        .route("/gardens/{id}/members/{user}/role", post(set_role))
        .route("/gardens/{id}/members/{user}/remove", post(remove))
        .route("/gardens/{id}/members/{user}/transfer", post(transfer))
        .route("/gardens/{id}/leave", post(leave))
}

async fn page(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, _) = authorize(&state, &actor, id, Permission::ViewGarden).await?;
    render(&state, &actor, &garden, None).await
}

/// The sharing page, optionally showing a freshly minted invite link.
///
/// The link is rendered into the POST response rather than redirected to, because a
/// token in a URL ends up in browser history and in any proxy log along the way.
async fn render(
    state: &AppState,
    actor: &Actor,
    garden: &Garden,
    fresh_link: Option<&str>,
) -> Result<Markup, AppError> {
    let id = garden.id;
    let now = state.now();
    let members = state.store.members_of(id).await?;
    let invitations = state.store.invitations_for(id).await?;
    let grantable = actor.grantable_roles(id);
    let can_manage = actor.can(id, Permission::ManageMembers);
    let is_owner = actor.role_in(id) == Some(Role::Owner);

    let pending: Vec<&Invitation> = invitations.iter().filter(|i| i.is_pending(now)).collect();

    Ok(ui::page(
        &format!("Sharing · {}", garden.name),
        Some(actor),
        html! {
            div.row {
                div {
                    h1 { "Sharing" }
                    p.muted.small style="margin:0" {
                        a href=(format!("/gardens/{id}")) { (garden.name) }
                    }
                }
            }

            @if let Some(link) = fresh_link {
                div.card {
                    h3 { "Invitation created" }
                    p.small.muted {
                        "Send this link to them. It works once, expires in 14 days, and \
                         only the invited address can accept it."
                    }
                    p.token { (link) }
                }
            }

            h2 { "People" }
            div.table-wrap {
                table {
                    thead {
                        tr {
                            th { "Person" } th { "Role" } th { "Since" }
                            @if can_manage { th {} }
                        }
                    }
                    tbody {
                        @for member in &members {
                            tr {
                                td {
                                    (member.user.label())
                                    @if member.user.id == actor.id() { span.muted.small { " (you)" } }
                                    br;
                                    span.muted.small { (member.user.email) }
                                }
                                td { span.pill.sev-info { (member.role.label()) } }
                                td.small.muted {
                                    (ui::relative(now.as_second() - member.granted_at.as_second()))
                                }
                                @if can_manage {
                                    td {
                                        @if actor.can_manage_member(id, member.user.id, member.role) {
                                            form method="post"
                                                 action=(format!("/gardens/{id}/members/{}/role", member.user.id))
                                                 style="display:inline-flex; gap:0.3rem" {
                                                select name="role" style="width:auto" {
                                                    @for role in &grantable {
                                                        option value=(role.label())
                                                               selected[*role == member.role] { (role.label()) }
                                                    }
                                                }
                                                button type="submit" { "Set" }
                                            }
                                            form method="post"
                                                 action=(format!("/gardens/{id}/members/{}/remove", member.user.id))
                                                 style="display:inline" {
                                                button.link.danger type="submit" { "Remove" }
                                            }
                                            @if is_owner && member.role == Role::Steward {
                                                form method="post"
                                                     action=(format!("/gardens/{id}/members/{}/transfer", member.user.id))
                                                     style="display:inline"
                                                     onsubmit="return confirm('Hand this garden over? You will become a steward.')" {
                                                    button.link type="submit" { "Make owner" }
                                                }
                                            }
                                        } @else {
                                            span.muted.small { "—" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            @if can_manage && !grantable.is_empty() {
                h2 { "Invite someone" }
                form.card method="post" action=(format!("/gardens/{id}/members/invite")) {
                    div.row {
                        div style="flex:2; min-width:14rem" {
                            label for="email" { "Their email" }
                            input #email type="email" name="email" required placeholder="sam@example.com";
                        }
                        div style="flex:1; min-width:9rem" {
                            label for="role" { "Role" }
                            select #role name="role" {
                                @for role in &grantable {
                                    option value=(role.label()) selected[*role == Role::Caretaker] {
                                        (role.label())
                                    }
                                }
                            }
                        }
                    }
                    div style="margin-top:0.75rem" {
                        @for role in &grantable {
                            p.small.muted style="margin:0 0 0.2rem" {
                                strong { (role.label()) } " — " (role.description())
                            }
                        }
                    }
                    p style="margin-top:0.75rem" { button.primary type="submit" { "Create invitation" } }
                }
            }

            @if !pending.is_empty() {
                h2 { "Pending invitations" }
                div.table-wrap {
                    table {
                        thead { tr { th { "Address" } th { "Role" } th { "Expires" } } }
                        tbody {
                            @for invitation in &pending {
                                tr {
                                    td { (invitation.email) }
                                    td { span.pill.sev-info { (invitation.role.label()) } }
                                    td.small.muted { (invitation.expires_at.to_string()) }
                                }
                            }
                        }
                    }
                }
            }

            @if !is_owner {
                form.card method="post" action=(format!("/gardens/{id}/leave"))
                     onsubmit="return confirm('Leave this garden?')" {
                    h3 { "Leave this garden" }
                    p.small.muted { "You will lose access until someone invites you again." }
                    button.danger type="submit" { "Leave" }
                }
            }
        },
    ))
}

#[derive(Deserialize)]
pub struct InviteForm {
    email: String,
    role: String,
}

async fn invite(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
    Form(form): Form<InviteForm>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let (garden, role) = authorize(&state, &actor, id, Permission::ManageMembers).await?;
    let now = state.now();

    let email = EmailAddress::parse(&form.email)
        .map_err(|e| AppError::bad_request(format!("That address is not valid: {e}")))?;
    let target: Role = form
        .role
        .parse()
        .map_err(|_| AppError::bad_request("Pick a role."))?;

    // Checked again here rather than trusting the form: the select was rendered from
    // `grantable_roles`, but a form field is user input, not a constraint.
    if !role.can_grant(target) {
        return Err(AppError::Denied(garden_auth::AccessDenied::InsufficientRole {
            garden: id,
            held: role,
            required: Role::Owner,
            permission: Permission::ManageMembers,
        }));
    }

    let (invitation, token) = Invitation::issue(id, email, target, actor.id(), role, now)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    state.store.create_invitation(&invitation).await?;
    state
        .store
        .log_event(
            id,
            "member.invited",
            Some(&format!("{} invited {} as {target}", actor.user.label(), invitation.email)),
            Some(actor.id()),
            now,
        )
        .await?;

    let link = format!("{}/invite/{}", state.config.base_url, token.expose());
    Ok(render(&state, &actor, &garden, Some(&link))
        .await?
        .into_response())
}

#[derive(Deserialize)]
pub struct RoleForm {
    role: String,
}

async fn set_role(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, user)): Path<(String, String)>,
    Form(form): Form<RoleForm>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let target_user: UserId = user.parse().map_err(|_| AppError::NotFound)?;
    let (_, role) = authorize(&state, &actor, id, Permission::ManageMembers).await?;
    let now = state.now();

    let new_role: Role = form
        .role
        .parse()
        .map_err(|_| AppError::bad_request("Unknown role."))?;
    let current = state
        .store
        .role_of(id, target_user)
        .await?
        .ok_or(AppError::NotFound)?;

    // Two separate checks: may the caller act on this member at all, and may they
    // hand out the role they are asking for. Skipping either one is a privilege
    // escalation path.
    if !actor.can_manage_member(id, target_user, current) || !role.can_grant(new_role) {
        return Err(AppError::Denied(garden_auth::AccessDenied::InsufficientRole {
            garden: id,
            held: role,
            required: Role::Owner,
            permission: Permission::ManageMembers,
        }));
    }

    state
        .store
        .grant_membership(&Membership::granted(
            id,
            target_user,
            new_role,
            actor.id(),
            now,
        ))
        .await?;
    state
        .store
        .log_event(
            id,
            "member.role_changed",
            Some(&format!("role set to {new_role}")),
            Some(actor.id()),
            now,
        )
        .await?;

    Ok(Redirect::to(&format!("/gardens/{id}/members")).into_response())
}

async fn remove(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, user)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let target_user: UserId = user.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::ManageMembers).await?;

    let current = state
        .store
        .role_of(id, target_user)
        .await?
        .ok_or(AppError::NotFound)?;
    if !actor.can_manage_member(id, target_user, current) {
        return Err(AppError::bad_request(
            "You cannot remove that member.",
        ));
    }

    state.store.revoke_membership(id, target_user).await?;
    state
        .store
        .log_event(
            id,
            "member.removed",
            Some("access revoked"),
            Some(actor.id()),
            state.now(),
        )
        .await?;
    Ok(Redirect::to(&format!("/gardens/{id}/members")).into_response())
}

async fn transfer(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path((id, user)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let target_user: UserId = user.parse().map_err(|_| AppError::NotFound)?;
    authorize(&state, &actor, id, Permission::TransferOwnership).await?;

    state
        .store
        .transfer_ownership(id, actor.id(), target_user, state.now())
        .await?;
    state
        .store
        .log_event(
            id,
            "garden.ownership_transferred",
            Some("ownership handed over"),
            Some(actor.id()),
            state.now(),
        )
        .await?;
    Ok(Redirect::to(&format!("/gardens/{id}/members")).into_response())
}

/// Leaving is always allowed, except for the owner — a garden with no owner would be
/// unreachable by anyone.
async fn leave(
    State(state): State<AppState>,
    Auth(actor): Auth,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id: GardenId = id.parse().map_err(|_| AppError::NotFound)?;
    let role = actor.require(id, Permission::ViewGarden)?;
    if role == Role::Owner {
        return Err(AppError::bad_request(
            "Transfer ownership or delete the garden instead — a garden cannot be left \
             with no owner.",
        ));
    }

    state.store.revoke_membership(id, actor.id()).await?;
    state
        .store
        .log_event(
            id,
            "member.left",
            Some(&format!("{} left", actor.user.label())),
            Some(actor.id()),
            state.now(),
        )
        .await?;
    Ok(Redirect::to("/").into_response())
}
