//! The authenticated caller, and every authorization decision in the system.
//!
//! Handlers must not compare roles themselves. They ask an [`Actor`], which is the
//! single place that knows how membership, account status, and administrator status
//! combine. One decision point means one place to audit.

use crate::membership::Membership;
use crate::role::{Permission, Role};
use crate::user::{User, UserId};
use gardyn_core::GardenId;
use std::collections::BTreeMap;

/// Why an action was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessDenied {
    /// The caller has no membership in this garden — or it does not exist. The two
    /// are deliberately indistinguishable to the caller.
    #[error("no such garden")]
    NotAMember { garden: GardenId },

    #[error("your role ({held}) cannot {permission}; {required} is required")]
    InsufficientRole {
        garden: GardenId,
        held: Role,
        required: Role,
        permission: Permission,
    },

    #[error("this account has been disabled")]
    AccountDisabled,

    #[error("that is restricted to server administrators")]
    NotAdministrator,
}

impl AccessDenied {
    /// Whether the response must hide the resource's existence.
    ///
    /// Garden ids appear in URLs. Answering "403 Forbidden" for a garden the caller
    /// is not a member of would confirm that the id is real, turning a shared-link
    /// guess into an enumeration oracle. Those cases render as 404 instead.
    pub fn conceals_existence(&self) -> bool {
        matches!(self, AccessDenied::NotAMember { .. })
    }
}

/// An authenticated user together with everything they can reach.
#[derive(Debug, Clone)]
pub struct Actor {
    pub user: User,
    roles: BTreeMap<GardenId, Role>,
}

impl Actor {
    pub fn new(user: User, memberships: impl IntoIterator<Item = Membership>) -> Self {
        let roles = memberships
            .into_iter()
            .filter(|m| m.user == user.id)
            .map(|m| (m.garden, m.role))
            .collect();
        Self { user, roles }
    }

    pub fn id(&self) -> UserId {
        self.user.id
    }

    /// The caller's role in a garden, or `None` if they have no access at all.
    ///
    /// A disabled account holds no roles, regardless of its memberships.
    pub fn role_in(&self, garden: GardenId) -> Option<Role> {
        if !self.user.is_active() {
            return None;
        }
        self.roles.get(&garden).copied()
    }

    pub fn can(&self, garden: GardenId, permission: Permission) -> bool {
        self.role_in(garden).is_some_and(|r| r.grants(permission))
    }

    /// The gate every handler calls before acting.
    pub fn require(
        &self,
        garden: GardenId,
        permission: Permission,
    ) -> Result<Role, AccessDenied> {
        if !self.user.is_active() {
            return Err(AccessDenied::AccountDisabled);
        }
        let held = self
            .role_in(garden)
            .ok_or(AccessDenied::NotAMember { garden })?;

        if held.grants(permission) {
            Ok(held)
        } else {
            Err(AccessDenied::InsufficientRole {
                garden,
                held,
                required: permission.minimum_role(),
                permission,
            })
        }
    }

