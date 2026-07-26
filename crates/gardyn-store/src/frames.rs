//! Camera frames: an index in SQLite, bytes on disk.
//!
//! Two rules govern everything here.
//!
//! **Filesystem paths are never derived from client input.** A path is built from two
//! server-generated UUIDs and an extension chosen from a closed set. There is no
//! string from an agent or a browser anywhere in it, so path traversal is not
//! mitigated — it is unrepresentable.
//!
//! **Uploaded bytes are sniffed, not trusted.** An agent that says "image/jpeg" while
//! sending HTML would otherwise get its content served back from our origin, which is
//! stored cross-site scripting with extra steps.

use crate::{Result, Store, StoreError, ts};
use gardyn_core::GardenId;
use jiff::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The image formats we accept, identified by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
}

impl ImageKind {
    /// Identify an image from its leading bytes.
    ///
    /// Returns `None` for anything unrecognised, which is what keeps a text/html
    /// payload from being stored and later echoed back to a browser.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
        const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

        if bytes.starts_with(JPEG) {
            Some(ImageKind::Jpeg)
        } else if bytes.starts_with(PNG) {
            Some(ImageKind::Png)
        } else {
            None
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Png => "image/png",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "jpg",
            ImageKind::Png => "png",
        }
    }

    pub fn from_content_type(value: &str) -> Option<Self> {
        match value {
            "image/jpeg" => Some(ImageKind::Jpeg),
            "image/png" => Some(ImageKind::Png),
            _ => None,
        }
    }
}

/// Largest frame we will accept. A Studio 2 still is well under this; anything larger
/// is a misconfigured agent or an attempt to fill the disk.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    /// Uploaded by an edge agent.
    Agent,
    /// Rendered from the physics model, for gardens with no hardware.
    Simulated,
}

impl FrameSource {
    pub fn slug(self) -> &'static str {
        match self {
            FrameSource::Agent => "agent",
            FrameSource::Simulated => "simulated",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "agent" => Ok(FrameSource::Agent),
            "simulated" => Ok(FrameSource::Simulated),
            other => Err(StoreError::Corrupt(format!("frame source {other:?}"))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub id: Uuid,
    pub garden: GardenId,
    pub captured_at: Timestamp,
    pub width: u32,
    pub height: u32,
    pub kind: ImageKind,
    pub byte_size: i64,
    pub light_duty_milli: Option<i64>,
    /// Captured in photo mode, so colour is comparable with other frames.
    pub comparable: bool,
    pub source: FrameSource,
}

impl Frame {
    /// Relative URL of the image, for use in an `<img src>`.
    pub fn image_path(&self) -> String {
        format!("/gardens/{}/frames/{}/image", self.garden, self.id)
    }

    pub fn light_percent(&self) -> Option<f32> {
        self.light_duty_milli.map(|d| d as f32 / 10.0)
    }
}

/// A new frame, before it is stored.
pub struct NewFrame<'a> {
    pub garden: GardenId,
    pub captured_at: Timestamp,
    pub width: u32,
    pub height: u32,
    pub light_duty_milli: Option<i64>,
    pub comparable: bool,
    pub source: FrameSource,
    pub bytes: &'a [u8],
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("that is not a JPEG or PNG image")]
    UnrecognisedFormat,
    #[error("image is too large ({0} bytes; the limit is {MAX_FRAME_BYTES})")]
    TooLarge(usize),
    #[error("image is empty")]
    Empty,
}

/// Where frame bytes live on disk.
#[derive(Debug, Clone)]
pub struct FrameStore {
    root: PathBuf,
}

