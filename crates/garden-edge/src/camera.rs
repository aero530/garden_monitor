//! Capturing a frame.
//!
//! Shells out to whichever capture tool the image ships with rather than linking a
//! V4L2 crate. That is a deliberate trade: it costs a process spawn per hour, and it
//! buys cross-compilation with no native dependencies and a failure mode you can
//! reproduce by hand on the device.

use garden_core::Timestamp;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("no capture tool found; install one of: rpicam-still, libcamera-still, fswebcam")]
    NoTool,
    #[error("{tool} failed: {detail}")]
    Failed { tool: &'static str, detail: String },
    #[error("{tool} produced nothing at {path}")]
    NoOutput { tool: &'static str, path: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Frame {
    pub bytes: Vec<u8>,
    pub captured_at: Timestamp,
    pub width: u32,
    pub height: u32,
}

/// What the Studio 2's ultra-wide camera is asked for.
///
/// Full sensor resolution would make an 8 MB JPEG an hour; this is plenty for canopy
/// area and comfortably inside the brain's upload limit.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// Capture tools in preference order.
///
/// `rpicam-still` is the current Raspberry Pi OS name, `libcamera-still` the older
/// one, and `fswebcam` the fallback for a plain UVC device — which is what the
/// Gardyn's USB camera actually is.
fn candidates(path: &str) -> Vec<(&'static str, Vec<String>)> {
    let dimensions = format!("{WIDTH}x{HEIGHT}");
    vec![
        (
            "rpicam-still",
            vec![
                "--nopreview".into(),
                "--immediate".into(),
                "--width".into(),
                WIDTH.to_string(),
                "--height".into(),
                HEIGHT.to_string(),
                "-o".into(),
                path.to_string(),
            ],
        ),
        (
            "libcamera-still",
            vec![
                "--nopreview".into(),
                "--immediate".into(),
                "--width".into(),
                WIDTH.to_string(),
                "--height".into(),
                HEIGHT.to_string(),
                "-o".into(),
                path.to_string(),
            ],
        ),
        (
            "fswebcam",
            vec![
                "--no-banner".into(),
                "-r".into(),
                dimensions,
                // The Gardyn's USB camera needs a moment of auto-exposure before the
                // frame is worth keeping; without this the first shot is near-black.
                "--skip".into(),
                "8".into(),
                path.to_string(),
            ],
        ),
    ]
}

fn tool_exists(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn capture() -> Result<Frame, CameraError> {
    let temp: PathBuf =
        std::env::temp_dir().join(format!("garden-frame-{}.jpg", Timestamp::now().as_nanosecond()));
    let path = temp.to_string_lossy().to_string();

    let mut last_error = None;
    for (tool, args) in candidates(&path) {
        if !tool_exists(tool) {
            continue;
        }
        let output = Command::new(tool).args(&args).output()?;
        if !output.status.success() {
            last_error = Some(CameraError::Failed {
                tool,
                detail: String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(200)
                    .collect(),
            });
            continue;
        }

        let bytes = std::fs::read(&temp).unwrap_or_default();
        let _ = std::fs::remove_file(&temp);
        if bytes.is_empty() {
            last_error = Some(CameraError::NoOutput { tool, path });
            break;
        }

        return Ok(Frame {
            bytes,
            captured_at: Timestamp::now(),
            width: WIDTH,
            height: HEIGHT,
        });
    }

    Err(last_error.unwrap_or(CameraError::NoTool))
}

/// Adapts this module to `garden_hal::Camera`, so `garden_hal::photo_mode` can drive
/// the capture directly rather than the pinning logic being written out again here.
///
/// `light_duty_milli` is left at zero: this shells out to a capture tool and has no
/// idea what the room was lit at. `photo_mode` stamps the level it pinned, which is the
/// only place that knows for certain.
pub struct HalCamera;

impl garden_hal::Camera for HalCamera {
    fn capture(&mut self) -> garden_hal::Result<garden_hal::Frame> {
        let frame = capture().map_err(|e| garden_hal::HalError::Camera(e.to_string()))?;
        Ok(garden_hal::Frame {
            captured_at: frame.captured_at,
            width: frame.width,
            height: frame.height,
            data: frame.bytes,
            light_duty_milli: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_candidate_writes_to_the_path_it_was_given() {
        // A tool invoked without an output path would silently succeed and leave the
        // caller reading an empty file.
        for (tool, args) in candidates("/tmp/frame.jpg") {
            assert!(
                args.iter().any(|a| a == "/tmp/frame.jpg"),
                "{tool} was not told where to write"
            );
        }
    }

    #[test]
    fn the_requested_size_is_passed_to_every_tool() {
        for (tool, args) in candidates("/tmp/frame.jpg") {
            let joined = args.join(" ");
            assert!(
                joined.contains(&WIDTH.to_string()),
                "{tool} was not given a width: {joined}"
            );
        }
    }

    #[test]
    fn rpicam_is_preferred_over_the_legacy_name() {
        let order: Vec<_> = candidates("/tmp/f.jpg").into_iter().map(|(t, _)| t).collect();
        assert_eq!(order[0], "rpicam-still");
        assert!(order.contains(&"fswebcam"), "the UVC fallback must stay");
    }

    #[test]
    fn a_missing_tool_is_reported_as_such_rather_than_as_a_failure() {
        assert!(!tool_exists("definitely-not-a-real-capture-tool-xyz"));
    }
}
