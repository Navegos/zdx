//! The canonical copy of an image artifact: one small, upright JPEG or PNG
//! made from the original file, used for the Mini App preview, as the frame
//! annotation coordinates refer to, and as the picture the agent gets. The
//! original file is never changed.
//!
//! Browsers disagree on EXIF orientation (Chromium ignores it in WebP, for
//! one), and a phone does not need a full-size original to place a pin.
//! So the engine makes the copy once, the app shows it as is (it has no EXIF
//! to reinterpret), and marks are fractions of exactly these pixels.
//!
//! Identity: `source_sha256` is the SHA-256 of the original bytes (the
//! artifact version), always computed here from the bytes decoded. `sha256`
//! is the SHA-256 of the copy's bytes. Copies are cached under
//! `$ZDX_HOME/cache/artifact-images/` by source hash and [`CANONICAL_VERSION`],
//! so the copy of an earlier version stays available to draw marks on after
//! the file changes. The cache is content-addressed; a caller must resolve the
//! artifact for the thread before asking for a copy.
//!
//! Large originals (camera photos included) are shrunk automatically; the
//! limits on originals, [`MAX_SOURCE_BYTES`] and [`MAX_SOURCE_PIXELS`], only
//! bound what is read and decoded. They are checked before reading and
//! before decoding, and again for cached copies.
//!
//! Memory: making a copy holds the original's bytes (at most 25 MiB), its
//! decoded pixels (at most [`MAX_DECODED_BYTES`]), and the decoder's working
//! buffers, which for some encodings (progressive JPEG, CMYK JPEG, lossy WebP
//! with alpha) are about three times the decoded output: measured near 480 MB
//! of heap for one 40-megapixel original. The shrink is a box filter that
//! writes the small output directly, so it adds little. At most
//! [`MAX_NORMALIZATIONS`] copies are made at once in the whole process, for
//! every caller (preview and feedback alike), and a slot is held until the
//! decode and shrink finish, whoever was waiting for the result.

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, RgbImage, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CurrentVersion, FeedbackError, hex, read_version};
use crate::config::paths;
use crate::core::artifacts::{self, ArtifactKind};

/// Bumped whenever the copy's pixels would change for the same original:
/// a new version makes new copies, and drafts made on an older copy are
/// refused instead of being drawn on a different frame.
/// 2: box-filter shrink (`thumbnail`) instead of a triangle filter.
pub const CANONICAL_VERSION: u32 = 2;
/// Copies made at once in the process (decode and shrink), for every caller.
pub const MAX_NORMALIZATIONS: usize = 2;
/// How long a caller waits for a free slot before [`FeedbackError::Busy`].
const SLOT_WAIT: Duration = Duration::from_mins(1);
/// Long edge of the copy. Matches the bot's inline image edge, so the marked
/// copy reaches the model without being resized again.
pub const CANONICAL_MAX_EDGE: u32 = 1568;
/// Byte budget of the copy. Leaves room under the bot's 1 MiB inline limit
/// for the marked copy, which is re-encoded with the marks drawn on it.
pub const CANONICAL_MAX_BYTES: usize = 768 * 1024;
/// Largest original that can be annotated, in bytes (25 MiB). Larger
/// originals are not a problem for the copy (it is always small); this
/// bounds what is read, hashed, and decoded. Image-annotation only: attached
/// note images and other surfaces keep their own limits.
pub const MAX_SOURCE_BYTES: u64 = 25 * 1024 * 1024;
/// Largest original that can be annotated, in pixels (40 megapixels, a large
/// camera photo), checked from the header before anything is decoded
/// (orientation does not change it).
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Largest decoded output: [`MAX_SOURCE_PIXELS`] of 8-bit RGBA (160 MB). A
/// 16-bit or wider image near the pixel limit is refused instead. This
/// bounds the output buffer only, not peak memory: decoder working buffers
/// come on top (see the module docs).
pub const MAX_DECODED_BYTES: u64 = MAX_SOURCE_PIXELS * 4;
/// JPEG qualities tried highest first, before shrinking further.
const JPEG_LADDER: [u8; 3] = [90, 80, 70];

