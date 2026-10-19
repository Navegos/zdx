//! Drawing the user's pins and areas on the canonical copy of an image
//! artifact (see `canonical`), so the agent sees what was marked on the very
//! pixels the user marked, instead of only reading coordinates.

use image::imageops::FilterType;
use image::{DynamicImage, Rgba, RgbaImage};

use super::canonical::{self, CanonicalImage};
use super::{FeedbackAnnotation, FeedbackError, FeedbackPoint, FeedbackTarget};

/// Pixel writes allowed for drawing every mark on every rendered image. Point
/// limits alone do not bound this: a few points can make long lines, and each
/// line is stamped with discs. About 0.3 s in a release build.
const MAX_DRAW_OPS: f64 = 80_000_000.0;
/// Byte budget of the marked copy: the bot's inline image limit, so the model
/// gets these exact bytes and not a re-encode.
pub const MARKED_MAX_BYTES: usize = 1024 * 1024;
const CLOSEUP_MAX_EDGE: u32 = 1024;
const CLOSEUP_MIN_EDGE: u32 = 512;
/// Upper bound on enlarging a close-up: past this a crop shows blocks, not detail.
const CLOSEUP_MAX_UPSCALE: f64 = 4.0;
/// Smallest close-up region, in copy pixels (when the copy is that large).
const CLOSEUP_MIN_SIDE: f64 = 192.0;
/// A close-up covering this much of the image adds nothing over the overview.
const CLOSEUP_MAX_COVERAGE: f64 = 0.6;
const CLOSEUP_JPEG_QUALITY: u8 = 90;
/// Qualities tried for the marked copy, highest first. Its size never changes.
const MARKED_JPEG_LADDER: [u8; 4] = [90, 80, 70, 60];

const RED: Rgba<u8> = Rgba([229, 50, 45, 255]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);

/// An image the bundle adds for the agent.
pub(super) struct RenderedImage {
    pub name: String,
    pub label: String,
    /// The note it belongs to; `None` for the overview.
    pub note: Option<usize>,
    pub bytes: Vec<u8>,
}

/// A mark in copy pixels.
enum Mark {
    Pin((f64, f64)),
    Area(Vec<Vec<(f64, f64)>>),
}

fn to_pixels(point: FeedbackPoint, width: u32, height: u32) -> (f64, f64) {
    (point.x * f64::from(width), point.y * f64::from(height))
}

fn marks((width, height): (u32, u32), annotations: &[FeedbackAnnotation]) -> Vec<(usize, Mark)> {
    annotations
        .iter()
        .enumerate()
        .filter_map(|(index, annotation)| {
            let mark = match &annotation.target {
                FeedbackTarget::ImagePin { point } => Mark::Pin(to_pixels(*point, width, height)),
                FeedbackTarget::ImageArea { strokes } => Mark::Area(
                    strokes
                        .iter()
                        .map(|stroke| {
                            stroke
                                .iter()
                                .map(|point| to_pixels(*point, width, height))
                                .collect()
                        })
                        .collect(),
                ),
                _ => return None,
            };
            Some((index + 1, mark))
        })
        .collect()
}

/// Pixel bounds of a set of strokes: `(x0, y0, x1, y1)`.
pub(super) fn stroke_bounds(
    strokes: &[Vec<FeedbackPoint>],
    width: u32,
    height: u32,
) -> (f64, f64, f64, f64) {
    let mut bounds = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for point in strokes.iter().flatten() {
        let (x, y) = to_pixels(*point, width, height);
        bounds = (
            bounds.0.min(x),
            bounds.1.min(y),
            bounds.2.max(x),
            bounds.3.max(y),
        );
    }
    bounds
}

fn mark_bounds(mark: &Mark) -> (f64, f64, f64, f64) {
    match mark {
        Mark::Pin((x, y)) => (*x, *y, *x, *y),
        Mark::Area(strokes) => {
            let mut bounds = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for (x, y) in strokes.iter().flatten() {
                bounds = (
                    bounds.0.min(*x),
                    bounds.1.min(*y),
                    bounds.2.max(*x),
                    bounds.3.max(*y),
                );
            }
            bounds
        }
    }
}

