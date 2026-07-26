//! Email, best effort.
//!
//! Kept honest about what this is: outbound SMTP from a residential IP is rejected on
//! reputation by most large receivers, whatever the message says. Point it at a relay
//! you already trust — your own mailcow, a VPS, or your ISP's smarthost — and treat
//! push as the channel that actually works.
//!
//! Configuration is deliberately plain host/port/credentials rather than anything
//! clever, because the thing most likely to be wrong is the relay, not the code.

use crate::message::Notification;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error("address is not valid: {0}")]
    Address(String),
    #[error("could not build the message: {0}")]
    Build(String),
    #[error("smtp: {0}")]
    Transport(String),
}

#[derive(Debug, Clone)]
pub struct EmailConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Envelope sender. Must be something the relay will accept, which is usually the
    /// single most common reason mail silently vanishes.
    pub from: String,
    /// STARTTLS on the submission port. Off only for a relay on localhost.
    pub starttls: bool,
}

pub struct EmailChannel {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
}

impl EmailChannel {
    pub fn new(config: EmailConfig) -> Result<Self, EmailError> {
        let mut builder = if config.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| EmailError::Transport(e.to_string()))?
        } else {
            // Unencrypted, for a relay on the same host or the same Podman network.
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
        }
        .port(config.port);

        if let (Some(user), Some(password)) = (&config.username, &config.password) {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }

        Ok(Self {
            transport: builder.build(),
            from: config.from,
        })
    }

    pub async fn send(&self, to: &str, note: &Notification) -> Result<(), EmailError> {
        let message = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|_| EmailError::Address(self.from.clone()))?,
            )
            .to(to.parse().map_err(|_| EmailError::Address(to.to_string()))?)
            .subject(&note.title)
            .header(ContentType::TEXT_PLAIN)
            .body(render_body(note))
            .map_err(|e| EmailError::Build(e.to_string()))?;

        self.transport
            .send(message)
            .await
            .map_err(|e| EmailError::Transport(e.to_string()))?;
        Ok(())
    }
}

/// Plain text, because the action links have to be clickable in every client and an
/// HTML mail is one more thing to render wrong on a phone.
fn render_body(note: &Notification) -> String {
    let mut body = note.body.clone();
    if !note.actions.is_empty() {
        body.push_str("\n\n");
        for action in &note.actions {
            body.push_str(&format!("{}: {}\n", action.label, action.url));
        }
    }
    if let Some(url) = &note.open_url {
        body.push_str(&format!("\nOpen the garden: {url}\n"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Notification, NotificationAction};

    fn note() -> Notification {
        Notification {
            title: "add water — Kitchen".into(),
            body: "tank at 22%, using 0.50 L/day".into(),
            priority: 4,
            tags: vec![],
            actions: vec![NotificationAction {
                label: "Done".into(),
                url: "https://brain/a/abc".into(),
            }],
            open_url: Some("https://brain/gardens/1".into()),
        }
    }

    #[test]
    fn the_body_carries_the_reasoning_and_the_links() {
        let body = render_body(&note());
        assert!(body.contains("22%"));
        assert!(body.contains("Done: https://brain/a/abc"));
        assert!(body.contains("Open the garden"));
    }

    #[test]
    fn a_notification_with_no_actions_has_no_dangling_section() {
        let mut n = note();
        n.actions.clear();
        n.open_url = None;
        assert_eq!(render_body(&n), "tank at 22%, using 0.50 L/day");
    }

    #[test]
    fn links_are_bare_urls_so_every_client_can_click_them() {
        // No HTML, no markdown — a phone mail client should linkify these itself.
        let body = render_body(&note());
        assert!(!body.contains("<a href"));
        assert!(body.contains("https://"));
    }

    #[test]
    fn an_unauthenticated_relay_is_allowed() {
        // The mailcow-on-the-same-Podman-network case.
        let channel = EmailChannel::new(EmailConfig {
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            from: "garden@example.com".into(),
            starttls: false,
        });
        assert!(channel.is_ok());
    }

    #[tokio::test]
    async fn a_malformed_recipient_is_rejected_before_any_connection() {
        let channel = EmailChannel::new(EmailConfig {
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            from: "garden@example.com".into(),
            starttls: false,
        })
        .unwrap();

        let result = channel.send("not-an-address", &note()).await;
        assert!(matches!(result, Err(EmailError::Address(_))));
    }
}
