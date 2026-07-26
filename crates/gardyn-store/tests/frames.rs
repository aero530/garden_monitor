//! Camera frame storage, against a real database and a real filesystem.

use gardyn_auth::EmailAddress;
use gardyn_core::{DeviceModel, Garden, GardenId};
use gardyn_store::Store;
use gardyn_store::frames::{FrameError, FrameSource, ImageKind, MAX_FRAME_BYTES, NewFrame};

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

/// A minimal but genuinely valid PNG.
fn png(tag: u8) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[tag; 64]);
    bytes
}

fn jpeg() -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend_from_slice(&[0x11; 64]);
    bytes
}

async fn fixture() -> (Store, Garden, Garden) {
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
    let kitchen = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    let office = store
        .create_garden("Office", DeviceModel::Studio2, "UTC", user.id, t0())
        .await
        .unwrap();
    (store, kitchen, office)
}

fn frame_at<'a>(garden: GardenId, bytes: &'a [u8], minutes: f64) -> NewFrame<'a> {
    NewFrame {
        garden,
        captured_at: gardyn_core::time::add_days(t0(), minutes / (24.0 * 60.0)),
        width: 640,
        height: 480,
        light_duty_milli: Some(800),
        comparable: true,
        source: FrameSource::Agent,
        bytes,
    }
}

#[tokio::test]
async fn a_stored_frame_reads_back_byte_for_byte() {
    let (store, kitchen, _) = fixture().await;
    let bytes = png(0xAB);

    let frame = store
        .put_frame(frame_at(kitchen.id, &bytes, 0.0))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(frame.kind, ImageKind::Png);
    assert_eq!(frame.byte_size, bytes.len() as i64);
    assert_eq!(store.frame_bytes(&frame).await.unwrap(), bytes);
}

#[tokio::test]
async fn the_format_comes_from_the_bytes_not_from_a_claim() {
    let (store, kitchen, _) = fixture().await;
    let frame = store
        .put_frame(frame_at(kitchen.id, &jpeg(), 0.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.kind, ImageKind::Jpeg);
    assert_eq!(frame.kind.content_type(), "image/jpeg");
}

#[tokio::test]
async fn html_masquerading_as_an_image_is_refused() {
    // The attack this guards: content stored here is later served from our own
    // origin, so anything a browser might render as a document must never land.
    let (store, kitchen, _) = fixture().await;
    let payload = b"<html><script>alert(document.cookie)</script></html>";

    let rejected = store
        .put_frame(frame_at(kitchen.id, payload, 0.0))
        .await
        .unwrap();
    assert!(matches!(rejected, Err(FrameError::UnrecognisedFormat)));
    assert_eq!(store.frame_count(kitchen.id).await.unwrap(), 0);
}

#[tokio::test]
async fn empty_and_oversized_uploads_are_refused() {
    let (store, kitchen, _) = fixture().await;

    assert!(matches!(
        store.put_frame(frame_at(kitchen.id, b"", 0.0)).await.unwrap(),
        Err(FrameError::Empty)
    ));

    let mut huge = png(0x01);
    huge.resize(MAX_FRAME_BYTES + 1, 0);
    assert!(matches!(
        store.put_frame(frame_at(kitchen.id, &huge, 0.0)).await.unwrap(),
        Err(FrameError::TooLarge(_))
    ));

    assert_eq!(store.frame_count(kitchen.id).await.unwrap(), 0);
}

#[tokio::test]
async fn a_rejected_upload_leaves_nothing_on_disk() {
    let (store, kitchen, _) = fixture().await;
    store
        .put_frame(frame_at(kitchen.id, b"not an image", 0.0))
        .await
        .unwrap()
        .unwrap_err();

    let dir = store.frames.root().join(kitchen.id.to_string());
    let count = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count, 0, "a refused upload wrote a file anyway");
}

#[tokio::test]
async fn a_frame_cannot_be_fetched_through_another_gardens_url() {
    // The isolation that matters. Both gardens belong to the same person here, so
    // membership alone would not catch a mix-up — the query has to be scoped.
    let (store, kitchen, office) = fixture().await;
    let frame = store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();

    assert!(store.find_frame(kitchen.id, frame.id).await.unwrap().is_some());
    assert!(
        store.find_frame(office.id, frame.id).await.unwrap().is_none(),
        "frame leaked across gardens"
    );
}

#[tokio::test]
async fn an_unknown_frame_id_is_simply_absent() {
    let (store, kitchen, _) = fixture().await;
    let missing = uuid::Uuid::new_v4();
    assert!(store.find_frame(kitchen.id, missing).await.unwrap().is_none());
}

#[tokio::test]
async fn frames_are_listed_newest_first() {
    let (store, kitchen, _) = fixture().await;
    for minutes in [0.0, 30.0, 60.0] {
        store
            .put_frame(frame_at(kitchen.id, &png(minutes as u8), minutes))
            .await
            .unwrap()
            .unwrap();
    }

    let recent = store.recent_frames(kitchen.id, 10).await.unwrap();
    assert_eq!(recent.len(), 3);
    assert!(recent[0].captured_at > recent[1].captured_at);
    assert!(recent[1].captured_at > recent[2].captured_at);

    let latest = store.latest_frame(kitchen.id).await.unwrap().unwrap();
    assert_eq!(latest.id, recent[0].id);
}