/// One rendered image: where its source region starts, its scale, and the
/// marks drawn on it.
struct Canvas {
    /// `None` for the overview, else the close-up's note and source region.
    closeup: Option<(usize, (u32, u32, u32, u32))>,
    scale: f64,
    size: (u32, u32),
}

impl Canvas {
    fn origin(&self) -> (f64, f64) {
        self.closeup
            .map_or((0.0, 0.0), |(_, (x, y, _, _))| (f64::from(x), f64::from(y)))
    }

    fn unit(&self) -> f64 {
        unit_for(self.size.0, self.size.1)
    }

    fn draws(&self, n: usize) -> bool {
        self.closeup.is_none_or(|(note, _)| note == n)
    }
}

/// The marked copy (always: the agent sees the image even for whole-image
/// notes), at the copy's exact size, and one close-up per mark that is small
/// enough to gain from one.
///
/// # Errors
/// [`FeedbackError::Invalid`] when the drawings are too long to draw within
/// [`MAX_DRAW_OPS`]; [`FeedbackError::Storage`] when the copy cannot be
/// decoded or a result cannot be encoded.
pub(super) fn render(
    image: &CanonicalImage,
    annotations: &[FeedbackAnnotation],
) -> Result<Vec<RenderedImage>, FeedbackError> {
    let size = (image.meta.width, image.meta.height);
    let marks = marks(size, annotations);
    let canvases = plan(size, &marks);
    check_cost(&canvases, &marks)?;
    let source = canonical::decode(image)?;
    let storage = |err: anyhow::Error| FeedbackError::Storage(err.context("encode image"));

    let mut out = Vec::with_capacity(canvases.len());
    for canvas in &canvases {
        let mut pixels = match canvas.closeup {
            Some((_, (x, y, w, h))) => scaled(&source.crop_imm(x, y, w, h), canvas.scale),
            None => source.to_rgba8(),
        };
        for (n, mark) in marks.iter().filter(|(n, _)| canvas.draws(*n)) {
            draw_mark(
                &mut pixels,
                mark,
                *n,
                canvas.origin(),
                canvas.scale,
                canvas.unit(),
            );
        }
        out.push(match canvas.closeup {
            None => {
                let (bytes, ext) = encode_marked(&pixels, &image.meta.mime).map_err(storage)?;
                RenderedImage {
                    name: format!("marked-image.{ext}"),
                    label: if marks.is_empty() {
                        "The annotated image".to_string()
                    } else {
                        "Marked image (every note's mark, numbered)".to_string()
                    },
                    note: None,
                    bytes,
                }
            }
            Some((n, _)) => RenderedImage {
                name: format!("note-{n}-closeup.jpg"),
                label: format!("Close-up of note {n}"),
                note: Some(n),
                bytes: canonical::encode_jpeg(
                    &canonical::flatten(&DynamicImage::ImageRgba8(pixels)),
                    CLOSEUP_JPEG_QUALITY,
                )
                .map_err(storage)?,
            },
        });
    }
    Ok(out)
}

/// The marked copy in the copy's format at the copy's size: PNG when the copy
/// is a PNG (transparency kept) and fits, else JPEG down the quality ladder.
/// Falls back to the smallest JPEG; the bot would then shrink it for the
/// model, which it says in the prompt.
fn encode_marked(pixels: &RgbaImage, mime: &str) -> anyhow::Result<(Vec<u8>, &'static str)> {
    if mime == "image/png" {
        let png = canonical::encode_png(pixels)?;
        if png.len() <= MARKED_MAX_BYTES {
            return Ok((png, "png"));
        }
    }
    let rgb = canonical::flatten(&DynamicImage::ImageRgba8(pixels.clone()));
    let mut smallest = Vec::new();
    for quality in MARKED_JPEG_LADDER {
        smallest = canonical::encode_jpeg(&rgb, quality)?;
        if smallest.len() <= MARKED_MAX_BYTES {
            break;
        }
    }
    Ok((smallest, "jpg"))
}

/// The overview first, then a close-up per mark that gains from one.
fn plan((width, height): (u32, u32), marks: &[(usize, Mark)]) -> Vec<Canvas> {
    let mut canvases = vec![Canvas {
        closeup: None,
        scale: 1.0,
        size: (width, height),
    }];
    for (n, mark) in marks {
        let Some(region) = closeup_region(mark_bounds(mark), width, height) else {
            continue;
        };
        let (_, _, w, h) = region;
        let long = w.max(h).max(1);
        let target = long.clamp(CLOSEUP_MIN_EDGE, CLOSEUP_MAX_EDGE);
        let scale = (f64::from(target) / f64::from(long)).min(CLOSEUP_MAX_UPSCALE);
        canvases.push(Canvas {
            closeup: Some((*n, region)),
            scale,
            size: scaled_size(w, h, scale),
        });
    }
    canvases
}

