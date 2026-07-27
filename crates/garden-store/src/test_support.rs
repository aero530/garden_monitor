//! Fixtures shared by this crate's unit tests.
//!
//! Every module here needs the same three things — a fixed instant, a store with one
//! user and one garden in it, and a frame row to hang measurements off. They were
//! being written out per module; this is the one copy.

use crate::Store;
use crate::frames::{FrameSource, NewFrame};
use garden_auth::{EmailAddress, UserId};
use garden_core::{DeviceModel, GardenId};
use jiff::Timestamp;
use uuid::Uuid;

/// A fixed instant. Wall-clock time in a test is a flake waiting for a slow machine.
pub fn t0() -> Timestamp {
    Timestamp::from_second(1_700_000_000).unwrap()
}

/// A store with one account and one Studio 2.
pub async fn fixture() -> (Store, GardenId) {
    let (store, garden, _) = fixture_with_user().await;
    (store, garden)
}

/// The same, when the test also needs the account.
pub async fn fixture_with_user() -> (Store, GardenId, UserId) {
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(
            EmailAddress::parse("phil@example.com").unwrap(),
            "Phil",
            "a long enough password",
            t0(),
        )
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    (store, garden.id, user.id)
}

/// The smallest valid PNG: 1×1, opaque black.
///
/// Real bytes rather than a stub, because `put_frame` sniffs the content type and
/// would reject anything that is not actually an image.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
    0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
    0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Store one frame, captured at `at`.
pub async fn frame_at(store: &Store, garden: GardenId, at: Timestamp) -> Uuid {
    store
        .put_frame(NewFrame {
            garden,
            captured_at: at,
            width: 1,
            height: 1,
            light_duty_milli: Some(800),
            comparable: true,
            source: FrameSource::Agent,
            bytes: TINY_PNG,
        })
        .await
        .unwrap()
        .unwrap()
        .id
}
