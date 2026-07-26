//! Accounts, roles, sharing, and sessions.
//!
//! Multi-tenancy is the point: one account holds several gardens, and any garden can
//! be shared with other accounts at a chosen [`Role`]. Everything here is pure logic
//! over types with no I/O, so the whole authorization policy is exhaustively testable
//! — which is the only way to be confident that a sharing bug is not quietly exposing
//! one person's garden to another.
//!
//! Two properties are worth stating up front, because the rest of the system leans on
//! them:
//!
//! - **[`Actor`] is the single authorization decision point.** Handlers never compare
//!   roles themselves; they call [`Actor::require`]. One place to audit.
//! - **Secrets are stored as digests.** Session cookies, invite links, and
//!   notification action links all go through [`SecretToken`], so a leaked backup
//!   yields nothing usable.

pub mod action;
pub mod actor;
pub mod credential;
pub mod invite;
pub mod membership;
pub mod role;
pub mod session;
pub mod token;
pub mod user;

pub use action::{ActionGrant, ActionGrantId, GrantError, TaskAction};
pub use actor::{AccessDenied, Actor};
pub use credential::{
    PasswordDigest, PasswordError, WeakPassword, accept_new_password, check_password_policy,
    hash_password, verify_password,
};
pub use invite::{InviteError, InviteStatus, Invitation, InvitationId};
pub use membership::Membership;
pub use role::{Permission, Role};
pub use session::{SESSION_COOKIE, Session, SessionId};
pub use token::{SecretToken, TokenDigest};
pub use user::{EmailAddress, InvalidEmail, User, UserId};

#[cfg(test)]
mod integration {
    //! End-to-end checks across the pieces, in the order a real sharing flow happens.

    use super::*;
    use gardyn_core::GardenId;

    fn t0() -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn user(name: &str) -> User {
        User::new(
            EmailAddress::parse(&format!("{name}@example.com")).unwrap(),
            name,
            t0(),
        )
    }

    #[test]
    fn the_whole_sharing_flow() {
        // Phil creates a garden and owns it.
        let phil = user("phil");
        let garden = GardenId::new();
        let phil_actor = Actor::new(
            phil.clone(),
            [Membership::founding_owner(garden, phil.id, t0())],
        );
        assert_eq!(phil_actor.role_in(garden), Some(Role::Owner));

        // Sam cannot see it.
        let sam = user("sam");
        assert!(!Actor::new(sam.clone(), []).can(garden, Permission::ViewGarden));

        // Phil invites Sam as a caretaker.
        let granter = phil_actor.role_in(garden).unwrap();
        let (mut invitation, link_token) = Invitation::issue(
            garden,
            sam.email.clone(),
            Role::Caretaker,
            phil.id,
            granter,
            t0(),
        )
        .unwrap();

        // The link works only for Sam.
        assert_eq!(
            invitation.accept(&user("mallory"), t0()),
            Err(InviteError::WrongRecipient)
        );
        let role = invitation.accept(&sam, t0()).unwrap();
        assert_eq!(role, Role::Caretaker);

        // Accepting creates the membership, and now Sam can work the garden.
        let sam_actor = Actor::new(
            sam.clone(),
            [Membership::granted(garden, sam.id, role, phil.id, t0())],
        );
        assert!(sam_actor.can(garden, Permission::CompleteTask));
        assert!(sam_actor.can(garden, Permission::ViewGarden));

        // But not run the place.
        assert!(!sam_actor.can(garden, Permission::ManageMembers));
        assert!(!sam_actor.can(garden, Permission::DeleteGarden));
        assert!(!sam_actor.can(garden, Permission::ControlHardware));

        // The stored invitation never contained the link secret.
        assert_ne!(invitation.digest.as_str(), link_token.expose());

        // Phil can revoke Sam; Sam cannot revoke Phil.
        assert!(phil_actor.can_manage_member(garden, sam.id, Role::Caretaker));
        assert!(!sam_actor.can_manage_member(garden, phil.id, Role::Owner));
    }

    #[test]
    fn a_caretaker_can_act_on_a_notification_link_but_a_viewer_cannot() {
        let garden = GardenId::new();
        let sam = user("sam");
        let action = TaskAction::Complete;

        let caretaker = Actor::new(
            sam.clone(),
            [Membership::granted(
                garden,
                sam.id,
                Role::Caretaker,
                UserId::new(),
                t0(),
            )],
        );
        assert!(caretaker.can(garden, action.required_permission()));

        let viewer = Actor::new(
            sam.clone(),
            [Membership::granted(
                garden,
                sam.id,
                Role::Viewer,
                UserId::new(),
                t0(),
            )],
        );
        // Holding the link is not enough; the role still has to allow it.
        assert!(!viewer.can(garden, action.required_permission()));
    }

    #[test]
    fn losing_access_invalidates_outstanding_notification_links() {
        // A grant must be checked against live membership at redemption time, so that
        // removing someone stops their pending links working without hunting them down.
        let garden = GardenId::new();
        let sam = user("sam");
        let (grant, _) = ActionGrant::issue(
            sam.id,
            garden,
            gardyn_core::TaskKey::new(gardyn_core::TaskKind::AddWater, gardyn_core::Target::Garden),
            TaskAction::Complete,
            t0(),
        );

        let removed = Actor::new(sam, []); // membership deleted
        assert!(grant.is_usable(t0()), "the grant itself is still fresh");
        assert!(
            !removed.can(garden, grant.action.required_permission()),
            "but authorization must be re-checked, not assumed"
        );
    }

    #[test]
    fn one_account_holds_several_gardens_shared_from_different_people() {
        let phil = user("phil");
        let own = GardenId::new();
        let from_sam = GardenId::new();
        let from_kim = GardenId::new();

        let actor = Actor::new(
            phil.clone(),
            [
                Membership::founding_owner(own, phil.id, t0()),
                Membership::granted(from_sam, phil.id, Role::Steward, UserId::new(), t0()),
                Membership::granted(from_kim, phil.id, Role::Viewer, UserId::new(), t0()),
            ],
        );

        assert_eq!(actor.garden_count(), 3);
        assert!(actor.can(own, Permission::DeleteGarden));
        assert!(actor.can(from_sam, Permission::ManageMembers));
        assert!(!actor.can(from_sam, Permission::DeleteGarden));
        assert!(!actor.can(from_kim, Permission::CompleteTask));
    }

    #[test]
    fn registering_and_signing_in() {
        let digest = accept_new_password("a sufficiently long password").unwrap();
        assert!(verify_password("a sufficiently long password", &digest));
        assert!(!verify_password("a sufficiently long passwore", &digest));

        let (session, cookie) = Session::issue(UserId::new(), t0(), None);
        assert!(session.is_valid(t0()));
        // What the browser holds and what the database holds are different values.
        assert_eq!(
            SecretToken::from_client(cookie.expose()).unwrap().digest(),
            session.digest
        );
    }
}