/// Refuses drawings whose lines would take too long to stamp, naming the
/// costliest note. Uses the same stamp spacing and disc sizes as
/// [`polyline`], on the canvases the marks are actually drawn on.
fn check_cost(canvases: &[Canvas], marks: &[(usize, Mark)]) -> Result<(), FeedbackError> {
    let mut total = 0.0;
    let mut worst = (0.0, 0);
    for (n, mark) in marks {
        let Mark::Area(strokes) = mark else { continue };
        let mut cost = 0.0;
        for canvas in canvases.iter().filter(|canvas| canvas.draws(*n)) {
            let unit = canvas.unit();
            for stroke in strokes {
                let length: f64 = stroke
                    .windows(2)
                    .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
                    .sum::<f64>()
                    * canvas.scale;
                for radius in [STROKE_HALO * unit, STROKE_INK * unit] {
                    let step = (radius * 0.5).max(0.5);
                    let stamps = length / step + stroke.len() as f64 + 1.0;
                    cost += stamps * (2.0 * radius + 2.0).powi(2);
                }
            }
        }
        total += cost;
        if cost > worst.0 {
            worst = (cost, *n);
        }
    }
    if total > MAX_DRAW_OPS {
        return Err(FeedbackError::Invalid(format!(
            "Drawing {} is too long to draw for the agent. Simplify it or split it into smaller drawings.",
            worst.1
        )));
    }
    Ok(())
}

/// Source region for a close-up, `(x, y, width, height)` in displayed-image
/// pixels, kept inside the image. `None` when it would cover most of the
/// image anyway.
fn closeup_region(
    (x0, y0, x1, y1): (f64, f64, f64, f64),
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (iw, ih) = (f64::from(width), f64::from(height));
    let (bw, bh) = (x1 - x0, y1 - y0);
    let pad = (bw.max(bh) * 0.25).max(32.0);
    let floor = CLOSEUP_MIN_SIDE.max(iw.min(ih) * 0.3);
    let w = (bw + 2.0 * pad).max(floor).min(iw);
    let h = (bh + 2.0 * pad).max(floor).min(ih);
    if w * h >= CLOSEUP_MAX_COVERAGE * iw * ih {
        return None;
    }
    let cx = f64::midpoint(x0, x1);
    let cy = f64::midpoint(y0, y1);
    let left = (cx - w / 2.0).clamp(0.0, iw - w);
    let top = (cy - h / 2.0).clamp(0.0, ih - h);
    let (w, h) = (w.round().max(1.0), h.round().max(1.0));
    Some((
        clamp_u32(left.round()).min(width.saturating_sub(1)),
        clamp_u32(top.round()).min(height.saturating_sub(1)),
        clamp_u32(w).min(width),
        clamp_u32(h).min(height),
    ))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to u32 range first
pub(super) fn clamp_u32(value: f64) -> u32 {
    value.clamp(0.0, f64::from(u32::MAX)) as u32
}

fn scaled_size(width: u32, height: u32, scale: f64) -> (u32, u32) {
    (
        clamp_u32((f64::from(width) * scale).round()).max(1),
        clamp_u32((f64::from(height) * scale).round()).max(1),
    )
}

/// Resizes by `scale`, nearest neighbour when enlarging so pixels stay
/// honest, with transparency flattened onto white (close-ups are JPEG).
fn scaled(pixels: &DynamicImage, scale: f64) -> RgbaImage {
    let (width, height) = scaled_size(pixels.width(), pixels.height(), scale);
    let resized = if width == pixels.width() && height == pixels.height() {
        pixels.clone()
    } else {
        let filter = if scale > 1.0 {
            FilterType::Nearest
        } else {
            FilterType::Triangle
        };
        pixels.resize_exact(width, height, filter)
    };
    DynamicImage::ImageRgb8(canonical::flatten(&resized)).to_rgba8()
}

/// Size of one drawing unit: marks scale with the picture, not the source.
fn unit_for(width: u32, height: u32) -> f64 {
    (f64::from(width.max(height)) / 400.0).max(2.0)
}

/// Line radii of a drawn area, in units: a white halo under red ink.
const STROKE_HALO: f64 = 2.6;
const STROKE_INK: f64 = 1.6;

/// Draws one mark. `origin` (source px) and `scale` map source pixels onto
/// `canvas`.
fn draw_mark(
    canvas: &mut RgbaImage,
    mark: &Mark,
    n: usize,
    origin: (f64, f64),
    scale: f64,
    unit: f64,
) {
    let map = |(x, y): (f64, f64)| ((x - origin.0) * scale, (y - origin.1) * scale);
    match mark {
        Mark::Pin(point) => {
            let (x, y) = map(*point);
            ring(canvas, (x, y), 8.0 * unit, 3.5 * unit, WHITE);
            ring(canvas, (x, y), 7.0 * unit, 4.5 * unit, RED);
            badge(canvas, (x + 12.0 * unit, y - 12.0 * unit), n, unit);
        }
        Mark::Area(strokes) => {
            let mapped: Vec<Vec<(f64, f64)>> = strokes
                .iter()
                .map(|stroke| stroke.iter().copied().map(map).collect())
                .collect();
            for stroke in &mapped {
                polyline(canvas, stroke, STROKE_HALO * unit, WHITE);
            }
            for stroke in &mapped {
                polyline(canvas, stroke, STROKE_INK * unit, RED);
            }
            let (x0, y0, _, _) = mark_bounds(&Mark::Area(mapped));
            badge(canvas, (x0 - 9.0 * unit, y0 - 9.0 * unit), n, unit);
        }
    }
}

fn plot(canvas: &mut RgbaImage, x: i64, y: i64, color: Rgba<u8>) {
    if let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y))
        && x < canvas.width()
        && y < canvas.height()
    {
        canvas.put_pixel(x, y, color);
    }
}

