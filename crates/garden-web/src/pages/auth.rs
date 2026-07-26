//! Sign in, register, accept an invitation, manage your account.

use crate::app::{AppState, Auth, MaybeAuth, clear_cookie, set_cookie, user_agent};
use crate::error::AppError;
use crate::ui;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header::SET_COOKIE};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Router, routing::get, routing::post};
use garden_auth::{
    EmailAddress, Membership, SecretToken, check_password_policy, session::DEFAULT_LIFETIME_DAYS,
};
use maud::{Markup, html};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_form).post(login))
        .route("/register", get(register_form).post(register))
        .route("/logout", post(logout))
        .route("/account", get(account))
        .route("/account/sign-out-everywhere", post(sign_out_everywhere))
        .route("/invite/{token}", get(invite_landing))
}

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    password: String,
}

#[derive(Deserialize, Default)]
pub struct AuthQuery {
    error: Option<String>,
    invite: Option<String>,
}

async fn login_form(
    MaybeAuth(actor): MaybeAuth,
    Query(query): Query<AuthQuery>,
) -> Response {
    if actor.is_some() {
        return Redirect::to("/").into_response();
    }
    ui::plain_page(
        "Sign in",
        html! {
            h1 { "🌱 Gardyn" }
            p.muted { "Sign in to your gardens." }
            @if let Some(error) = &query.error {
                p.error { (error) }
            }
            form method="post" action="/login" {
                label for="email" { "Email" }
                input #email type="email" name="email" required autocomplete="username" autofocus;
                label for="password" { "Password" }
                input #password type="password" name="password" required autocomplete="current-password";
                p style="margin-top:1rem" {
                    button.primary type="submit" style="width:100%" { "Sign in" }
                }
            }
            p.small.muted { "No account? " a href="/register" { "Register" } "." }
        },
    )
    .into_response()
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let Ok(email) = EmailAddress::parse(&form.email) else {
        return Ok(Redirect::to("/login?error=Check+your+details").into_response());
    };

    let outcome = state
        .store
        .authenticate(
            &email,
            &form.password,
            state.now(),
            user_agent(&headers),
        )
        .await?;

    let Some((_user, token)) = outcome else {
        // One message for every failure mode. Distinguishing "no such account" from
        // "wrong password" hands over a list of who is registered.
        return Ok(Redirect::to("/login?error=Those+details+did+not+match").into_response());
    };

    Ok(with_session_cookie(&state, Redirect::to("/"), &token))
}

#[derive(Deserialize)]
pub struct RegisterForm {
    email: String,
    display_name: String,
    password: String,
    invite: Option<String>,
}