    /// Every garden the caller can see, with their role, ordered for stable rendering.
    pub fn gardens(&self) -> impl Iterator<Item = (GardenId, Role)> + '_ {
        let active = self.user.is_active();
        self.roles
            .iter()
            .filter(move |_| active)
            .map(|(g, r)| (*g, *r))
    }

    pub fn garden_count(&self) -> usize {
        if self.user.is_active() {
            self.roles.len()
        } else {
            0
        }
    }

    /// Server administration: fleet and system health.
    ///
    /// Note what this does **not** do. Being a server administrator grants no access
    /// to anyone's garden. An admin can see that a device is offline and that the
    /// broker is down; they cannot see what someone is growing, or act on it. That
    /// separation is the whole reason `require` never consults `is_admin`.
    pub fn require_admin(&self) -> Result<(), AccessDenied> {
        if !self.user.is_active() {
            return Err(AccessDenied::AccountDisabled);
        }
        if self.user.is_admin {
            Ok(())
        } else {
            Err(AccessDenied::NotAdministrator)
        }
    }

    pub fn is_admin(&self) -> bool {
        self.user.is_active() && self.user.is_admin
    }

    /// Whether the caller may hand `role` to someone else in this garden.
    pub fn can_grant(&self, garden: GardenId, role: Role) -> bool {
        self.role_in(garden).is_some_and(|r| r.can_grant(role))
    }

    /// Roles the caller is allowed to offer, for populating a share form.
    pub fn grantable_roles(&self, garden: GardenId) -> Vec<Role> {
        Role::ALL
            .iter()
            .copied()
            .filter(|r| self.can_grant(garden, *r))
            .collect()
    }

    /// Whether the caller may change or revoke a member currently holding `target`.
    /// Nobody may act on their own membership through this path — leaving a garden is
    /// a separate, always-permitted action.
    pub fn can_manage_member(&self, garden: GardenId, member: UserId, target: Role) -> bool {
        if member == self.id() {
            return false;
        }
        self.role_in(garden)
            .is_some_and(|r| r.can_manage_member(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::EmailAddress;

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

    fn actor_with(user: User, garden: GardenId, role: Role) -> Actor {
        let m = Membership::granted(garden, user.id, role, UserId::new(), t0());
        Actor::new(user, [m])
    }

    #[test]
    fn an_owner_can_do_everything_in_their_own_garden() {
        let g = GardenId::new();
        let actor = actor_with(user("phil"), g, Role::Owner);
        assert!(actor.require(g, Permission::DeleteGarden).is_ok());
        assert!(actor.require(g, Permission::ManageMembers).is_ok());
        assert!(actor.require(g, Permission::ViewGarden).is_ok());
    }

    #[test]
    fn a_stranger_gets_a_concealing_error_not_a_forbidden() {
        // The critical property: garden ids live in URLs, so "403" would confirm the
        // id is real and turn guessing into enumeration.
        let actor = Actor::new(user("mallory"), []);
        let err = actor
            .require(GardenId::new(), Permission::ViewGarden)
            .unwrap_err();
        assert!(matches!(err, AccessDenied::NotAMember { .. }));
        assert!(err.conceals_existence());
    }

    #[test]
    fn an_under_privileged_member_gets_an_explanatory_error() {
        let g = GardenId::new();
        let actor = actor_with(user("sam"), g, Role::Viewer);
        let err = actor.require(g, Permission::CompleteTask).unwrap_err();
        match err {
            AccessDenied::InsufficientRole { held, required, .. } => {
                assert_eq!(held, Role::Viewer);
                assert_eq!(required, Role::Caretaker);
            }
            other => panic!("expected an explanatory error, got {other:?}"),
        }
        // But it must not conceal — they already know the garden exists.
        assert!(!err.conceals_existence());
    }

    #[test]
    fn a_server_admin_cannot_read_other_peoples_gardens() {
        // Administration is about infrastructure, not about the contents of somebody
        // else's tower.
        let mut admin = user("root");
        admin.is_admin = true;
        let actor = Actor::new(admin, []);

        assert!(actor.require_admin().is_ok());
        let err = actor
            .require(GardenId::new(), Permission::ViewGarden)
            .unwrap_err();
        assert!(matches!(err, AccessDenied::NotAMember { .. }));
        assert_eq!(actor.garden_count(), 0);
    }

    #[test]
    fn a_non_admin_cannot_reach_the_fleet_view() {
        let g = GardenId::new();
        let actor = actor_with(user("phil"), g, Role::Owner);
        assert_eq!(actor.require_admin(), Err(AccessDenied::NotAdministrator));
        assert!(!actor.is_admin());
    }

    #[test]
    fn a_disabled_account_loses_everything_without_losing_its_memberships() {
        let g = GardenId::new();
        let mut u = user("phil");
        u.disabled_at = Some(t0());
        let actor = actor_with(u, g, Role::Owner);

        assert_eq!(actor.role_in(g), None);
        assert_eq!(actor.garden_count(), 0);
        assert_eq!(actor.gardens().count(), 0);
        assert_eq!(
            actor.require(g, Permission::ViewGarden),
            Err(AccessDenied::AccountDisabled)
        );
    }

    #[test]
    fn a_disabled_admin_cannot_administer() {
        let mut u = user("root");
        u.is_admin = true;
        u.disabled_at = Some(t0());
        let actor = Actor::new(u, []);
        assert_eq!(actor.require_admin(), Err(AccessDenied::AccountDisabled));
        assert!(!actor.is_admin());
    }

    #[test]
    fn one_account_can_hold_several_gardens_at_different_roles() {
        let mine = GardenId::new();
        let theirs = GardenId::new();
        let u = user("phil");
        let actor = Actor::new(
            u.clone(),
            [
                Membership::founding_owner(mine, u.id, t0()),
                Membership::granted(theirs, u.id, Role::Caretaker, UserId::new(), t0()),
            ],
        );

        assert_eq!(actor.garden_count(), 2);
        assert_eq!(actor.role_in(mine), Some(Role::Owner));
        assert_eq!(actor.role_in(theirs), Some(Role::Caretaker));

        // Full control of their own, hands-off on the shared one.
        assert!(actor.can(mine, Permission::DeleteGarden));
        assert!(!actor.can(theirs, Permission::DeleteGarden));
        assert!(actor.can(theirs, Permission::CompleteTask));
    }

    #[test]
    fn memberships_belonging_to_other_users_are_ignored() {
        // Guards against a query bug handing an actor somebody else's rows.
        let g = GardenId::new();
        let u = user("phil");
        let someone_else = Membership::founding_owner(g, UserId::new(), t0());
        let actor = Actor::new(u, [someone_else]);
        assert_eq!(actor.role_in(g), None);
    }

    #[test]
    fn the_share_form_offers_only_roles_the_caller_may_grant() {
        let g = GardenId::new();
        let owner = actor_with(user("phil"), g, Role::Owner);
        assert_eq!(
            owner.grantable_roles(g),
            vec![Role::Viewer, Role::Caretaker, Role::Steward]
        );

        let steward = actor_with(user("sam"), g, Role::Steward);
        assert_eq!(steward.grantable_roles(g), vec![Role::Viewer, Role::Caretaker]);

        let caretaker = actor_with(user("kim"), g, Role::Caretaker);
        assert!(caretaker.grantable_roles(g).is_empty());
    }

    #[test]
    fn nobody_can_edit_their_own_membership() {
        // Otherwise an owner could accidentally demote themselves out of their garden,
        // or a steward could promote themselves.
        let g = GardenId::new();
        let u = user("phil");
        let id = u.id;
        let actor = actor_with(u, g, Role::Owner);
        assert!(!actor.can_manage_member(g, id, Role::Owner));
    }

    #[test]
    fn a_steward_cannot_remove_the_owner() {
        let g = GardenId::new();
        let steward = actor_with(user("sam"), g, Role::Steward);
        assert!(!steward.can_manage_member(g, UserId::new(), Role::Owner));
        assert!(steward.can_manage_member(g, UserId::new(), Role::Caretaker));
    }
}