#[allow(clippy::cast_possible_truncation)] // canvas coordinates are small
fn span(center: f64, radius: f64) -> std::ops::RangeInclusive<i64> {
    ((center - radius).floor() as i64)..=((center + radius).ceil() as i64)
}

/// Pixels between `inner` and `outer` radius (`inner` 0 fills a disc).
fn ring(canvas: &mut RgbaImage, (cx, cy): (f64, f64), outer: f64, inner: f64, color: Rgba<u8>) {
    for y in span(cy, outer) {
        for x in span(cx, outer) {
            #[allow(clippy::cast_precision_loss)] // canvas coordinates are small
            let distance = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
            if distance <= outer && distance >= inner {
                plot(canvas, x, y, color);
            }
        }
    }
}

fn polyline(canvas: &mut RgbaImage, points: &[(f64, f64)], radius: f64, color: Rgba<u8>) {
    let Some(first) = points.first() else { return };
    ring(canvas, *first, radius, 0.0, color);
    for pair in points.windows(2) {
        let ((ax, ay), (bx, by)) = (pair[0], pair[1]);
        let length = (bx - ax).hypot(by - ay);
        let steps = (length / (radius * 0.5).max(0.5)).ceil().max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bounded by length
        let count = steps as u32;
        for step in 1..=count {
            let t = f64::from(step) / steps;
            ring(
                canvas,
                (ax + (bx - ax) * t, ay + (by - ay) * t),
                radius,
                0.0,
                color,
            );
        }
    }
}

