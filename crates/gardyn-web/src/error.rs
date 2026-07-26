//! Turning failures into responses without leaking anything.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use gardyn_auth::AccessDenied;
use gardyn_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("sign in to continue")]
    NotSignedIn,

    /// For machine callers. Distinct from [`AppError::NotSignedIn`] because an agent
    /// needs a status code it can branch on, not a 303 to an HTML login form.
    #[error("invalid credentials")]
    Unauthorized,

    #[error(transparent)]
    Denied(#[from] AccessDenied),

    #[error("not found")]
    NotFound,

    #[error("{0}")]
    BadRequest(String),

    #[error(transparent)]
    Store(#[from] StoreError),
}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            // Bounce to the login page rather than showing a bare 401 — this is a
            // browser app, not an API.
            AppError::NotSignedIn => Redirect::to("/login").into_response(),

            AppError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
            }

            AppError::Denied(denied) => {
                // The critical mapping. A garden the caller is not a member of must
                // answer exactly as a garden that does not exist, or the id in the URL
                // becomes an existence oracle and sharing links become enumerable.
                if denied.conceals_existence() {
                    render(StatusCode::NOT_FOUND, "Not found", "No such garden.")
                } else {
                    render(StatusCode::FORBIDDEN, "Not allowed", &denied.to_string())
                }
            }

            AppError::NotFound => render(StatusCode::NOT_FOUND, "Not found", "No such page."),

            AppError::BadRequest(message) => {
                render(StatusCode::BAD_REQUEST, "That did not work", &message)
            }

            AppError::Store(error) => {
                // Log the detail; show the user nothing that describes our internals.
                tracing::error!(%error, "database error");
                render(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something broke",
                    "That failed on our side. The error has been logged.",
                )
            }
        }
    }
}

fn render(status: StatusCode, heading: &str, message: &str) -> Response {
    (status, crate::ui::error_page(heading, message)).into_response()
}