/// What the canonical copy is and where it came from (also its cache record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalMeta {
    pub version: u32,
    /// SHA-256 of the original file bytes.
    pub source_sha256: String,
    /// SHA-256 of the copy's bytes.
    pub sha256: String,
    /// `image/jpeg`, or `image/png` when the image uses transparency.
    pub mime: String,
    /// The copy's size: upright, at most [`CANONICAL_MAX_EDGE`] on its long edge.
    pub width: u32,
    pub height: u32,
    /// `PNG`, `JPEG`, or `WebP`.
    pub source_format: String,
    /// The original's length in bytes. Required: cache records without it
    /// predate the source limits and are not used.
    pub source_bytes: u64,
    /// The original's size as stored, before EXIF orientation.
    pub source_width: u32,
    pub source_height: u32,
    /// The original's EXIF orientation tag (1 means none), applied to the copy.
    pub source_orientation: u8,
}

impl CanonicalMeta {
    /// The original's upright size (EXIF orientation applied).
    #[must_use]
    pub fn source_upright(&self) -> (u32, u32) {
        if self.source_orientation >= 5 {
            (self.source_height, self.source_width)
        } else {
            (self.source_width, self.source_height)
        }
    }

    /// File extension of the copy.
    #[must_use]
    pub fn ext(&self) -> &'static str {
        if self.mime == "image/png" {
            "png"
        } else {
            "jpg"
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalImage {
    pub meta: CanonicalMeta,
    pub bytes: Vec<u8>,
}

/// The canonical copy of an image artifact of `thread_id`, made on first use.
///
/// # Errors
/// [`FeedbackError::ArtifactNotFound`] when the path is not an artifact of
/// the thread; [`FeedbackError::UnsupportedArtifact`] for formats other than
/// PNG, JPEG, and WebP; [`FeedbackError::Invalid`] when the original is over
/// a limit or cannot be read.
pub fn canonical_for_artifact(
    thread_id: &str,
    artifact_path: &str,
) -> Result<CanonicalImage, FeedbackError> {
    let (path, _mime) = artifacts::resolve_artifact_for_download(thread_id, artifact_path)
        .map_err(|_resolve| FeedbackError::ArtifactNotFound)?;
    if artifacts::kind_for_path(&path) != ArtifactKind::Image {
        return Err(FeedbackError::UnsupportedArtifact("this kind of"));
    }
    check_source_size(&path)?;
    let current = read_version(&path).map_err(|_read| FeedbackError::ArtifactNotFound)?;
    canonical_for_version(&current)
}

/// Refuses an original over [`MAX_SOURCE_BYTES`] from its length, before it
/// is read or hashed. A file that grows after this check is caught by
/// [`canonical_for_version`], which checks the bytes it got.
///
/// # Errors
/// [`FeedbackError::Invalid`] when the file is too large;
/// [`FeedbackError::ArtifactNotFound`] when it cannot be read.
pub(super) fn check_source_size(path: &std::path::Path) -> Result<(), FeedbackError> {
    let length = fs::metadata(path)
        .map_err(|_err| FeedbackError::ArtifactNotFound)?
        .len();
    if length > MAX_SOURCE_BYTES {
        return Err(too_large());
    }
    Ok(())
}

/// The copy for a version [`read_version`] read: its hash was computed from
/// exactly these bytes.
pub(super) fn canonical_for_version(
    current: &CurrentVersion,
) -> Result<CanonicalImage, FeedbackError> {
    let bytes = current
        .bytes
        .as_deref()
        .filter(|bytes| bytes.len() as u64 <= MAX_SOURCE_BYTES)
        .ok_or_else(too_large)?;
    canonical_for_hashed(&current.sha, bytes)
}

fn too_large() -> FeedbackError {
    FeedbackError::Invalid(
        "This image is larger than 25 MiB, too large to annotate. Images up to 25 MiB and 40 megapixels are annotated on an optimized copy."
            .to_string(),
    )
}

fn too_many_pixels(width: u32, height: u32) -> FeedbackError {
    FeedbackError::Invalid(format!(
        "This image is {width}×{height} px, more than 40 megapixels, too many to annotate. Images up to 25 MiB and 40 megapixels are annotated on an optimized copy."
    ))
}

/// Whether an original this size may be annotated. Applied to new copies
/// (from the file length and the header) and to cached ones, so a copy made
/// under an earlier, looser policy is never served or drawn on.
fn within_source_limits(bytes: u64, width: u32, height: u32) -> bool {
    bytes <= MAX_SOURCE_BYTES && u64::from(width) * u64::from(height) <= MAX_SOURCE_PIXELS
}

/// The copy for these original bytes, from the cache or made now and cached.
/// The source hash is computed here, so the cache entry always names the
/// bytes it was made from.
///
/// # Errors
/// As [`canonical_for_artifact`]; [`FeedbackError::Busy`] when no slot frees
/// up in time. A cache that cannot be written is not an error (the copy is
/// returned anyway).
pub fn canonical_from_bytes(bytes: &[u8]) -> Result<CanonicalImage, FeedbackError> {
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(too_large());
    }
    canonical_for_hashed(&hex(&Sha256::digest(bytes)), bytes)
}

/// `source_sha256` must be the SHA-256 of `bytes`: callers in this module
/// hash the bytes themselves or pass the hash [`read_version`] computed from
/// the same read.
fn canonical_for_hashed(
    source_sha256: &str,
    bytes: &[u8],
) -> Result<CanonicalImage, FeedbackError> {
    if let Some(cached) = cached(source_sha256) {
        return Ok(cached);
    }
    let _slot = NORMALIZATIONS.acquire(SLOT_WAIT)?;
    // Another caller may have made it while this one waited.
    if let Some(cached) = cached(source_sha256) {
        return Ok(cached);
    }
    #[cfg(any(test, feature = "testing"))]
    test_hooks::wait_while_paused(source_sha256);
    let made = normalize(source_sha256, bytes)?;
    if let Err(err) = store(&made) {
        tracing::warn!(error = %format!("{err:#}"), "Failed to cache canonical image copy");
    }
    Ok(made)
}

/// A counting gate: at most `max` holders at once, others wait up to a
/// deadline. One process-wide instance ([`NORMALIZATIONS`]) bounds image
/// normalization for every caller.
struct Gate {
    max: usize,
    running: Mutex<usize>,
    freed: Condvar,
}

/// Every normalization in the process, preview and feedback alike.
static NORMALIZATIONS: Gate = Gate::new(MAX_NORMALIZATIONS);

/// A held place in a [`Gate`], released on drop. Taken on the blocking thread
/// that does the work, so it lives exactly as long as the decode and shrink,
/// even when the request that asked for them is gone.
struct Slot<'a>(&'a Gate);