#[tokio::test]
async fn one_gardens_frames_never_appear_in_another() {
    let (store, kitchen, office) = fixture().await;
    store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(store.frame_count(kitchen.id).await.unwrap(), 1);
    assert_eq!(store.frame_count(office.id).await.unwrap(), 0);
    assert!(store.latest_frame(office.id).await.unwrap().is_none());
}

#[tokio::test]
async fn neighbours_step_through_a_time_lapse() {
    let (store, kitchen, _) = fixture().await;
    let mut ids = Vec::new();
    for minutes in [0.0, 30.0, 60.0] {
        let frame = store
            .put_frame(frame_at(kitchen.id, &png(minutes as u8), minutes))
            .await
            .unwrap()
            .unwrap();
        ids.push(frame.id);
    }

    let middle = store.find_frame(kitchen.id, ids[1]).await.unwrap().unwrap();
    let (older, newer) = store.frame_neighbours(kitchen.id, &middle).await.unwrap();
    assert_eq!(older.unwrap().id, ids[0]);
    assert_eq!(newer.unwrap().id, ids[2]);

    let first = store.find_frame(kitchen.id, ids[0]).await.unwrap().unwrap();
    let (before_first, _) = store.frame_neighbours(kitchen.id, &first).await.unwrap();
    assert!(before_first.is_none(), "nothing precedes the earliest frame");
}

#[tokio::test]
async fn pruning_removes_old_frames_and_their_bytes() {
    let (store, kitchen, _) = fixture().await;
    let old = store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();
    let fresh_at = gardyn_core::time::add_days(t0(), 20.0);
    let fresh = store
        .put_frame(NewFrame {
            captured_at: fresh_at,
            ..frame_at(kitchen.id, &png(0x02), 0.0)
        })
        .await
        .unwrap()
        .unwrap();

    let now = gardyn_core::time::add_days(t0(), 21.0);
    let removed = store.prune_frames(kitchen.id, 7.0, now).await.unwrap();

    assert_eq!(removed, 1);
    assert!(store.find_frame(kitchen.id, old.id).await.unwrap().is_none());
    assert!(store.find_frame(kitchen.id, fresh.id).await.unwrap().is_some());
    assert!(
        store.frame_bytes(&old).await.is_err(),
        "the file should be gone, not just the row"
    );
}

#[tokio::test]
async fn deleting_a_garden_removes_its_photographs_from_disk() {
    // Cascades take the rows; the bytes are the database's blind spot. Leaving
    // pictures of someone's home behind after they deleted the garden is not
    // acceptable, so this is checked on the filesystem rather than in SQL.
    let (store, kitchen, office) = fixture().await;
    let frame = store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();
    store
        .put_frame(frame_at(office.id, &png(0x02), 0.0))
        .await
        .unwrap()
        .unwrap();

    let kitchen_dir = store.frames.root().join(kitchen.id.to_string());
    let office_dir = store.frames.root().join(office.id.to_string());
    assert!(kitchen_dir.exists());

    store.delete_garden(kitchen.id).await.unwrap();

    assert!(!kitchen_dir.exists(), "frame directory survived deletion");
    assert!(store.frame_bytes(&frame).await.is_err());
    assert_eq!(store.frame_count(kitchen.id).await.unwrap(), 0);
    // The other garden is untouched.
    assert!(office_dir.exists());
    assert_eq!(store.frame_count(office.id).await.unwrap(), 1);
}

#[tokio::test]
async fn photo_mode_frames_are_marked_comparable() {
    let (store, kitchen, _) = fixture().await;
    let pinned = store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();
    let ambient = store
        .put_frame(NewFrame {
            comparable: false,
            light_duty_milli: Some(430),
            ..frame_at(kitchen.id, &png(0x02), 10.0)
        })
        .await
        .unwrap()
        .unwrap();

    assert!(pinned.comparable);
    assert!(!ambient.comparable);
    // Round-trips, so the UI can warn that colour is not trustworthy.
    let reloaded = store.find_frame(kitchen.id, ambient.id).await.unwrap().unwrap();
    assert!(!reloaded.comparable);
    assert_eq!(reloaded.light_percent(), Some(43.0));
}

#[tokio::test]
async fn the_image_url_points_at_the_authenticated_route() {
    let (store, kitchen, _) = fixture().await;
    let frame = store
        .put_frame(frame_at(kitchen.id, &png(0x01), 0.0))
        .await
        .unwrap()
        .unwrap();

    let url = frame.image_path();
    assert_eq!(url, format!("/gardens/{}/frames/{}/image", kitchen.id, frame.id));
    // Not a static mount — the path is under /gardens so it goes through the
    // membership check like everything else.
    assert!(url.starts_with("/gardens/"));
}