async fn register_form(
    State(state): State<AppState>,
    Query(query): Query<AuthQuery>,
) -> Result<Response, AppError> {
    let first_user = state.store.user_count().await? == 0;

    // Registration is closed once the server has an owner. Otherwise a self-hosted
    // box on a shared network is an open sign-up form.
    let invitation = match &query.invite {
        Some(raw) => match SecretToken::from_client(raw) {
            Some(token) => state.store.find_invitation_by_token(&token).await?,
            None => None,
        },
        None => None,
    };

    if !first_user && invitation.is_none() {
        return Ok(ui::error_page(
            "Registration is closed",
            "This server is not open for sign-ups. Ask someone to share a garden with \
             you — the invitation link lets you create an account.",
        )
        .into_response());
    }

    let prefill = invitation.as_ref().map(|i| i.email.to_string());

    Ok(ui::plain_page(
        "Create an account",
        html! {
            h1 { "Create an account" }
            @if first_user {
                p.muted { "You are the first account on this server, so you will be its administrator." }
            } @else {
                p.muted { "You have been invited to help with a garden." }
            }
            @if let Some(error) = &query.error {
                p.error { (error) }
            }
            form method="post" action="/register" {
                @if let Some(invite) = &query.invite {
                    input type="hidden" name="invite" value=(invite);
                }
                label for="display_name" { "Your name" }
                input #display_name type="text" name="display_name" required autocomplete="name";
                label for="email" { "Email" }
                @match &prefill {
                    // The invitation names an address; changing it would just fail on
                    // acceptance, so it is fixed here rather than silently rejected later.
                    Some(email) => {
                        input #email type="email" name="email" value=(email) readonly;
                        p.small.muted { "Fixed by the invitation." }
                    }
                    None => { input #email type="email" name="email" required autocomplete="email"; }
                }
                label for="password" { "Password" }
                input #password type="password" name="password" required
                      minlength="12" autocomplete="new-password";
                p.small.muted { "At least 12 characters." }
                p style="margin-top:1rem" {
                    button.primary type="submit" style="width:100%" { "Create account" }
                }
            }
            p.small.muted { "Already registered? " a href="/login" { "Sign in" } "." }
        },
    )
    .into_response())
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Result<Response, AppError> {
    let now = state.now();
    let first_user = state.store.user_count().await? == 0;

    let invitation = match form.invite.as_deref().and_then(SecretToken::from_client) {
        Some(token) => state.store.find_invitation_by_token(&token).await?,
        None => None,
    };
    if !first_user && invitation.is_none() {
        return Err(AppError::bad_request("This server is not open for sign-ups."));
    }

    let Ok(email) = EmailAddress::parse(&form.email) else {
        return Ok(redirect_with_error("/register", "That email address is not valid"));
    };
    if let Err(weak) = check_password_policy(&form.password) {
        return Ok(redirect_with_error("/register", &weak.to_string()));
    }

    // Validate the invitation *before* creating anything.
    //
    // `Invitation::accept` enforces the recipient too, but it runs after the account
    // exists. On a server with registration otherwise closed, that ordering let a
    // stranger holding a leaked link register under their own address: they got no
    // garden access, but they got an account, which is exactly what closed
    // registration is supposed to prevent.
    if let Some(invitation) = &invitation {
        if !invitation.is_pending(now) {
            return Ok(redirect_with_error(
                "/register",
                "That invitation is no longer valid",
            ));
        }
        if invitation.email != email {
            return Ok(redirect_with_error(
                "/register",
                "That invitation was sent to a different address",
            ));
        }
    }

    let user = match state
        .store
        .create_user(email, &form.display_name, &form.password, now)
        .await
    {
        Ok(user) => user,
        Err(garden_store::StoreError::EmailTaken) => {
            return Ok(redirect_with_error(
                "/register",
                "That address is already registered — sign in instead",
            ));
        }
        Err(e) => return Err(e.into()),
    };

    // Accepting turns the invitation into a membership. The recipient check inside
    // `accept` is what stops a forwarded link working for someone else.
    if let Some(mut invitation) = invitation {
        match invitation.accept(&user, now) {
            Ok(role) => {
                state.store.save_invitation(&invitation).await?;
                state
                    .store
                    .grant_membership(&Membership::granted(
                        invitation.garden,
                        user.id,
                        role,
                        invitation.invited_by,
                        now,
                    ))
                    .await?;
                state
                    .store
                    .log_event(
                        invitation.garden,
                        "member.joined",
                        Some(&format!("{} joined as {role}", user.label())),
                        Some(user.id),
                        now,
                    )
                    .await?;
            }
            Err(e) => return Ok(redirect_with_error("/register", &e.to_string())),
        }
    }

    let token = state
        .store
        .open_session(user.id, now, user_agent(&headers))
        .await?;
    Ok(with_session_cookie(&state, Redirect::to("/"), &token))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(raw) = crate::app::read_cookie(&headers, state.config.cookie_name())
        && let Some(token) = SecretToken::from_client(raw)
    {
        state.store.close_session(&token).await?;
    }

    let mut response = Redirect::to("/login").into_response();
    let cookie = clear_cookie(state.config.cookie_name(), state.config.secure_cookies);
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    Ok(response)
}

/// Landing page for an invitation link.
///
/// Branches on whether the recipient already has an account, because "share this
/// garden with Sam" has to work whether or not Sam has ever used the system.
async fn invite_landing(
    State(state): State<AppState>,
    MaybeAuth(actor): MaybeAuth,
    Path(raw_token): Path<String>,
) -> Result<Response, AppError> {
    let now = state.now();
    let Some(token) = SecretToken::from_client(&raw_token) else {
        return Ok(ui::error_page("Invalid link", "That invitation link is not valid.").into_response());
    };
    let Some(mut invitation) = state.store.find_invitation_by_token(&token).await? else {
        return Ok(ui::error_page("Invalid link", "That invitation link is not valid.").into_response());
    };

    if !invitation.is_pending(now) {
        return Ok(ui::error_page(
            "That invitation is no longer valid",
            &format!("It is {}.", invitation.status(now).label()),
        )
        .into_response());
    }

    let garden = state.store.find_garden(invitation.garden).await?;
    let garden_name = garden.map(|g| g.name).unwrap_or_else(|| "a garden".into());

    // Already signed in: accept immediately if the addresses match.
    if let Some(actor) = actor {
        return match invitation.accept(&actor.user, now) {
            Ok(role) => {
                state.store.save_invitation(&invitation).await?;
                state
                    .store
                    .grant_membership(&Membership::granted(
                        invitation.garden,
                        actor.id(),
                        role,
                        invitation.invited_by,
                        now,
                    ))
                    .await?;
                state
                    .store
                    .log_event(
                        invitation.garden,
                        "member.joined",
                        Some(&format!("{} joined as {role}", actor.user.label())),
                        Some(actor.id()),
                        now,
                    )
                    .await?;
                Ok(Redirect::to(&format!("/gardens/{}", invitation.garden)).into_response())
            }
            Err(e) => Ok(ui::error_page(
                "That invitation is not for this account",
                &format!(
                    "{e}. It was sent to {}, and you are signed in as {}.",
                    invitation.email,
                    actor.user.email
                ),
            )
            .into_response()),
        };
    }

    // Not signed in: offer both paths.
    Ok(ui::plain_page(
        "You have been invited",
        html! {
            h1 { "Join " (garden_name) }
            p.muted {
                "You have been invited to help with " strong { (garden_name) }
                " as a " strong { (invitation.role.label()) } "."
            }
            p.small.muted { (invitation.role.description()) }
            p { a.button.primary href=(format!("/register?invite={}", token.expose())) { "Create an account" } }
            p.small.muted {
                "Already have an account? " a href="/login" { "Sign in" }
                " and open this link again."
            }
        },
    )
    .into_response())
}

async fn account(State(state): State<AppState>, Auth(actor): Auth) -> Result<Markup, AppError> {
    let now = state.now();
    let sessions = state.store.sessions_of(actor.id()).await?;
    let gardens = state.store.gardens_for_user(actor.id()).await?;

    Ok(ui::page(
        "Account",
        Some(&actor),
        html! {
            h1 { (actor.user.label()) }
            p.muted { (actor.user.email) }
            @if actor.is_admin() {
                p { span.pill.health-up { "server administrator" } }
            }

            h2 { "Gardens" }
            p.muted.small {
                (gardens.len()) " garden" @if gardens.len() != 1 { "s" }
                " · " (gardens.iter().filter(|g| g.is_someone_elses()).count()) " shared with you"
            }

            h2 { "Notifications" }
            p { a.button href="/account/notifications" { "Notification settings" } }

            h2 { "Signed-in devices" }
            div.table-wrap {
                table {
                    thead { tr { th { "Device" } th { "Last used" } th { "Expires" } } }
                    tbody {
                        @for session in &sessions {
                            tr {
                                td.small { (session.user_agent.as_deref().unwrap_or("unknown")) }
                                td.small.muted {
                                    (ui::relative(now.as_second() - session.last_seen_at.as_second()))
                                }
                                td.small.muted { (session.expires_at.to_string()) }
                            }
                        }
                    }
                }
            }
            form method="post" action="/account/sign-out-everywhere" style="margin-top:1rem" {
                button.danger type="submit" { "Sign out everywhere" }
            }
        },
    ))
}

async fn sign_out_everywhere(
    State(state): State<AppState>,
    Auth(actor): Auth,
) -> Result<Response, AppError> {
    state.store.close_all_sessions(actor.id()).await?;
    let mut response = Redirect::to("/login").into_response();
    let cookie = clear_cookie(state.config.cookie_name(), state.config.secure_cookies);
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    Ok(response)
}

fn redirect_with_error(path: &str, message: &str) -> Response {
    let encoded = message.replace(' ', "+");
    Redirect::to(&format!("{path}?error={encoded}")).into_response()
}

fn with_session_cookie(state: &AppState, redirect: Redirect, token: &SecretToken) -> Response {
    let mut response = redirect.into_response();
    let cookie = set_cookie(
        state.config.cookie_name(),
        token.expose(),
        (DEFAULT_LIFETIME_DAYS * 86_400.0) as i64,
        state.config.secure_cookies,
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    response
}
