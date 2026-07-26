//! Membership: the link that makes a garden shared.

use crate::role::Role;
use crate::user::UserId;
use garden_core::GardenId;
use serde::{Deserialize, Serialize};

/// One person's access to one garden.
///
/// Sharing a garden is exactly "create a membership"; un-sharing is "delete it".
/// There is no separate concept, which keeps the authorization surface to a single
/// table and a single question: what role does this user hold in this garden?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Membership {
    pub garden: GardenId,
    pub user: UserId,
    pub role: Role,
    /// Who granted this. `None` for the founding owner, who granted it to themselves
    /// by creating the garden.
    pub granted_by: Option<UserId>,
    pub granted_at: jiff::Timestamp,
}

impl Membership {
    /// The membership created when someone adds a garden.
    pub fn founding_owner(garden: GardenId, user: UserId, at: jiff::Timestamp) -> Self {
        Self {
            garden,
            user,
            role: Role::Owner,
            granted_by: None,
            granted_at: at,
        }
    }

    pub fn granted(
        garden: GardenId,
        user: UserId,
        role: Role,
        by: UserId,
        at: jiff::Timestamp,
    ) -> Self {
        Self {
            garden,
            user,
            role,
            granted_by: Some(by),
            granted_at: at,
        }
    }

    pub fn is_founder(&self) -> bool {
        self.granted_by.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn creating_a_garden_makes_you_its_owner() {
        let m = Membership::founding_owner(GardenId::new(), UserId::new(), t0());
        assert_eq!(m.role, Role::Owner);
        assert!(m.is_founder());
    }

    #[test]
    fn shared_memberships_record_who_granted_them() {
        let granter = UserId::new();
        let m = Membership::granted(
            GardenId::new(),
            UserId::new(),
            Role::Caretaker,
            granter,
            t0(),
        );
        assert_eq!(m.granted_by, Some(granter));
        assert!(!m.is_founder());
    }
}
