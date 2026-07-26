//! Garden identity and metadata.
//!
//! Until now the system assumed a single device. Once one account can hold several
//! gardens and share them with other accounts, every piece of state has to be
//! attributable to a specific garden — otherwise a sharing bug leaks one person's
//! device into another person's dashboard.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Opaque garden identifier.
///
/// A UUID rather than a sequential integer specifically because these appear in URLs
/// under a sharing model. Sequential ids invite enumeration, and make an
/// authorization bug trivially exploitable instead of merely present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GardenId(pub Uuid);

impl GardenId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for GardenId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GardenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for GardenId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceModel {
    Studio2,
    Studio1,
    Home4,
    Home3,
    /// A simulated garden. Useful for trying the system out before touching hardware,
    /// and for demonstrating sharing without exposing a real device.
    Simulated,
}

impl DeviceModel {
    pub fn label(self) -> &'static str {
        match self {
            DeviceModel::Studio2 => "Gardyn Studio 2",
            DeviceModel::Studio1 => "Gardyn Studio",
            DeviceModel::Home4 => "Gardyn Home 4",
            DeviceModel::Home3 => "Gardyn Home 3",
            DeviceModel::Simulated => "Simulated",
        }
    }

    pub fn slot_count(self) -> u8 {
        match self {
            DeviceModel::Studio2 | DeviceModel::Studio1 | DeviceModel::Simulated => 16,
            DeviceModel::Home4 | DeviceModel::Home3 => 30,
        }
    }
}

impl fmt::Display for DeviceModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A garden as the operator thinks of it: a named device in a place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Garden {
    pub id: GardenId,
    /// Operator-chosen name. People with two gardens call them things like
    /// "kitchen" and "office", not by serial number.
    pub name: String,
    pub model: DeviceModel,
    /// IANA timezone. Quiet hours and the daily brief are meaningless without it, and
    /// two gardens on one account can legitimately be in different zones.
    pub timezone: String,
    pub created_at: jiff::Timestamp,
}

impl Garden {
    pub fn new(name: impl Into<String>, model: DeviceModel, created_at: jiff::Timestamp) -> Self {
        Self {
            id: GardenId::new(),
            name: name.into(),
            model,
            timezone: "UTC".to_string(),
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip_through_urls() {
        let a = GardenId::new();
        let b = GardenId::new();
        assert_ne!(a, b);

        let parsed: GardenId = a.to_string().parse().unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn a_malformed_id_is_rejected_rather_than_coerced() {
        assert!("not-a-uuid".parse::<GardenId>().is_err());
        assert!("1".parse::<GardenId>().is_err());
    }

    #[test]
    fn models_know_their_slot_counts() {
        assert_eq!(DeviceModel::Studio2.slot_count(), 16);
        assert_eq!(DeviceModel::Home4.slot_count(), 30);
    }
}
