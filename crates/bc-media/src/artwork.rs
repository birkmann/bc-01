//! Cover art pipeline: decode once, produce three WebP sizes, a blurhash and a dominant colour.
//!
//! Replaces `services/library/artwork.py` (JPEG 700/128) per PLAN "Images": WebP at 128 / 320 /
//! 800 px, blurhash and dominant colour stored with the release.
//!
//! Resizing uses `fast_image_resize` (Lanczos3 on RGB8); sources are never upscaled. Alpha is
//! dropped (covers are opaque in practice).
//!
//! On-disk layout: `{art_dir}/{id/1000:04}/{id}_{thumb|medium|full}.webp`. The legacy layout
//! `cache/art/NNNN/{id}.jpg` + `{id}_thumb.jpg` is addressed by [`legacy_jpeg_path`].

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use sha2::{Digest, Sha256};

use crate::error::{MediaError, Result};

/// Output sizes (longest side, pixels).
pub const THUMB_PX: u32 = 128;
pub const MEDIUM_PX: u32 = 320;
pub const FULL_PX: u32 = 800;
/// WebP lossy quality.
pub const WEBP_QUALITY: f32 = 80.0;

/// One of the three stored renditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtSize {
    Thumb,
    Medium,
    Full,
}

impl ArtSize {
    pub const ALL: [ArtSize; 3] = [ArtSize::Thumb, ArtSize::Medium, ArtSize::Full];

    /// File-name / URL token.
    pub fn name(self) -> &'static str {
        match self {
            Self::Thumb => "thumb",
            Self::Medium => "medium",
            Self::Full => "full",
        }
    }
    /// Parse `thumb|medium|full`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "thumb" => Some(Self::Thumb),
            "medium" => Some(Self::Medium),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
    /// Bit in the sizes bitmask: 1 = thumb, 2 = medium, 4 = full.
    pub fn bit(self) -> u8 {
        match self {
            Self::Thumb => 1,
            Self::Medium => 2,
            Self::Full => 4,
        }
    }
    /// Longest side in pixels.
    pub fn px(self) -> u32 {
        match self {
            Self::Thumb => THUMB_PX,
            Self::Medium => MEDIUM_PX,
            Self::Full => FULL_PX,
        }
    }
}

/// Everything derived from one cover image.
#[derive(Debug, Clone)]
pub struct ProcessedArt {
    /// First 16 hex chars of the sha256 of the *source* bytes.
    pub hash: String,
    /// Source dimensions.
    pub width: u32,
    pub height: u32,
    /// Blurhash (4x3 components) of a 32x32 downscale.
    pub blurhash: String,
    /// Dominant colour `#rrggbb`, see [`dominant_color`].
    pub color: String,
    /// WebP, longest side <= 128.
    pub thumb: Vec<u8>,
    /// WebP, longest side <= 320.
    pub medium: Vec<u8>,
    /// WebP, longest side <= 800.
    pub full: Vec<u8>,
}

impl ProcessedArt {
    /// Bytes of one rendition.
    pub fn bytes(&self, size: ArtSize) -> &[u8] {
        match size {
            ArtSize::Thumb => &self.thumb,
            ArtSize::Medium => &self.medium,
            ArtSize::Full => &self.full,
        }
    }
    /// Bitmask of the non-empty renditions.
    pub fn bitmask(&self) -> u8 {
        ArtSize::ALL
            .iter()
            .filter(|s| !self.bytes(**s).is_empty())
            .fold(0, |m, s| m | s.bit())
    }
}

fn img_err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Image(e.to_string())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Fit `(w, h)` inside a `max x max` box preserving aspect, never upscaling.
fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
    if w <= max && h <= max {
        return (w, h);
    }
    let scale = f64::from(max) / f64::from(w.max(h));
    let nw = ((f64::from(w) * scale).round() as u32).max(1);
    let nh = ((f64::from(h) * scale).round() as u32).max(1);
    (nw.min(max), nh.min(max))
}

fn resize_rgb(resizer: &mut Resizer, src: &FirImage<'_>, w: u32, h: u32) -> Result<Vec<u8>> {
    if src.width() == w && src.height() == h {
        return Ok(src.buffer().to_vec());
    }
    let mut dst = FirImage::new(w, h, PixelType::U8x3);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    resizer.resize(src, &mut dst, &opts).map_err(img_err)?;
    Ok(dst.into_vec())
}

fn encode_webp(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    webp::Encoder::from_rgb(rgb, w, h)
        .encode(WEBP_QUALITY)
        .to_vec()
}

