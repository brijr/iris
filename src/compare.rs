//! Pixel comparison of two captures.
//!
//! Decoded pixels are composited on white and compared per channel.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, ImageFormat, ImageReader, Limits, RgbaImage};
use serde::Serialize;

use crate::capture::absolute_output;

/// Reject dimensions and decoded buffers too large to compare safely.
const MAX_DIMENSION: u32 = 32_768;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DIFF_BYTES: u64 = 512 * 1024 * 1024;

/// Difference-image color for a changed pixel.
const CHANGED: [u8; 3] = [255, 0, 0];
/// Difference-image color for union-canvas area neither capture covers.
const BLANK: [u8; 3] = [255, 255, 255];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

/// Smallest rectangle containing every changed pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Bounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub before: Dimensions,
    pub after: Dimensions,
    pub same_dimensions: bool,
    pub changed_pixels: u64,
    /// Pixels in the rectangular union canvas.
    pub total_pixels: u64,
    pub changed_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Compares two captures pixel by pixel.
///
/// A pixel changes when its largest composited channel gap is greater than
/// `threshold`.
///
/// Different sizes use the union canvas without rescaling. A pixel covered by
/// only one capture changes.
///
/// When `output` is present, writes a PNG with changes in red and unchanged
/// pixels faded from `after`.
pub fn compare(
    before: &Path,
    after: &Path,
    threshold: u8,
    output: Option<&Path>,
    is_cancelled: impl Fn() -> bool,
) -> Result<Comparison> {
    ensure!(!is_cancelled(), "comparison cancelled");
    let before_image = decode(before, "before")?;
    ensure!(!is_cancelled(), "comparison cancelled");
    let after_image = decode(after, "after")?;
    ensure!(!is_cancelled(), "comparison cancelled");

    let (before_width, before_height) = before_image.dimensions();
    let (after_width, after_height) = after_image.dimensions();
    let before_width_usize = usize::try_from(before_width)
        .context("before image width exceeds this platform's address space")?;
    let before_height_usize = usize::try_from(before_height)
        .context("before image height exceeds this platform's address space")?;
    let after_width_usize = usize::try_from(after_width)
        .context("after image width exceeds this platform's address space")?;
    let after_height_usize = usize::try_from(after_height)
        .context("after image height exceeds this platform's address space")?;
    let union_width = before_width.max(after_width);
    let union_height = before_height.max(after_height);
    let union_width_usize = usize::try_from(union_width)
        .context("difference image width exceeds this platform's address space")?;
    let union_height_usize = usize::try_from(union_height)
        .context("difference image height exceeds this platform's address space")?;
    let total_pixels = u64::from(union_width) * u64::from(union_height);

    let mut diff = if output.is_some() {
        let bytes = total_pixels * 3;
        if bytes > MAX_DIFF_BYTES {
            bail!(
                "a {union_width}x{union_height} difference image needs {bytes} bytes of memory; \
                 compare smaller captures or omit the difference output"
            );
        }
        let length = usize::try_from(bytes)
            .context("difference image exceeds this platform's address space")?;
        Some(vec![0; length])
    } else {
        None
    };

    let before_raw = before_image.as_raw();
    let after_raw = after_image.as_raw();
    let diff_stride = union_width_usize * 3;

    let mut changed_pixels = 0_u64;
    let mut change_box = ChangeBox::new();

    for y in 0..union_height_usize {
        ensure!(!is_cancelled(), "comparison cancelled");
        let before_row = row(before_raw, before_width_usize, before_height_usize, y);
        let after_row = row(after_raw, after_width_usize, after_height_usize, y);
        let overlap = match (before_row, after_row) {
            (Some(_), Some(_)) => before_width_usize.min(after_width_usize),
            _ => 0,
        };
        let mut diff_row = diff
            .as_mut()
            .map(|buffer| &mut buffer[y * diff_stride..][..diff_stride]);

        let mut row_changed = 0_u64;
        let mut first = 0;
        let mut last = 0;

        if let (Some(before_row), Some(after_row)) = (before_row, after_row) {
            let span = overlap * 4;
            for (x, (before_pixel, after_pixel)) in before_row[..span]
                .as_chunks::<4>()
                .0
                .iter()
                .zip(after_row[..span].as_chunks::<4>().0.iter())
                .enumerate()
            {
                let after_pixel = on_white(after_pixel);
                let changed = distance(on_white(before_pixel), after_pixel) > threshold;
                if changed {
                    if row_changed == 0 {
                        first = x;
                    }
                    last = x;
                    row_changed += 1;
                }
                if let Some(target) = diff_row.as_deref_mut() {
                    let color = if changed { CHANGED } else { ghost(after_pixel) };
                    let at = x * 3;
                    target[at..at + 3].copy_from_slice(&color);
                }
            }
        }

        let before_has_row = before_row.is_some();
        let after_has_row = after_row.is_some();
        let row_width = if diff_row.is_some() {
            union_width_usize
        } else {
            before_row
                .map_or(0, |row| row.len() / 4)
                .max(after_row.map_or(0, |row| row.len() / 4))
        };
        for x in overlap..row_width {
            let changed = (before_has_row && x < before_width_usize)
                != (after_has_row && x < after_width_usize);
            if changed {
                if row_changed == 0 {
                    first = x;
                }
                last = x;
                row_changed += 1;
            }
            if let Some(target) = diff_row.as_deref_mut() {
                let at = x * 3;
                target[at..at + 3].copy_from_slice(if changed { &CHANGED } else { &BLANK });
            }
        }

        if row_changed > 0 {
            changed_pixels += row_changed;
            change_box.add_row(y, first, last);
        }
    }

    ensure!(!is_cancelled(), "comparison cancelled");
    let output = match (output, diff) {
        (Some(path), Some(pixels)) => {
            write_diff(path, union_width, union_height, &pixels)?;
            Some(absolute_output(path))
        }
        _ => None,
    };
    let changed_fraction = if total_pixels == 0 {
        0.0
    } else {
        let changed =
            u32::try_from(changed_pixels).context("changed pixel count exceeds image limits")?;
        let total = u32::try_from(total_pixels).context("pixel count exceeds image limits")?;
        f64::from(changed) / f64::from(total)
    };

    Ok(Comparison {
        before: Dimensions {
            width: before_width,
            height: before_height,
        },
        after: Dimensions {
            width: after_width,
            height: after_height,
        },
        same_dimensions: before_width == after_width && before_height == after_height,
        changed_pixels,
        total_pixels,
        changed_fraction,
        bounds: change_box.finish(),
        output,
    })
}

