//! Getting a task off the dashboard and onto your phone.
//!
//! Three channels, all self-hosted:
//!
//! - [`ntfy`] — push, via your own ntfy container. The reliable one, and the only one
//!   that carries Done / Snooze buttons.
//! - [`email`] — best effort. Outbound SMTP from a residential IP is widely rejected
//!   on reputation, so this is a nice-to-have rather than the backbone.
//! - [`calendar`] — an iCal feed for the predictable cadence work.
//!
//! [`policy`] decides *whether* to send. That separation is deliberate: the decision
//! is pure logic and heavily tested, and the channels are thin I/O around it.
//!
//! See NOTIFICATIONS.md for the container and phone setup.

pub mod calendar;
pub mod email;
pub mod message;
pub mod ntfy;
pub mod policy;

pub use calendar::{CalendarTask, render as render_calendar};
pub use email::{EmailChannel, EmailConfig, EmailError};
pub use message::{Notification, NotificationAction, compose, compose_brief};
pub use ntfy::{NtfyChannel, NtfyConfig, NtfyError};
pub use policy::{Decision, HoldReason, LastNotified, QuietHours, Reach, decide, reach_for};

/// Everything the dispatcher needs to deliver on all channels.
///
/// Both channels are optional and independently so: a deployment with push but no
/// working mail server is the expected case, not a degraded one.
pub struct Notifier {
    pub ntfy: Option<NtfyChannel>,
    pub email: Option<EmailChannel>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Delivered {
    pub push: bool,
    pub email: bool,
}

impl Delivered {
    pub fn any(self) -> bool {
        self.push || self.email
    }
}

impl Notifier {
    pub fn new(ntfy: Option<NtfyChannel>, email: Option<EmailChannel>) -> Self {
        Self { ntfy, email }
    }

    pub fn is_configured(&self) -> bool {
        self.ntfy.is_some() || self.email.is_some()
    }

    /// Deliver on every channel the reach calls for and the recipient has set up.
    ///
    /// A failure on one channel never stops the other. Push working while mail is
    /// misconfigured is the normal state of a self-hosted deployment, and it should
    /// not cost you the notification.
    pub async fn deliver(
        &self,
        note: &Notification,
        reach: Reach,
        topic: Option<&str>,
        address: Option<&str>,
    ) -> Delivered {
        let mut delivered = Delivered::default();

        if reach.push
            && let (Some(channel), Some(topic)) = (&self.ntfy, topic)
        {
            match channel.send(topic, note).await {
                Ok(()) => delivered.push = true,
                Err(e) => tracing_warn(&format!("ntfy delivery failed: {e}")),
            }
        }

        if reach.email
            && let (Some(channel), Some(address)) = (&self.email, address)
        {
            match channel.send(address, note).await {
                Ok(()) => delivered.email = true,
                Err(e) => tracing_warn(&format!("email delivery failed: {e}")),
            }
        }

        delivered
    }
}

/// Logged rather than propagated: a channel that is down is an operational fact, not
/// a reason to abandon the dispatch loop for every other recipient.
fn tracing_warn(message: &str) {
    eprintln!("garden-notify: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_core::{Severity, TaskKind};

    #[tokio::test]
    async fn an_unconfigured_notifier_delivers_nothing_and_says_so() {
        let notifier = Notifier::new(None, None);
        assert!(!notifier.is_configured());

        let note = compose(
            TaskKind::AddWater,
            "garden",
            "Kitchen",
            "tank low",
            None,
            Severity::Critical,
            5,
            None,
            Vec::new(),
        );
        let delivered = notifier
            .deliver(&note, reach_for(Severity::Critical), Some("t"), Some("a@b.com"))
            .await;
        assert!(!delivered.any());
    }

    #[tokio::test]
    async fn a_recipient_with_no_topic_is_skipped_rather_than_erroring() {
        // Someone who has not set up the phone app yet still gets email, and the
        // dispatch loop keeps going for everyone else.
        let notifier = Notifier::new(
            Some(NtfyChannel::new(NtfyConfig {
                base_url: "http://127.0.0.1:1".into(),
                token: None,
            })
            .unwrap()),
            None,
        );
        let note = compose(
            TaskKind::AddWater,
            "garden",
            "Kitchen",
            "tank low",
            None,
            Severity::Critical,
            5,
            None,
            Vec::new(),
        );
        let delivered = notifier
            .deliver(&note, reach_for(Severity::Critical), None, None)
            .await;
        assert!(!delivered.push);
    }

    #[test]
    fn the_quiet_severities_never_reach_a_channel() {
        assert!(!reach_for(Severity::Info).push);
        assert!(!reach_for(Severity::Advisory).push);
        assert!(reach_for(Severity::Important).push);
    }
}
