//! Sharing a garden with someone else.
//!
//! An invitation names an address and a role. Accepting it creates a
//! [`Membership`](crate::membership::Membership) — there is no other way for a
//! garden to become shared.
//!
//! Because self-hosted outbound email is unreliable, the UI also surfaces the raw
//! invite link for the inviter to send however they like. That makes link handling
//! the security boundary rather than mailbox possession, which is why acceptance
//! checks the recipient rather than trusting whoever holds the URL.

use crate::role::Role;
use crate::token::{SecretToken, TokenDigest};
use crate::user::{EmailAddress, User, UserId};
use gardyn_core::GardenId;
use gardyn_core::time::add_days;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_INVITE_LIFETIME_DAYS: f64 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvitationId(pub Uuid);

impl InvitationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InvitationId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: InvitationId,
    pub garden: GardenId,
    /// Who it is for. Acceptance is checked against this.
    pub email: EmailAddress,
    pub role: Role,
    pub invited_by: UserId,
    pub digest: TokenDigest,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub accepted_at: Option<Timestamp>,
    pub accepted_by: Option<UserId>,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    #[error("that invitation has expired")]
    Expired,
    #[error("that invitation has already been used")]
    AlreadyAccepted,
    #[error("that invitation was withdrawn")]
    Revoked,
    #[error("that invitation was sent to a different address")]
    WrongRecipient,
    #[error("the inviter cannot grant that role")]
    RoleNotGrantable,
}

impl Invitation {
    /// Create an invitation. The returned token belongs in the link and is not stored.
    ///
    /// `granter` is the inviter's role, checked here so that a privilege-escalation
    /// attempt fails at creation rather than at acceptance.
    pub fn issue(
        garden: GardenId,
        email: EmailAddress,
        role: Role,
        invited_by: UserId,
        granter: Role,
        now: Timestamp,
    ) -> Result<(Self, SecretToken), InviteError> {
        if !granter.can_grant(role) {
            return Err(InviteError::RoleNotGrantable);
        }
        let token = SecretToken::generate();
        let invitation = Self {
            id: InvitationId::new(),
            garden,
            email,
            role,
            invited_by,
            digest: token.digest(),
            created_at: now,
            expires_at: add_days(now, DEFAULT_INVITE_LIFETIME_DAYS),
            accepted_at: None,
            accepted_by: None,
            revoked_at: None,
        };
        Ok((invitation, token))
    }

    pub fn status(&self, now: Timestamp) -> InviteStatus {
        if self.revoked_at.is_some() {
            InviteStatus::Revoked
        } else if self.accepted_at.is_some() {
            InviteStatus::Accepted
        } else if now >= self.expires_at {
            InviteStatus::Expired
        } else {
            InviteStatus::Pending
        }
    }

    pub fn is_pending(&self, now: Timestamp) -> bool {
        self.status(now) == InviteStatus::Pending
    }

    /// Redeem the invitation for a specific user.
    ///
    /// The recipient check is the point of the whole type. Without it, a forwarded
    /// link — or one pasted into a group chat — would grant access to whoever opened
    /// it first, which is not what "share with Sam" means.
    pub fn accept(&mut self, user: &User, now: Timestamp) -> Result<Role, InviteError> {
        match self.status(now) {
            InviteStatus::Revoked => return Err(InviteError::Revoked),
            InviteStatus::Accepted => return Err(InviteError::AlreadyAccepted),
            InviteStatus::Expired => return Err(InviteError::Expired),
            InviteStatus::Pending => {}
        }
        if user.email != self.email {
            return Err(InviteError::WrongRecipient);
        }

        self.accepted_at = Some(now);
        self.accepted_by = Some(user.id);
        Ok(self.role)
    }