fn decode(path: &Path, role: &str) -> Result<RgbaImage> {
    let mut reader = ImageReader::open(path)
        .with_context(|| format!("failed to open {role} image {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to read {role} image {}", path.display()))?;

    // Detect the format from file content, not the extension.
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) => {}
        Some(other) => bail!(
            "{role} image {} is {other:?}; iris compares PNG, JPEG and WebP",
            path.display()
        ),
        None => bail!(
            "{role} image {} is in no format iris knows; it compares PNG, JPEG and WebP",
            path.display()
        ),
    }

    let mut limits = Limits::no_limits();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);

    let decoded = reader
        .decode()
        .with_context(|| format!("failed to decode {role} image {}", path.display()))?;
    let rgba_bytes = u64::from(decoded.width()) * u64::from(decoded.height()) * 4;
    if rgba_bytes > MAX_DECODE_BYTES {
        bail!(
            "{role} image needs {rgba_bytes} bytes after RGBA conversion; compare a smaller capture"
        );
    }
    Ok(decoded.into_rgba8())
}

/// One RGBA8 row, or `None` when `y` is outside the image.
#[inline]
fn row(raw: &[u8], width: usize, height: usize, y: usize) -> Option<&[u8]> {
    if y >= height {
        return None;
    }
    let stride = width * 4;
    let start = y * stride;
    Some(&raw[start..start + stride])
}

/// Composites an RGBA8 pixel on white.
#[inline]
fn on_white(pixel: &[u8]) -> [u8; 3] {
    let alpha = u32::from(pixel[3]);
    if alpha == 255 {
        return [pixel[0], pixel[1], pixel[2]];
    }
    let white = 255 * (255 - alpha);
    [pixel[0], pixel[1], pixel[2]].map(|channel| {
        // SAFETY: channel * alpha + white <= 255 * 255, so the rounded quotient fits in a u8.
        u8::try_from((u32::from(channel) * alpha + white + 127) / 255)
            .expect("alpha compositing must stay within a channel")
    })
}

/// Largest per-channel gap between two composited pixels.
#[inline]
fn distance(before: [u8; 3], after: [u8; 3]) -> u8 {
    before[0]
        .abs_diff(after[0])
        .max(before[1].abs_diff(after[1]))
        .max(before[2].abs_diff(after[2]))
}

/// Fades an unchanged pixel in difference output.
#[inline]
fn ghost(pixel: [u8; 3]) -> [u8; 3] {
    let luma =
        (u32::from(pixel[0]) * 299 + u32::from(pixel[1]) * 587 + u32::from(pixel[2]) * 114) / 1000;
    // SAFETY: weights sum to 1000, so luma and its faded value are within 0..=255.
    let gray = u8::try_from(255 - (255 - luma) / 4).expect("luma must stay within a channel");
    [gray; 3]
}