impl Gate {
    const fn new(max: usize) -> Self {
        Self {
            max,
            running: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    fn acquire(&self, wait: Duration) -> Result<Slot<'_>, FeedbackError> {
        let deadline = Instant::now() + wait;
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *running >= self.max {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(FeedbackError::Busy);
            }
            running = self
                .freed
                .wait_timeout(running, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        *running += 1;
        Ok(Slot(self))
    }

    #[cfg(any(test, feature = "testing"))]
    fn running(&self) -> usize {
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut running = self
            .0
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *running = running.saturating_sub(1);
        self.0.freed.notify_one();
    }
}

/// Copies being made right now, across the process.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn normalizations_running() -> usize {
    NORMALIZATIONS.running()
}

/// Test-only pause: a normalization of a paused source holds its slot and
/// waits, so tests can observe the slot limit and disconnects.
#[cfg(any(test, feature = "testing"))]
pub mod test_hooks {
    use std::collections::HashSet;
    use std::sync::{Condvar, Mutex};

    static PAUSED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    static RESUMED: Condvar = Condvar::new();

    /// Normalizations of `source_sha256` wait, holding their slot, until the
    /// guard drops.
    ///
    /// # Panics
    /// When a test holding the pause lock panicked.
    #[must_use]
    pub fn pause(source_sha256: &str) -> PauseGuard {
        let key = source_sha256.to_ascii_lowercase();
        PAUSED
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(key.clone());
        PauseGuard(key)
    }