    pub fn revoke(&mut self, now: Timestamp) {
        if self.accepted_at.is_none() {
            self.revoked_at = Some(now);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteStatus {
    Pending,
    Accepted,
    Expired,
    Revoked,
}

impl InviteStatus {
    pub fn label(self) -> &'static str {
        match self {
            InviteStatus::Pending => "pending",
            InviteStatus::Accepted => "accepted",
            InviteStatus::Expired => "expired",
            InviteStatus::Revoked => "withdrawn",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gardyn_core::time::add_days;

    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn email(s: &str) -> EmailAddress {
        EmailAddress::parse(s).unwrap()
    }

    fn user(addr: &str) -> User {
        User::new(email(addr), "Someone", t0())
    }

    fn pending() -> (Invitation, SecretToken) {
        Invitation::issue(
            GardenId::new(),
            email("sam@example.com"),
            Role::Caretaker,
            UserId::new(),
            Role::Owner,
            t0(),
        )
        .unwrap()
    }

    #[test]
    fn the_intended_recipient_can_accept() {
        let (mut invite, _) = pending();
        let role = invite.accept(&user("sam@example.com"), t0()).unwrap();
        assert_eq!(role, Role::Caretaker);
        assert_eq!(invite.status(t0()), InviteStatus::Accepted);
    }

    #[test]
    fn a_forwarded_link_does_not_work_for_someone_else() {
        // The link is the security boundary; possession alone must not be enough.
        let (mut invite, _) = pending();
        assert_eq!(
            invite.accept(&user("mallory@example.com"), t0()),
            Err(InviteError::WrongRecipient)
        );
        assert!(invite.is_pending(t0()));
    }

    #[test]
    fn recipient_matching_ignores_address_casing() {
        let (mut invite, _) = pending();
        assert!(invite.accept(&user("SAM@Example.com"), t0()).is_ok());
    }

    #[test]
    fn an_invitation_cannot_be_used_twice() {
        let (mut invite, _) = pending();
        invite.accept(&user("sam@example.com"), t0()).unwrap();
        assert_eq!(
            invite.accept(&user("sam@example.com"), t0()),
            Err(InviteError::AlreadyAccepted)
        );
    }

    #[test]
    fn invitations_expire() {
        let (mut invite, _) = pending();
        let late = add_days(t0(), DEFAULT_INVITE_LIFETIME_DAYS + 1.0);
        assert_eq!(invite.status(late), InviteStatus::Expired);
        assert_eq!(
            invite.accept(&user("sam@example.com"), late),
            Err(InviteError::Expired)
        );
    }

    #[test]
    fn a_withdrawn_invitation_stops_working_immediately() {
        let (mut invite, _) = pending();
        invite.revoke(t0());
        assert_eq!(
            invite.accept(&user("sam@example.com"), t0()),
            Err(InviteError::Revoked)
        );
    }

    #[test]
    fn withdrawing_an_accepted_invitation_does_not_retroactively_undo_it() {
        // Revoking access means removing the membership, not rewriting history.
        let (mut invite, _) = pending();
        invite.accept(&user("sam@example.com"), t0()).unwrap();
        invite.revoke(t0());
        assert_eq!(invite.status(t0()), InviteStatus::Accepted);
    }

    #[test]
    fn a_steward_cannot_invite_a_steward() {
        assert_eq!(
            Invitation::issue(
                GardenId::new(),
                email("sam@example.com"),
                Role::Steward,
                UserId::new(),
                Role::Steward,
                t0(),
            )
            .err(),
            Some(InviteError::RoleNotGrantable)
        );
    }

    #[test]
    fn nobody_can_invite_an_owner() {
        for granter in Role::ALL {
            assert_eq!(
                Invitation::issue(
                    GardenId::new(),
                    email("sam@example.com"),
                    Role::Owner,
                    UserId::new(),
                    *granter,
                    t0(),
                )
                .err(),
                Some(InviteError::RoleNotGrantable),
                "{granter} could invite an owner"
            );
        }
    }

    #[test]
    fn a_caretaker_cannot_invite_at_all() {
        for role in Role::ALL {
            assert!(
                Invitation::issue(
                    GardenId::new(),
                    email("sam@example.com"),
                    *role,
                    UserId::new(),
                    Role::Caretaker,
                    t0(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn the_invite_token_is_not_stored_in_the_invitation() {
        let (invite, token) = pending();
        assert_ne!(invite.digest.as_str(), token.expose());
        assert_eq!(invite.digest, token.digest());
    }
}
