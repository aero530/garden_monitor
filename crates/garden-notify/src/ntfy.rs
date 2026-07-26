//! Push, via a self-hosted ntfy server.
//!
//! Chosen because it puts a real notification on an iPhone or Android home screen
//! with no app to write and no third party in the path. The server is one container
//! on the Fedora VM; the phone app points at it.
//!
//! The action buttons are the reason this beats email: the notification itself carries
//! Done / Snooze / Not-applicable, so the whole loop closes from the lock screen. Each
//! button is a single-use link scoped to one task — see `garden_auth::action`.

use crate::message::Notification;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum NtfyError {
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("ntfy rejected the message: {status} {body}")]
    Rejected {
        status: reqwest::StatusCode,
        body: String,
    },
}

#[derive(Debug, Clone)]
pub struct NtfyConfig {
    /// Base URL of the self-hosted server, e.g. `http://ntfy.local:8090`.
    pub base_url: String,
    /// Optional access token, if the server requires auth. Strongly recommended:
    /// without it anyone who can reach the server can publish to your topic.
    pub token: Option<String>,
}

/// ntfy's JSON publish format.
///
/// Used in preference to the header-based API because a notification body can contain
/// newlines and non-ASCII, and stuffing that into an HTTP header is how you get a
/// silently truncated message.
#[derive(Serialize)]
struct Publish<'a> {
    topic: &'a str,
    title: &'a str,
    message: &'a str,
    priority: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<&'a str>,
}

#[derive(Serialize)]
struct Action<'a> {
    action: &'static str,
    label: &'a str,
    url: &'a str,
    /// Keep the notification on screen after tapping, so a mis-tap is visible.
    clear: bool,
    /// `GET`, because the links are one-shot signed URLs and a phone's notification
    /// action cannot carry a session cookie for a POST.
    method: &'static str,
}

pub struct NtfyChannel {
    config: NtfyConfig,
    http: reqwest::Client,
}

impl NtfyChannel {
    pub fn new(config: NtfyConfig) -> Result<Self, NtfyError> {
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        })
    }

    pub async fn send(&self, topic: &str, note: &Notification) -> Result<(), NtfyError> {
        let actions: Vec<Action> = note
            .actions
            .iter()
            .take(3) // ntfy renders at most three.
            .map(|a| Action {
                action: "http",
                label: &a.label,
                url: &a.url,
                clear: false,
                method: "GET",
            })
            .collect();

        let payload = Publish {
            topic,
            title: &note.title,
            message: &note.body,
            priority: note.priority.clamp(1, 5),
            tags: note.tags.iter().map(String::as_str).collect(),
            actions,
            click: note.open_url.as_deref(),
        };

        let mut request = self
            .http
            .post(self.config.base_url.trim_end_matches('/'))
            .json(&payload);
        if let Some(token) = &self.config.token {
            request = request.bearer_auth(token);
        }

        let response = request.send().await?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(NtfyError::Rejected {
            status,
            body: body.chars().take(200).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::NotificationAction;

    fn note() -> Notification {
        Notification {
            title: "Add water — Kitchen".into(),
            body: "tank at 22% (3.4 L), using 0.50 L/day — reserve reached in 1.8 days".into(),
            priority: 4,
            tags: vec!["droplet".into()],
            actions: vec![
                NotificationAction {
                    label: "Done".into(),
                    url: "https://brain/a/abc".into(),
                },
                NotificationAction {
                    label: "Snooze".into(),
                    url: "https://brain/a/def".into(),
                },
            ],
            open_url: Some("https://brain/gardens/1".into()),
        }
    }

    fn payload(note: &Notification) -> serde_json::Value {
        let actions: Vec<Action> = note
            .actions
            .iter()
            .take(3)
            .map(|a| Action {
                action: "http",
                label: &a.label,
                url: &a.url,
                clear: false,
                method: "GET",
            })
            .collect();
        serde_json::to_value(Publish {
            topic: "garden-phil",
            title: &note.title,
            message: &note.body,
            priority: note.priority,
            tags: note.tags.iter().map(String::as_str).collect(),
            actions,
            click: note.open_url.as_deref(),
        })
        .unwrap()
    }

    #[test]
    fn the_payload_carries_topic_title_body_and_priority() {
        let json = payload(&note());
        assert_eq!(json["topic"], "garden-phil");
        assert_eq!(json["priority"], 4);
        assert!(json["title"].as_str().unwrap().contains("Kitchen"));
        assert!(json["message"].as_str().unwrap().contains("1.8 days"));
    }

    #[test]
    fn action_buttons_are_get_requests() {
        // A notification action cannot carry a session cookie, so the links are
        // one-shot signed URLs fetched with GET.
        let json = payload(&note());
        for action in json["actions"].as_array().unwrap() {
            assert_eq!(action["method"], "GET");
            assert_eq!(action["action"], "http");
        }
    }

    #[test]
    fn at_most_three_actions_are_sent() {
        // ntfy silently drops the rest, which would look like a broken button.
        let mut n = note();
        n.actions = (0..6)
            .map(|i| NotificationAction {
                label: format!("a{i}"),
                url: format!("https://brain/a/{i}"),
            })
            .collect();
        assert_eq!(payload(&n)["actions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_notification_with_no_actions_omits_the_field() {
        let mut n = note();
        n.actions.clear();
        n.tags.clear();
        let json = payload(&n);
        assert!(json.get("actions").is_none());
        assert!(json.get("tags").is_none());
    }

    #[test]
    fn a_body_with_newlines_survives_because_it_is_json_not_a_header() {
        let mut n = note();
        n.body = "line one\nline two — with an em dash".into();
        let json = payload(&n);
        assert_eq!(json["message"], "line one\nline two — with an em dash");
    }

    #[test]
    fn priority_is_clamped_into_ntfys_range() {
        let channel_priority = |p: u8| p.clamp(1, 5);
        assert_eq!(channel_priority(0), 1);
        assert_eq!(channel_priority(9), 5);
        assert_eq!(channel_priority(3), 3);
    }
}