/// Decode `bytes` (jpeg / png / gif / webp / bmp) and derive all renditions and metadata.
pub fn process_cover(bytes: &[u8]) -> Result<ProcessedArt> {
    if bytes.is_empty() {
        return Err(MediaError::Image("empty image".into()));
    }
    let hash = hex(&Sha256::digest(bytes))[..16].to_string();
    let decoded = image::load_from_memory(bytes).map_err(img_err)?;
    let rgb = decoded.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return Err(MediaError::Image("zero-sized image".into()));
    }
    let src = FirImage::from_vec_u8(w, h, rgb.into_raw(), PixelType::U8x3).map_err(img_err)?;
    let mut resizer = Resizer::new();

    let mut out: [Vec<u8>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (slot, size) in out.iter_mut().zip(ArtSize::ALL) {
        let (tw, th) = fit(w, h, size.px());
        let pixels = resize_rgb(&mut resizer, &src, tw, th)?;
        *slot = encode_webp(&pixels, tw, th);
    }

    // 32x32 squash for blurhash and colour.
    let small = resize_rgb(&mut resizer, &src, 32, 32)?;
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for px in small.as_chunks::<3>().0 {
        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    let blurhash = blurhash::encode(4, 3, 32, 32, &rgba).map_err(img_err)?;
    let color = dominant_color(&small);

    let [thumb, medium, full] = out;
    Ok(ProcessedArt {
        hash,
        width: w,
        height: h,
        blurhash,
        color,
        thumb,
        medium,
        full,
    })
}

/// Dominant colour of tightly packed RGB8 pixels (intended for a 32x32 downscale).
///
/// Rule: quantise every pixel to 3 bits per channel (512 buckets). Pixels that are nearly black
/// (max channel < 24) are ignored. Each bucket scores `count * (0.15 + saturation)` where
/// saturation is `(max - min) / max` of the bucket's mean colour, so a populated saturated hue
/// beats a larger grey area but a mostly-grey cover still yields grey. The result is the mean
/// colour of the winning bucket's pixels as `#rrggbb`. If every pixel is ignored, the plain mean
/// of all pixels is returned.
pub fn dominant_color(rgb: &[u8]) -> String {
    #[derive(Default, Clone, Copy)]
    struct Bucket {
        n: u32,
        r: u64,
        g: u64,
        b: u64,
    }
    let mut buckets = [Bucket::default(); 512];
    let (mut tn, mut tr, mut tg, mut tb) = (0u64, 0u64, 0u64, 0u64);
    for px in rgb.as_chunks::<3>().0 {
        let (r, g, b) = (px[0], px[1], px[2]);
        tn += 1;
        tr += u64::from(r);
        tg += u64::from(g);
        tb += u64::from(b);
        if r.max(g).max(b) < 24 {
            continue;
        }
        let idx = (usize::from(r >> 5) << 6) | (usize::from(g >> 5) << 3) | usize::from(b >> 5);
        let bk = &mut buckets[idx];
        bk.n += 1;
        bk.r += u64::from(r);
        bk.g += u64::from(g);
        bk.b += u64::from(b);
    }
    let mut best: Option<(f64, [u8; 3])> = None;
    for bk in buckets.iter().filter(|b| b.n > 0) {
        let n = u64::from(bk.n);
        let mean = [(bk.r / n) as u8, (bk.g / n) as u8, (bk.b / n) as u8];
        let mx = f64::from(mean.iter().copied().max().unwrap_or(0));
        let mn = f64::from(mean.iter().copied().min().unwrap_or(0));
        let sat = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
        let score = f64::from(bk.n) * (0.15 + sat);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, mean));
        }
    }
    let [r, g, b] = match best {
        Some((_, c)) => c,
        None if tn > 0 => [(tr / tn) as u8, (tg / tn) as u8, (tb / tn) as u8],
        None => [0, 0, 0],
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

// ---------------------------------------------------------------------------
// On-disk layout
// ---------------------------------------------------------------------------

/// `{art_dir}/{id/1000:04}/{id}_{thumb|medium|full}.webp`.
pub fn art_file_path(art_dir: &Path, release_id: i64, size: ArtSize) -> PathBuf {
    bc_core::paths::shard_path(art_dir, release_id, &format!("_{}.webp", size.name()))
}

/// Legacy `cache/art/NNNN/{id}.jpg` (full) or `{id}_thumb.jpg`.
pub fn legacy_jpeg_path(art_dir: &Path, release_id: i64, thumb: bool) -> PathBuf {
    bc_core::paths::shard_path(
        art_dir,
        release_id,
        if thumb { "_thumb.jpg" } else { ".jpg" },
    )
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| MediaError::Write("art path has no parent".into()))?;
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("webp.{}.tmp", std::process::id()));
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(res?)
}

/// Write every non-empty rendition atomically (temp file + rename). Returns the sizes bitmask
/// (1 = thumb, 2 = medium, 4 = full).
pub fn write_art_files(art_dir: &Path, release_id: i64, art: &ProcessedArt) -> Result<u8> {
    let mut mask = 0u8;
    for size in ArtSize::ALL {
        let data = art.bytes(size);
        if data.is_empty() {
            continue;
        }
        write_atomic(&art_file_path(art_dir, release_id, size), data)?;
        mask |= size.bit();
    }
    Ok(mask)
}