fn write_diff(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let file = File::create(path).with_context(|| format!("failed to write {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    PngEncoder::new_with_quality(&mut writer, CompressionType::Fast, FilterType::Adaptive)
        .write_image(pixels, width, height, ExtendedColorType::Rgb8)
        .with_context(|| format!("failed to write {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("failed to write {}", path.display()))
}

struct ChangeBox {
    any: bool,
    min_x: usize,
    max_x: usize,
    min_y: usize,
    max_y: usize,
}

impl ChangeBox {
    const fn new() -> Self {
        Self {
            any: false,
            min_x: usize::MAX,
            max_x: 0,
            min_y: 0,
            max_y: 0,
        }
    }

    fn add_row(&mut self, y: usize, first: usize, last: usize) {
        if !self.any {
            self.any = true;
            self.min_y = y;
        }
        self.max_y = y;
        self.min_x = self.min_x.min(first);
        self.max_x = self.max_x.max(last);
    }

    fn finish(&self) -> Option<Bounds> {
        if !self.any {
            return None;
        }
        Some(Bounds {
            x: u32::try_from(self.min_x).expect("change bounds are within image limits"),
            y: u32::try_from(self.min_y).expect("change bounds are within image limits"),
            width: u32::try_from(self.max_x - self.min_x + 1)
                .expect("change bounds are within image limits"),
            height: u32::try_from(self.max_y - self.min_y + 1)
                .expect("change bounds are within image limits"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("iris-compare-{label}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Encodes a PNG fixture.
    fn write_rgba(path: &Path, width: u32, height: u32, pixels: &[u8]) {
        let mut writer = BufWriter::new(File::create(path).expect("create fixture"));
        PngEncoder::new(&mut writer)
            .write_image(pixels, width, height, ExtendedColorType::Rgba8)
            .expect("encode fixture");
        writer.flush().expect("flush fixture");
    }

    #[test]
    fn transparent_pixels_match_the_white_behind_them() {
        let dir = TempDir::new("alpha");
        let before = dir.join("before.png");
        let after = dir.join("after.png");

        // Transparent source colors do not affect composited output.
        write_rgba(&before, 2, 1, &[0, 0, 0, 0, 17, 99, 200, 0]);
        write_rgba(&after, 2, 1, &[255; 8]);

        let result = compare(&before, &after, 0, None, || false).expect("compare");

        assert_eq!(result.changed_pixels, 0);
        assert_eq!(result.bounds, None);
        assert!(result.same_dimensions);
    }

    #[test]
    fn area_only_one_capture_covers_counts_as_changed() {
        let dir = TempDir::new("union");
        let before = dir.join("before.png");
        let after = dir.join("after.png");

        write_rgba(&before, 2, 2, &[255; 16]);
        write_rgba(&after, 3, 2, &[255; 24]);

        let result = compare(&before, &after, 0, None, || false).expect("compare");

        assert!(!result.same_dimensions);
        // The extra column must change.
        assert_eq!(result.total_pixels, 6);
        assert_eq!(result.changed_pixels, 2);
        assert_eq!(
            result.bounds,
            Some(Bounds {
                x: 2,
                y: 0,
                width: 1,
                height: 2
            })
        );
    }

    #[test]
    fn threshold_forgives_its_own_distance_and_bounds_frame_the_rest() {
        let dir = TempDir::new("threshold");
        let before = dir.join("before.png");
        let after = dir.join("after.png");

        let white = [255u8; 36];
        let mut altered = white;
        let at = |x: usize, y: usize| (y * 3 + x) * 4;
        altered[at(1, 1)] = 245;
        altered[at(2, 2)] = 0;
        altered[at(2, 2) + 1] = 0;

        write_rgba(&before, 3, 3, &white);
        write_rgba(&after, 3, 3, &altered);

        let forgiven = compare(&before, &after, 10, None, || false).expect("compare");
        assert_eq!(forgiven.changed_pixels, 1);
        assert_eq!(
            forgiven.bounds,
            Some(Bounds {
                x: 2,
                y: 2,
                width: 1,
                height: 1
            })
        );

        let counted = compare(&before, &after, 9, None, || false).expect("compare");
        assert_eq!(counted.changed_pixels, 2);
        assert_eq!(
            counted.bounds,
            Some(Bounds {
                x: 1,
                y: 1,
                width: 2,
                height: 2
            })
        );
    }
}