    pub struct PauseGuard(String);

    impl Drop for PauseGuard {
        fn drop(&mut self) {
            if let Some(set) = PAUSED.lock().unwrap().as_mut() {
                set.remove(&self.0);
            }
            RESUMED.notify_all();
        }
    }

    pub(super) fn wait_while_paused(source_sha256: &str) {
        let key = source_sha256.to_ascii_lowercase();
        let mut paused = PAUSED.lock().unwrap();
        while paused.as_ref().is_some_and(|set| set.contains(&key)) {
            paused = RESUMED.wait(paused).unwrap();
        }
    }
}

/// The cached copy of the original with this hash, if one was made (for
/// example by the preview) and is intact.
#[must_use]
pub fn cached(source_sha256: &str) -> Option<CanonicalImage> {
    let (meta_path, bytes_path) = cache_paths(source_sha256)?;
    let meta: CanonicalMeta = serde_json::from_slice(&fs::read(meta_path).ok()?).ok()?;
    let bytes = fs::read(bytes_path).ok()?;
    let intact = meta.version == CANONICAL_VERSION
        && meta.source_sha256.eq_ignore_ascii_case(source_sha256)
        && within_source_limits(meta.source_bytes, meta.source_width, meta.source_height)
        && hex(&Sha256::digest(&bytes)) == meta.sha256;
    intact.then_some(CanonicalImage { meta, bytes })
}

fn cache_paths(source_sha256: &str) -> Option<(PathBuf, PathBuf)> {
    if source_sha256.len() != 64 || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let dir = paths::zdx_home().join("cache").join("artifact-images");
    let stem = format!(
        "{}-v{CANONICAL_VERSION}",
        source_sha256.to_ascii_lowercase()
    );
    Some((
        dir.join(format!("{stem}.json")),
        dir.join(format!("{stem}.img")),
    ))
}

/// Writes bytes then meta, each through a temporary file and a rename, so a
/// reader never sees a half-written copy (and the hash check catches the rest).
fn store(image: &CanonicalImage) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let (meta_path, bytes_path) =
        cache_paths(&image.meta.source_sha256).context("invalid source hash")?;
    let dir = meta_path.parent().context("cache dir")?;
    fs::create_dir_all(dir).context("create image cache dir")?;
    let write = |path: &PathBuf, bytes: &[u8]| -> anyhow::Result<()> {
        let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        fs::write(&tmp, bytes).context("write cache file")?;
        fs::rename(&tmp, path).inspect_err(|_err| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    };
    write(&bytes_path, &image.bytes)?;
    write(&meta_path, &serde_json::to_vec_pretty(&image.meta)?)?;
    Ok(())
}

/// Formats that can be annotated, typed by their bytes.
fn image_format(bytes: &[u8]) -> Result<(ImageFormat, &'static str), FeedbackError> {
    match infer::get(bytes).map(|kind| kind.mime_type()) {
        Some("image/png") => Ok((ImageFormat::Png, "PNG")),
        Some("image/jpeg") => Ok((ImageFormat::Jpeg, "JPEG")),
        Some("image/webp") => Ok((ImageFormat::WebP, "WebP")),
        Some("image/gif") => Err(FeedbackError::UnsupportedArtifact("GIF")),
        _ if looks_like_svg(bytes) => Err(FeedbackError::UnsupportedArtifact("SVG")),
        _ => Err(FeedbackError::UnsupportedArtifact("this kind of")),
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    String::from_utf8_lossy(head).contains("<svg")
}

/// Decodes the original within limits (pixel count from the header first,
/// decoded size, decoder held to the header size), shrinks it as stored,
/// then turns the small result upright, so no full-size rotated buffer is
/// ever made, and encodes it within [`CANONICAL_MAX_BYTES`].
fn normalize(source_sha256: &str, bytes: &[u8]) -> Result<CanonicalImage, FeedbackError> {
    let (format, format_name) = image_format(bytes)?;
    let unreadable = || FeedbackError::Invalid("The image file could not be read.".to_string());
    let reader = || image::ImageReader::with_format(Cursor::new(bytes), format);
    let (stored_width, stored_height) = reader().into_dimensions().map_err(|_err| unreadable())?;
    if !within_source_limits(bytes.len() as u64, stored_width, stored_height) {
        return Err(if bytes.len() as u64 > MAX_SOURCE_BYTES {
            too_large()
        } else {
            too_many_pixels(stored_width, stored_height)
        });
    }
    let mut limited = reader();
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(stored_width);
    limits.max_image_height = Some(stored_height);
    limited.limits(limits);
    let mut decoder = limited.into_decoder().map_err(|_err| unreadable())?;
    let orientation = orientation(&mut decoder, format)?;
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err(FeedbackError::Invalid(format!(
            "This image needs more than {} MB of memory to decode (16-bit colour?). Convert it to an 8-bit PNG or JPEG.",
            MAX_DECODED_BYTES / 1_000_000
        )));
    }
    let small = {
        let original = DynamicImage::from_decoder(decoder).map_err(|_err| unreadable())?;
        shrink(&original, CANONICAL_MAX_EDGE)
    };
    let mut upright = small;
    upright.apply_orientation(orientation);
    let (encoded, mime) = encode_within(upright, CANONICAL_MAX_BYTES)
        .map_err(|err| FeedbackError::Storage(err.context("encode canonical image")))?;
    let (width, height) = dimensions(&encoded).ok_or_else(unreadable)?;
    Ok(CanonicalImage {
        meta: CanonicalMeta {
            version: CANONICAL_VERSION,
            source_sha256: source_sha256.to_ascii_lowercase(),
            sha256: hex(&Sha256::digest(&encoded)),
            mime: mime.to_string(),
            width,
            height,
            source_format: format_name.to_string(),
            source_bytes: bytes.len() as u64,
            source_width: stored_width,
            source_height: stored_height,
            source_orientation: orientation.to_exif(),
        },
        bytes: encoded,
    })
}

/// The EXIF orientation, read the same way for every format. WebP writers
/// differ on the `Exif\0\0` header in front of the TIFF data; both count. An
/// EXIF block that cannot be read is refused rather than guessed.
fn orientation(
    decoder: &mut impl image::ImageDecoder,
    format: ImageFormat,
) -> Result<Orientation, FeedbackError> {
    let unreadable = |_err| {
        FeedbackError::Invalid(
            "The image's EXIF data could not be read, so it cannot be shown upright reliably."
                .to_string(),
        )
    };
    if format != ImageFormat::WebP {
        return decoder.orientation().map_err(unreadable);
    }
    let exif = decoder.exif_metadata().map_err(unreadable)?;
    Ok(exif
        .as_deref()
        .and_then(|chunk| {
            let tiff = chunk.strip_prefix(b"Exif\0\0").unwrap_or(chunk);
            Orientation::from_exif_chunk(tiff)
        })
        .unwrap_or(Orientation::NoTransforms))
}

/// At most `max_edge` on the long edge, 8 bits per channel; never enlarged.
/// A box filter (`thumbnail`): each output pixel averages the source pixels
/// it covers and is written straight into the small output, so no
/// full-width intermediate is made (a separable filter's first pass is as
/// wide as the original, with a 32-bit float per channel).
fn shrink(pixels: &DynamicImage, max_edge: u32) -> DynamicImage {
    let alpha = pixels.color().has_alpha();
    let (width, height) = (pixels.width(), pixels.height());
    let long = width.max(height).max(1);
    let resized = if long > max_edge {
        let scale = f64::from(max_edge) / f64::from(long);
        let fit =
            |side: u32| super::image_marks::clamp_u32((f64::from(side) * scale).round()).max(1);
        pixels.thumbnail_exact(fit(width), fit(height))
    } else {
        pixels.clone()
    };
    if alpha {
        DynamicImage::ImageRgba8(resized.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(resized.into_rgb8())
    }
}

/// PNG when some pixel is not opaque and the PNG fits, else JPEG down the
/// quality ladder (transparency flattened onto white); halves the size when
/// nothing fits. Terminates: a 1×1 JPEG is a few hundred bytes.
fn encode_within(
    mut pixels: DynamicImage,
    max_bytes: usize,
) -> anyhow::Result<(Vec<u8>, &'static str)> {
    loop {
        if let DynamicImage::ImageRgba8(rgba) = &pixels
            && rgba.pixels().any(|pixel| pixel[3] < 255)
        {
            let png = encode_png(rgba)?;
            if png.len() <= max_bytes {
                return Ok((png, "image/png"));
            }
        }
        let rgb = flatten(&pixels);
        for quality in JPEG_LADDER {
            let jpeg = encode_jpeg(&rgb, quality)?;
            if jpeg.len() <= max_bytes || (rgb.width() <= 1 && rgb.height() <= 1) {
                return Ok((jpeg, "image/jpeg"));
            }
        }
        let (width, height) = ((pixels.width() / 2).max(1), (pixels.height() / 2).max(1));
        pixels = pixels.thumbnail_exact(width, height);
    }
}

/// Transparency flattened onto white.
pub(super) fn flatten(pixels: &DynamicImage) -> RgbImage {
    if !pixels.color().has_alpha() {
        return pixels.to_rgb8();
    }
    let rgba = pixels.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = u16::from(pixel[3]);
        let blend = |channel: u8| -> u8 {
            let value = (u16::from(channel) * alpha + 255 * (255 - alpha)) / 255;
            u8::try_from(value).unwrap_or(u8::MAX)
        };
        out.put_pixel(
            x,
            y,
            image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
        );
    }
    out
}

pub(super) fn encode_jpeg(pixels: &RgbImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), quality)
        .encode_image(pixels)?;
    Ok(out)
}

pub(super) fn encode_png(pixels: &RgbaImage) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    pixels.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Decodes a copy for drawing on. Copies are small and made by this module.
pub(super) fn decode(image: &CanonicalImage) -> Result<DynamicImage, FeedbackError> {
    image::load_from_memory(&image.bytes).map_err(|err| {
        FeedbackError::Storage(anyhow::Error::new(err).context("decode canonical image"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_admits_its_limit_and_times_out_the_rest() {
        let gate = Gate::new(2);
        let first = gate.acquire(Duration::from_millis(10)).unwrap();
        let _second = gate.acquire(Duration::from_millis(10)).unwrap();
        assert_eq!(gate.running(), 2);
        assert!(matches!(
            gate.acquire(Duration::from_millis(50)),
            Err(FeedbackError::Busy)
        ));
        // A waiter gets the place as soon as a holder leaves.
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| gate.acquire(Duration::from_secs(10)).map(|_slot| ()));
            std::thread::sleep(Duration::from_millis(100));
            assert!(!waiter.is_finished(), "waits while the gate is full");
            drop(first);
            assert!(waiter.join().unwrap().is_ok());
        });
        assert_eq!(gate.running(), 1);
    }
}