/// 3×5 bitmap digits, one row per byte, high bit on the left.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// A red disc with the note number in white, kept inside the canvas.
fn badge(canvas: &mut RgbaImage, (cx, cy): (f64, f64), n: usize, unit: f64) {
    let digits: Vec<usize> = n
        .to_string()
        .bytes()
        .map(|b| usize::from(b - b'0'))
        .collect();
    let cell = (unit * 1.8).round().max(2.0);
    #[allow(clippy::cast_precision_loss)] // at most a few digits
    let count = digits.len() as f64;
    let text_w = (count * 3.0 + (count - 1.0)) * cell;
    let text_h = 5.0 * cell;
    let radius = text_w.max(text_h) / 2.0 + 1.5 * cell;
    let halo = radius + 0.8 * unit;
    let max_x = (f64::from(canvas.width()) - halo).max(halo);
    let max_y = (f64::from(canvas.height()) - halo).max(halo);
    let (cx, cy) = (cx.clamp(halo, max_x), cy.clamp(halo, max_y));
    ring(canvas, (cx, cy), halo, 0.0, WHITE);
    ring(canvas, (cx, cy), radius, 0.0, RED);
    let left = cx - text_w / 2.0;
    let top = cy - text_h / 2.0;
    for (index, digit) in digits.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)] // at most a few digits
        let x0 = left + index as f64 * 4.0 * cell;
        for (row, bits) in DIGITS[*digit].iter().enumerate() {
            for col in 0u8..3 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)] // 3×5 cells
                let (px, py) = (x0 + f64::from(col) * cell, top + row as f64 * cell);
                fill_rect(canvas, (px, py), cell, WHITE);
            }
        }
    }
}

#[allow(clippy::cast_possible_truncation)] // canvas coordinates are small
fn fill_rect(canvas: &mut RgbaImage, (x, y): (f64, f64), size: f64, color: Rgba<u8>) {
    let (x0, y0) = (x.round() as i64, y.round() as i64);
    let extent = size.round() as i64;
    for yy in y0..y0 + extent {
        for xx in x0..x0 + extent {
            plot(canvas, xx, yy, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::artifact_feedback::canonical::CanonicalMeta;

    #[test]
    fn closeups_stay_inside_the_image_at_edges_and_skip_large_areas() {
        // A pin in the bottom-right corner: the region slides inside.
        let (x, y, w, h) = closeup_region((1999.0, 999.0, 1999.0, 999.0), 2000, 1000).unwrap();
        assert!(x + w <= 2000 && y + h <= 1000, "{x} {y} {w} {h}");
        assert_eq!((x + w, y + h), (2000, 1000));
        // A pin at the top-left corner.
        let (x, y, _, _) = closeup_region((0.0, 0.0, 0.0, 0.0), 2000, 1000).unwrap();
        assert_eq!((x, y), (0, 0));
        // An area over most of the image gains nothing from a close-up.
        assert!(closeup_region((10.0, 10.0, 1990.0, 990.0), 2000, 1000).is_none());
        // A tiny image: the region is the whole image, so no close-up.
        assert!(closeup_region((5.0, 5.0, 5.0, 5.0), 40, 30).is_none());
    }

    #[test]
    fn marks_are_drawn_where_the_fractions_point() {
        let pixels = image::RgbImage::from_pixel(800, 400, image::Rgb([0, 0, 0]));
        let bytes = canonical::encode_jpeg(&pixels, 90).unwrap();
        let image = CanonicalImage {
            meta: CanonicalMeta {
                version: canonical::CANONICAL_VERSION,
                source_sha256: "0".repeat(64),
                sha256: "1".repeat(64),
                mime: "image/jpeg".to_string(),
                width: 800,
                height: 400,
                source_format: "JPEG".to_string(),
                source_bytes: 1000,
                source_width: 800,
                source_height: 400,
                source_orientation: 1,
            },
            bytes,
        };
        let annotation = |target| FeedbackAnnotation {
            intent: super::super::FeedbackIntent::Change,
            comment: "x".to_string(),
            target,
            media: Vec::new(),
        };
        let rendered = render(
            &image,
            &[annotation(FeedbackTarget::ImagePin {
                point: FeedbackPoint { x: 0.25, y: 0.5 },
            })],
        )
        .unwrap();
        assert_eq!(rendered.len(), 2, "overview plus one close-up");
        let overview = image::load_from_memory(&rendered[0].bytes)
            .unwrap()
            .to_rgb8();
        // Drawn on the copy at its own size.
        assert_eq!(overview.dimensions(), (800, 400));
        // 10.5 px left of the pin is inside the red ring (4.5 to 7 units of
        // 2 px); the pin's own pixel stays visible.
        let on_ring = overview.get_pixel(200 - 11, 200);
        assert!(on_ring[0] > 150 && on_ring[1] < 120, "{on_ring:?}");
        let centre = overview.get_pixel(200, 200);
        assert!(centre[0] < 60, "{centre:?}");
        assert_eq!(rendered[1].note, Some(1));
        assert_eq!(rendered[1].name, "note-1-closeup.jpg");
    }
}
