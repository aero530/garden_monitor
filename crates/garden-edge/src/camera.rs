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

/// What the Studio 2's ultra-wide camera is asked for: its full 4:3 sensor.
///
/// **The aspect ratio is the load-bearing part, not the pixel count.** This was
/// 1920×1080, chosen to keep the JPEG small, and that quietly threw away field of
/// view: `v4l2-ctl --list-formats-ext` reports a 3264×2448 sensor whose only 16:9
/// modes — 1920×1080 and 1280×720 — are centre crops of it. On an ultra-wide lens
/// aimed at a tower, the cropped strip is exactly the part with plants in it.
///
/// After the brain's quarter turn the sensor's vertical axis becomes the stored
/// frame's horizontal one, so the loss shows up as a *narrower* picture than the
/// factory app's, which is how it was noticed.
///
/// Size is handled where it costs nothing: the brain already decodes and re-encodes
/// every frame to rotate it, so it downscales in the same pass. The Pi uploads what
/// the camera gave it and does no image processing at all — which matters on a single
/// ARMv6 core.
const WIDTH: u32 = 3264;
const HEIGHT: u32 = 2448;

/// Resolution override, for a device whose camera is not this one.
///
/// Set both or neither. A width without a height is a typo, not a request, and
/// honouring half of it would pick some other mode than either value intended.
fn requested_size() -> (u32, u32) {
    let read = |key: &str| std::env::var(key).ok()?.parse::<u32>().ok().filter(|v| *v > 0);
    match (read("GARDEN_CAMERA_WIDTH"), read("GARDEN_CAMERA_HEIGHT")) {
        (Some(w), Some(h)) => (w, h),
        _ => (WIDTH, HEIGHT),
    }
}

/// Width and height straight out of the JPEG, rather than what we asked for.
///
/// A capture tool given a mode the camera does not have picks a nearby one and says
/// nothing, so the requested size is a claim and this is a measurement. Reading the
/// SOF marker by hand keeps a whole image decoder off the Pi for the sake of four
/// bytes.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // JPEG is a chain of marker segments: 0xFF, a marker byte, then a big-endian
    // length covering itself. A Start Of Frame carries the dimensions; every other
    // segment is skipped by its own length.
    let mut i = 2; // Past the SOI.
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // SOF0..SOF15, excluding DHT (0xC4), JPG (0xC8) and DAC (0xCC), which share
        // the range but are not frame headers.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let height = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            let width = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        let length = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
        if length < 2 {
            return None;
        }
        i += 2 + length;
    }
    None
}

/// Capture tools in preference order.
///
/// `rpicam-still` is the current Raspberry Pi OS name, `libcamera-still` the older
/// one, and `fswebcam` the fallback for a plain UVC device — which is what the
/// Gardyn's USB camera actually is.
fn candidates(path: &str) -> Vec<(&'static str, Vec<String>)> {
    let (width, height) = requested_size();
    let dimensions = format!("{width}x{height}");
    vec![
        (
            "rpicam-still",
            vec![
                "--nopreview".into(),
                "--immediate".into(),
                "--width".into(),
                width.to_string(),
                "--height".into(),
                height.to_string(),
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
                width.to_string(),
                "--height".into(),
                height.to_string(),
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

        // What arrived, falling back to what was asked for only when the bytes will
        // not say. The brain re-derives dimensions from the decode anyway, so this is
        // for the agent's own logs and for `garden_hal::photo_mode` — both of which
        // are worse than useless if they report a mode the camera declined to give.
        let (width, height) = jpeg_dimensions(&bytes).unwrap_or_else(requested_size);
        return Ok(Frame {
            bytes,
            captured_at: Timestamp::now(),
            width,
            height,
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
    fn the_default_mode_has_the_sensor_s_own_aspect_ratio() {
        // The whole point of the change that set these. A 16:9 mode on this camera is
        // a centre crop of a 4:3 sensor, and on an ultra-wide lens pointed at a tower
        // the cropped strip is the part with plants in it. Pixel count may be traded
        // freely; the ratio may not.
        let ratio = f64::from(WIDTH) / f64::from(HEIGHT);
        assert!(
            (ratio - 4.0 / 3.0).abs() < 0.01,
            "{WIDTH}x{HEIGHT} is {ratio:.3}, not 4:3"
        );
    }

    #[test]
    fn dimensions_come_from_the_jpeg_rather_than_from_the_request() {
        // A minimal JPEG: SOI, an APP0 segment to be skipped, then a SOF0 declaring
        // 2448 high by 3264 wide. Proves the skip arithmetic as well as the read.
        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x06, b'J', b'F', b'I', b'F']);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&2448u16.to_be_bytes());
        jpeg.extend_from_slice(&3264u16.to_be_bytes());
        jpeg.extend_from_slice(&[0x03; 10]);

        assert_eq!(jpeg_dimensions(&jpeg), Some((3264, 2448)));
    }

    #[test]
    fn a_huffman_table_is_not_mistaken_for_a_frame_header() {
        // 0xC4 sits inside the SOF marker range and is not one. Reading it as a frame
        // header yields whatever the table's contents happen to look like, which is a
        // plausible-looking wrong answer rather than an obvious failure.
        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x08, 1, 2, 3, 4, 5, 6]);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&1200u16.to_be_bytes());
        jpeg.extend_from_slice(&1600u16.to_be_bytes());
        jpeg.extend_from_slice(&[0x03; 10]);

        assert_eq!(jpeg_dimensions(&jpeg), Some((1600, 1200)));
    }

    #[test]
    fn a_truncated_or_non_jpeg_body_reports_nothing_rather_than_guessing() {
        assert_eq!(jpeg_dimensions(&[]), None);
        assert_eq!(jpeg_dimensions(&[0xFF, 0xD8, 0xFF, 0xC0]), None);
        assert_eq!(jpeg_dimensions(b"this is not a jpeg at all"), None);
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