impl FrameStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Both components are server-generated UUIDs and the extension comes from a
    /// closed enum, so no caller-supplied string reaches the filesystem.
    fn path_for(&self, garden: GardenId, id: Uuid, kind: ImageKind) -> PathBuf {
        self.root
            .join(garden.to_string())
            .join(format!("{id}.{}", kind.extension()))
    }

    fn write(&self, garden: GardenId, id: Uuid, kind: ImageKind, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(garden, id, kind);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Corrupt(format!("creating {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| StoreError::Corrupt(format!("writing {}: {e}", path.display())))
    }

    fn read(&self, garden: GardenId, id: Uuid, kind: ImageKind) -> Result<Vec<u8>> {
        let path = self.path_for(garden, id, kind);
        std::fs::read(&path).map_err(|_| StoreError::NotFound)
    }

    fn remove(&self, garden: GardenId, id: Uuid, kind: ImageKind) {
        // A missing file is not an error; the index row is the source of truth.
        let _ = std::fs::remove_file(self.path_for(garden, id, kind));
    }

    /// Delete every frame belonging to a garden.
    ///
    /// Called when a garden is deleted. The directory name is a UUID we generated, so
    /// this recursive removal cannot be aimed anywhere unexpected.
    pub(crate) fn remove_garden_directory(&self, garden: GardenId) {
        let _ = std::fs::remove_dir_all(self.root.join(garden.to_string()));
    }
}

fn frame_from_row(row: &SqliteRow) -> Result<Frame> {
    let id: String = row.try_get("id")?;
    let garden: String = row.try_get("garden_id")?;
    let content_type: String = row.try_get("content_type")?;
    let source: String = row.try_get("source")?;

    Ok(Frame {
        id: Uuid::parse_str(&id).map_err(|e| StoreError::Corrupt(format!("frame id: {e}")))?,
        garden: GardenId(
            Uuid::parse_str(&garden).map_err(|e| StoreError::Corrupt(format!("garden: {e}")))?,
        ),
        captured_at: ts::decode(&row.try_get::<String, _>("captured_at")?)?,
        width: row.try_get::<i64, _>("width")? as u32,
        height: row.try_get::<i64, _>("height")? as u32,
        kind: ImageKind::from_content_type(&content_type)
            .ok_or_else(|| StoreError::Corrupt(format!("content type {content_type:?}")))?,
        byte_size: row.try_get("byte_size")?,
        light_duty_milli: row.try_get("light_duty_milli")?,
        comparable: row.try_get::<i64, _>("comparable")? != 0,
        source: FrameSource::parse(&source)?,
    })
}

impl Store {
    /// Store a frame. Validates the bytes before anything touches the disk.
    pub async fn put_frame(&self, new: NewFrame<'_>) -> Result<std::result::Result<Frame, FrameError>> {
        if new.bytes.is_empty() {
            return Ok(Err(FrameError::Empty));
        }
        if new.bytes.len() > MAX_FRAME_BYTES {
            return Ok(Err(FrameError::TooLarge(new.bytes.len())));
        }
        let Some(kind) = ImageKind::sniff(new.bytes) else {
            return Ok(Err(FrameError::UnrecognisedFormat));
        };

        let id = Uuid::new_v4();
        // Disk first: an index row pointing at a missing file would render as a broken
        // image, whereas an orphaned file is invisible and reclaimed by pruning.
        self.frames.write(new.garden, id, kind, new.bytes)?;

        let result = sqlx::query(
            "INSERT INTO frames (id, garden_id, captured_at, width, height, content_type,
                byte_size, light_duty_milli, comparable, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )
        .bind(id.to_string())
        .bind(new.garden.to_string())
        .bind(ts::encode(new.captured_at))
        .bind(i64::from(new.width))
        .bind(i64::from(new.height))
        .bind(kind.content_type())
        .bind(new.bytes.len() as i64)
        .bind(new.light_duty_milli)
        .bind(i64::from(new.comparable))
        .bind(new.source.slug())
        .bind(ts::encode(new.captured_at))
        .execute(&self.db)
        .await;

        if let Err(e) = result {
            self.frames.remove(new.garden, id, kind);
            return Err(e.into());
        }

        Ok(Ok(Frame {
            id,
            garden: new.garden,
            captured_at: new.captured_at,
            width: new.width,
            height: new.height,
            kind,
            byte_size: new.bytes.len() as i64,
            light_duty_milli: new.light_duty_milli,
            comparable: new.comparable,
            source: new.source,
        }))
    }

    /// Look a frame up **within a garden**.
    ///
    /// The garden is part of the query rather than checked afterwards, so a frame id
    /// from one garden cannot be fetched through another garden's URL even if the
    /// caller is a member of that other one.
    pub async fn find_frame(&self, garden: GardenId, id: Uuid) -> Result<Option<Frame>> {
        let row = sqlx::query("SELECT * FROM frames WHERE garden_id = ?1 AND id = ?2")
            .bind(garden.to_string())
            .bind(id.to_string())
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(frame_from_row).transpose()
    }

    pub async fn frame_bytes(&self, frame: &Frame) -> Result<Vec<u8>> {
        self.frames.read(frame.garden, frame.id, frame.kind)
    }

    pub async fn latest_frame(&self, garden: GardenId) -> Result<Option<Frame>> {
        let row = sqlx::query(
            "SELECT * FROM frames WHERE garden_id = ?1 ORDER BY captured_at DESC LIMIT 1",
        )
        .bind(garden.to_string())
        .fetch_optional(&self.db)
        .await?;
        row.as_ref().map(frame_from_row).transpose()
    }

    /// Recent frames, newest first.
    pub async fn recent_frames(&self, garden: GardenId, limit: i64) -> Result<Vec<Frame>> {
        let rows = sqlx::query(
            "SELECT * FROM frames WHERE garden_id = ?1 ORDER BY captured_at DESC LIMIT ?2",
        )
        .bind(garden.to_string())
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.db)
        .await?;
        rows.iter().map(frame_from_row).collect()
    }

    pub async fn frame_count(&self, garden: GardenId) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM frames WHERE garden_id = ?1")
            .bind(garden.to_string())
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// The frames either side of one, for stepping through a time-lapse.
    pub async fn frame_neighbours(
        &self,
        garden: GardenId,
        frame: &Frame,
    ) -> Result<(Option<Frame>, Option<Frame>)> {
        let at = ts::encode(frame.captured_at);

        let older = sqlx::query(
            "SELECT * FROM frames WHERE garden_id = ?1 AND captured_at < ?2
             ORDER BY captured_at DESC LIMIT 1",
        )
        .bind(garden.to_string())
        .bind(&at)
        .fetch_optional(&self.db)
        .await?;

        let newer = sqlx::query(
            "SELECT * FROM frames WHERE garden_id = ?1 AND captured_at > ?2
             ORDER BY captured_at ASC LIMIT 1",
        )
        .bind(garden.to_string())
        .bind(&at)
        .fetch_optional(&self.db)
        .await?;

        Ok((
            older.as_ref().map(frame_from_row).transpose()?,
            newer.as_ref().map(frame_from_row).transpose()?,
        ))
    }

    /// Delete frames older than `keep_days`, bytes included.
    ///
    /// Without this a garden accumulates roughly 8,700 images a year and quietly
    /// fills the disk. Callers run it on a schedule.
    pub async fn prune_frames(
        &self,
        garden: GardenId,
        keep_days: f64,
        now: Timestamp,
    ) -> Result<u64> {
        let cutoff = gardyn_core::time::add_days(now, -keep_days);
        let doomed = sqlx::query("SELECT * FROM frames WHERE garden_id = ?1 AND captured_at < ?2")
            .bind(garden.to_string())
            .bind(ts::encode(cutoff))
            .fetch_all(&self.db)
            .await?;

        let mut removed = 0;
        for row in &doomed {
            let frame = frame_from_row(row)?;
            self.frames.remove(frame.garden, frame.id, frame.kind);
            sqlx::query("DELETE FROM frames WHERE id = ?1")
                .bind(frame.id.to_string())
                .execute(&self.db)
                .await?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_and_png_are_recognised() {
        assert_eq!(
            ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(
            ImageKind::sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00]),
            Some(ImageKind::Png)
        );
    }

    #[test]
    fn anything_else_is_refused() {
        // The one that matters: HTML uploaded as an "image" would otherwise be served
        // back from our own origin.
        assert_eq!(ImageKind::sniff(b"<html><script>alert(1)</script>"), None);
        assert_eq!(ImageKind::sniff(b"GIF89a"), None);
        assert_eq!(ImageKind::sniff(b"%PDF-1.4"), None);
        assert_eq!(ImageKind::sniff(b""), None);
        assert_eq!(ImageKind::sniff(&[0xFF, 0xD8]), None, "truncated magic");
    }

    #[test]
    fn a_claimed_content_type_cannot_override_the_bytes() {
        // `from_content_type` exists only for reading rows we wrote ourselves; the
        // write path always sniffs.
        assert_eq!(
            ImageKind::from_content_type("image/jpeg"),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(ImageKind::from_content_type("text/html"), None);
    }

    #[test]
    fn stored_paths_contain_no_caller_supplied_text() {
        let store = FrameStore::new("/data/frames");
        let garden = GardenId::new();
        let id = Uuid::new_v4();
        let path = store.path_for(garden, id, ImageKind::Jpeg);
        let rendered = path.to_string_lossy().replace('\\', "/");

        assert!(rendered.starts_with("/data/frames/"));
        assert!(rendered.contains(&garden.to_string()));
        assert!(rendered.ends_with(&format!("{id}.jpg")));
        assert!(!rendered.contains(".."));
    }

    #[test]
    fn frames_from_different_gardens_never_share_a_directory() {
        let store = FrameStore::new("/data/frames");
        let id = Uuid::new_v4();
        let a = store.path_for(GardenId::new(), id, ImageKind::Png);
        let b = store.path_for(GardenId::new(), id, ImageKind::Png);
        assert_ne!(a, b);
    }
}
