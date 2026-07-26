//! Roles and permissions.
//!
//! This is the security core of sharing. Everything here is a total function over
//! enums with no I/O, so the entire policy is exhaustively testable — which matters,
//! because an authorization bug here means one person's garden showing up on another
//! person's dashboard.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// What a member may do with a garden.
///
/// Ordered from least to most privileged so that comparisons express intent:
/// `role >= Role::Caretaker` reads as "at least a caretaker".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Sees everything, changes nothing.
    Viewer,
    /// Does the physical work: completes tasks, logs what they did, manages plantings.
    /// The housemate who waters it while you are away.
    Caretaker,
    /// Everything operational, plus configuration and inviting other members.
    Steward,
    /// Full control, including deletion and handing the garden to someone else.
    Owner,
}

impl Role {
    pub const ALL: &'static [Role] = &[Role::Viewer, Role::Caretaker, Role::Steward, Role::Owner];

    pub fn label(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Caretaker => "caretaker",
            Role::Steward => "steward",
            Role::Owner => "owner",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Role::Viewer => "Can see the garden and its history, but change nothing.",
            Role::Caretaker => "Can complete tasks, log actions, and manage plantings.",
            Role::Steward => "Can also configure the garden and invite other people.",
            Role::Owner => "Full control, including deleting or transferring the garden.",
        }
    }

    pub fn grants(self, permission: Permission) -> bool {
        self >= permission.minimum_role()
    }

    /// Whether a member holding this role may grant `target` to someone else.
    ///
    /// Strictly *below* their own role, deliberately. If a steward could grant
    /// steward, any shared garden would be one invitation away from an unbounded
    /// privilege chain that the owner never approved. Ownership moves only through an
    /// explicit transfer, never through an invitation.
    pub fn can_grant(self, target: Role) -> bool {
        self.grants(Permission::ManageMembers) && target < self && target != Role::Owner
    }

    /// Whether a member holding this role may modify or revoke a member holding
    /// `target`. Peers cannot remove each other; only someone strictly senior can.
    pub fn can_manage_member(self, target: Role) -> bool {
        self.grants(Permission::ManageMembers) && target < self
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "viewer" => Ok(Role::Viewer),
            "caretaker" => Ok(Role::Caretaker),
            "steward" => Ok(Role::Steward),
            "owner" => Ok(Role::Owner),
            other => Err(UnknownRole(other.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown role: {0}")]
pub struct UnknownRole(pub String);

/// A specific capability within a garden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ViewGarden,
    /// Mark a task done, snooze it, or dismiss it.
    CompleteTask,
    /// Record a top-off, dose, harvest, or prune.
    LogEvent,
    /// Add, remove, or reassign what is growing in a slot.
    ManagePlantings,
    /// Change thresholds, capabilities, notification settings.
    ConfigureGarden,
    /// Drive lights and pump directly. Only meaningful after firmware takeover, and
    /// separated from `ConfigureGarden` because it can physically harm the plants.
    ControlHardware,
    /// Invite, re-role, and remove other members.
    ManageMembers,
    TransferOwnership,
    DeleteGarden,
}

impl Permission {
    pub fn minimum_role(self) -> Role {
        match self {
            Permission::ViewGarden => Role::Viewer,
            Permission::CompleteTask | Permission::LogEvent | Permission::ManagePlantings => {
                Role::Caretaker
            }
            Permission::ConfigureGarden
            | Permission::ControlHardware
            | Permission::ManageMembers => Role::Steward,
            Permission::TransferOwnership | Permission::DeleteGarden => Role::Owner,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Permission::ViewGarden => "view the garden",
            Permission::CompleteTask => "complete tasks",
            Permission::LogEvent => "log actions",
            Permission::ManagePlantings => "manage plantings",
            Permission::ConfigureGarden => "configure the garden",
            Permission::ControlHardware => "control hardware",
            Permission::ManageMembers => "manage members",
            Permission::TransferOwnership => "transfer ownership",
            Permission::DeleteGarden => "delete the garden",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY_PERMISSION: &[Permission] = &[
        Permission::ViewGarden,
        Permission::CompleteTask,
        Permission::LogEvent,
        Permission::ManagePlantings,
        Permission::ConfigureGarden,
        Permission::ControlHardware,
        Permission::ManageMembers,
        Permission::TransferOwnership,
        Permission::DeleteGarden,
    ];

    #[test]
    fn everyone_can_look() {
        for role in Role::ALL {
            assert!(role.grants(Permission::ViewGarden), "{role} cannot view");
        }
    }

    #[test]
    fn viewers_can_change_nothing() {
        for permission in EVERY_PERMISSION {
            if *permission == Permission::ViewGarden {
                continue;
            }
            assert!(
                !Role::Viewer.grants(*permission),
                "viewer should not be able to {permission}"
            );
        }
    }

    #[test]
    fn caretakers_do_the_work_but_do_not_run_the_place() {
        assert!(Role::Caretaker.grants(Permission::CompleteTask));
        assert!(Role::Caretaker.grants(Permission::LogEvent));
        assert!(Role::Caretaker.grants(Permission::ManagePlantings));

        assert!(!Role::Caretaker.grants(Permission::ConfigureGarden));
        assert!(!Role::Caretaker.grants(Permission::ManageMembers));
        assert!(!Role::Caretaker.grants(Permission::ControlHardware));
    }

    #[test]
    fn only_the_owner_can_delete_or_transfer() {
        for role in Role::ALL {
            let expected = *role == Role::Owner;
            assert_eq!(role.grants(Permission::DeleteGarden), expected);
            assert_eq!(role.grants(Permission::TransferOwnership), expected);
        }
    }

    #[test]
    fn hardware_control_is_held_back_from_caretakers() {
        // Driving the pump directly can kill plants; it is separated from ordinary
        // task completion on purpose.
        assert!(!Role::Caretaker.grants(Permission::ControlHardware));
        assert!(Role::Steward.grants(Permission::ControlHardware));
    }

    #[test]
    fn permissions_are_monotonic_in_role() {
        // A more senior role must never have fewer permissions than a junior one.
        for permission in EVERY_PERMISSION {
            let mut seen_granted = false;
            for role in Role::ALL {
                let granted = role.grants(*permission);
                if seen_granted {
                    assert!(
                        granted,
                        "{role} lacks {permission} that a junior role has"
                    );
                }
                seen_granted |= granted;
            }
        }
    }

    #[test]
    fn nobody_can_grant_their_own_role() {
        // Otherwise one shared garden is an unbounded privilege chain the owner
        // never approved.
        for role in Role::ALL {
            assert!(!role.can_grant(*role), "{role} could clone itself");
        }
    }

    #[test]
    fn stewards_can_grant_only_downward() {
        assert!(Role::Steward.can_grant(Role::Caretaker));
        assert!(Role::Steward.can_grant(Role::Viewer));
        assert!(!Role::Steward.can_grant(Role::Steward));
        assert!(!Role::Steward.can_grant(Role::Owner));
    }

    #[test]
    fn ownership_never_moves_by_invitation() {
        for role in Role::ALL {
            assert!(
                !role.can_grant(Role::Owner),
                "{role} could hand out ownership"
            );
        }
    }

    #[test]
    fn caretakers_and_viewers_cannot_invite_anyone() {
        for role in [Role::Viewer, Role::Caretaker] {
            for target in Role::ALL {
                assert!(!role.can_grant(*target), "{role} could invite {target}");
            }
        }
    }

    #[test]
    fn peers_cannot_remove_each_other() {
        assert!(!Role::Steward.can_manage_member(Role::Steward));
        assert!(Role::Owner.can_manage_member(Role::Steward));
        assert!(Role::Steward.can_manage_member(Role::Caretaker));
    }

    #[test]
    fn roles_round_trip_through_strings() {
        for role in Role::ALL {
            assert_eq!(role.label().parse::<Role>().unwrap(), *role);
        }
        assert!("superuser".parse::<Role>().is_err());
    }
}