/// The immutable `?v=` cache-busting value: 12 hex chars derived from the source hash and the
/// available sizes, so it changes when either does.
pub fn version_string(hash: &str, bitmask: u8) -> String {
    hex(&Sha256::digest(format!("{hash}:{bitmask}").as_bytes()))[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 128])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    fn webp_dims(data: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(data, image::ImageFormat::WebP).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn three_sizes_fit_and_keep_aspect() {
        let art = process_cover(&gradient_png(1000, 500)).unwrap();
        assert_eq!((art.width, art.height), (1000, 500));
        assert_eq!(webp_dims(&art.thumb), (128, 64));
        assert_eq!(webp_dims(&art.medium), (320, 160));
        assert_eq!(webp_dims(&art.full), (800, 400));
        assert_eq!(art.bitmask(), 7);
        assert_eq!(art.hash.len(), 16);
        assert!(!art.blurhash.is_empty());
        assert!(art.color.starts_with('#') && art.color.len() == 7);
    }

    #[test]
    fn small_sources_are_never_upscaled() {
        let art = process_cover(&gradient_png(100, 90)).unwrap();
        assert_eq!(webp_dims(&art.thumb), (100, 90));
        assert_eq!(webp_dims(&art.medium), (100, 90));
        assert_eq!(webp_dims(&art.full), (100, 90));
    }

    #[test]
    fn hash_is_over_source_bytes() {
        let a = gradient_png(64, 64);
        let b = solid_png(64, 64, [1, 2, 3]);
        assert_eq!(
            process_cover(&a).unwrap().hash,
            process_cover(&a).unwrap().hash
        );
        assert_ne!(
            process_cover(&a).unwrap().hash,
            process_cover(&b).unwrap().hash
        );
    }

    #[test]
    fn dominant_colour_of_solid_image() {
        let art = process_cover(&solid_png(64, 64, [200, 40, 40])).unwrap();
        assert_eq!(art.color, "#c82828");
    }

    #[test]
    fn saturated_colour_beats_larger_grey_area() {
        // 60% grey, 40% red.
        let mut px = Vec::new();
        for i in 0..1024 {
            if i < 614 {
                px.extend_from_slice(&[128, 128, 128]);
            } else {
                px.extend_from_slice(&[220, 20, 30]);
            }
        }
        assert_eq!(dominant_color(&px), "#dc141e");
        // all black falls back to the mean
        assert_eq!(dominant_color(&[0, 0, 0, 0, 0, 0]), "#000000");
    }

    #[test]
    fn rejects_garbage() {
        assert!(process_cover(b"not an image").is_err());
        assert!(process_cover(&[]).is_err());
    }

    #[test]
    fn decodes_jpeg_bmp_gif() {
        let img = image::RgbImage::from_pixel(40, 40, image::Rgb([10, 200, 10]));
        for fmt in [
            image::ImageFormat::Jpeg,
            image::ImageFormat::Bmp,
            image::ImageFormat::Gif,
        ] {
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, fmt).unwrap();
            let art = process_cover(&buf.into_inner()).unwrap();
            assert_eq!((art.width, art.height), (40, 40), "{fmt:?}");
        }
    }

    #[test]
    fn paths_are_sharded() {
        let dir = Path::new("/x/art");
        assert_eq!(
            art_file_path(dir, 12345, ArtSize::Medium),
            PathBuf::from("/x/art/0012/12345_medium.webp")
        );
        assert_eq!(
            art_file_path(dir, 7, ArtSize::Thumb),
            PathBuf::from("/x/art/0000/7_thumb.webp")
        );
        assert_eq!(
            legacy_jpeg_path(dir, 12345, false),
            PathBuf::from("/x/art/0012/12345.jpg")
        );
        assert_eq!(
            legacy_jpeg_path(dir, 12345, true),
            PathBuf::from("/x/art/0012/12345_thumb.jpg")
        );
    }

    #[test]
    fn write_files_and_version() {
        let tmp = tempfile::tempdir().unwrap();
        let art = process_cover(&gradient_png(400, 400)).unwrap();
        let mask = write_art_files(tmp.path(), 4321, &art).unwrap();
        assert_eq!(mask, 7);
        for s in ArtSize::ALL {
            let p = art_file_path(tmp.path(), 4321, s);
            assert_eq!(fs::read(&p).unwrap(), art.bytes(s));
        }
        // no temp files left behind
        let leftovers = fs::read_dir(tmp.path().join("0004"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let v = version_string(&art.hash, mask);
        assert_eq!(v.len(), 12);
        assert_ne!(v, version_string(&art.hash, 3));
        assert_ne!(v, version_string("other", mask));
        assert_eq!(v, version_string(&art.hash, mask));
    }
}
