//! Gallery copies of works cut into strips: pages that continue each other across a cut are
//! joined back into the picture before upload. Telegram keeps the pages as they were posted.
//!
//! Port of Vitrine's `tools/stitch_strips.py` with its calibrated thresholds. A true cut
//! continues the fine structure of the last column into the next strip about as well as the
//! picture's own neighbouring columns do. Pages that only share a layout (letterbox bars, app
//! chrome, card frames) also line up at their far edges, or jump in brightness at the would-be
//! seam. Joins are lossless (jpegtran when the JPEG blocks line up, otherwise lossless WebP or
//! PNG) and checked pixel for pixel. A page showing the same picture keeps whichever copy has
//! more pixels, then more bytes; every other page stays. Anything unexpected leaves the pages
//! as they were.
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};

/// (file name, content type, bytes), as the gallery client uploads them.
pub type Page = (String, &'static str, Vec<u8>);

const MIN_SEAM: f64 = 0.5;
const SEAM_RATIO: f64 = 0.75;
const MIN_DETAIL: f64 = 2.0;
const LAYOUT_MARGIN: f64 = 0.3;
const MAX_JUMP: f64 = 10.0;
const JUMP_RATIO: f64 = 4.0;
const MAX_OVERLAP: usize = 64;
const OVERLAP_CORR: f64 = 0.95;
const OVERLAP_DIFF: f64 = 4.0;
const SAME_ASPECT: f64 = 0.03;
const SAME_PICTURE: f64 = 6.0;
const WEBP_MAX: u32 = 16383;

/// The gallery's page list: strips joined, the better copy of a repeated whole picture kept.
pub fn stitch_pages(pages: Vec<Page>) -> Vec<Page> {
    match plan(&pages) {
        Ok(Some(plan)) => {
            let before = pages.len();
            let mut slots: Vec<Option<Page>> = pages.into_iter().map(Some).collect();
            let joined: Vec<Page> = plan
                .into_iter()
                .map(|out| match out {
                    Out::Keep(i) => slots[i].take().expect("each page is placed once"),
                    Out::New(page) => page,
                })
                .collect();
            tracing::info!(before, after = joined.len(), "图库切片已拼接");
            joined
        }
        Ok(None) => pages,
        Err(error) => {
            tracing::warn!("图库切片拼接失败，按原页入库: {error:#}");
            pages
        }
    }
}

enum Out {
    Keep(usize),
    New(Page),
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    /// Side by side.
    X,
    /// Stacked.
    Y,
}

struct Group {
    axis: Axis,
    chain: Vec<usize>,
    overlaps: Vec<usize>,
    full: Option<usize>,
    keep_full: bool,
    joined: Option<Page>,
}

fn plan(pages: &[Page]) -> Result<Option<Vec<Out>>> {
    if pages.len() < 2 {
        return Ok(None);
    }
    let mut formats = Vec::with_capacity(pages.len());
    let mut dims = Vec::with_capacity(pages.len());
    for (_, _, bytes) in pages {
        let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        formats.push(reader.format());
        dims.push(reader.into_dimensions()?);
    }
    // Strips share their height (or width when stacked); most works stop here undecoded.
    let shared = |side: fn(&(u32, u32)) -> u32| {
        let mut sides: Vec<u32> = dims.iter().map(side).collect();
        sides.sort_unstable();
        sides.windows(2).any(|pair| pair[0] == pair[1])
    };
    if !shared(|d| d.0) && !shared(|d| d.1) {
        return Ok(None);
    }
    // A joined GIF would lose its animation.
    if formats.contains(&Some(ImageFormat::Gif)) {
        return Ok(None);
    }
    let mut images = Vec::with_capacity(pages.len());
    let mut icc = Vec::with_capacity(pages.len());
    for (_, _, bytes) in pages {
        let mut decoder = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()?
            .into_decoder()?;
        if decoder.orientation()? != Orientation::NoTransforms {
            return Ok(None);
        }
        icc.push(decoder.icc_profile()?);
        images.push(DynamicImage::from_decoder(decoder)?);
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut used = HashSet::new();
    for axis in [Axis::X, Axis::Y] {
        let grays: Vec<Gray> = images.iter().map(|image| Gray::of(image, axis)).collect();
        for (chain, overlaps) in chains(&grays) {
            if chain.iter().any(|i| used.contains(i)) {
                // A grid of cuts in both directions: leave it alone.
                return Ok(None);
            }
            used.extend(chain.iter().copied());
            groups.push(Group {
                axis,
                chain,
                overlaps,
                full: None,
                keep_full: false,
                joined: None,
            });
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }

    for group in &mut groups {
        let strips: Vec<&Page> = group.chain.iter().map(|&i| &pages[i]).collect();
        let strip_images: Vec<&DynamicImage> = group.chain.iter().map(|&i| &images[i]).collect();
        let Some((bytes, content_type, ext, picture)) = stitch(
            &strips,
            &strip_images,
            &group.overlaps,
            group.axis,
            icc[group.chain[0]].clone(),
        )?
        else {
            return Ok(None);
        };
        let aspect = f64::from(picture.width()) / f64::from(picture.height());
        let mut best: Option<(usize, f64)> = None;
        for (i, page) in images.iter().enumerate() {
            let page_aspect = f64::from(page.width()) / f64::from(page.height());
            if used.contains(&i) || (page_aspect - aspect).abs() > SAME_ASPECT * aspect {
                continue;
            }
            let diff = same_picture(&picture, page);
            if diff <= SAME_PICTURE && best.is_none_or(|(_, d)| diff < d) {
                best = Some((i, diff));
            }
        }
        if let Some((i, _)) = best {
            used.insert(i);
            group.full = Some(i);
            let mine = (pixels(&picture), bytes.len());
            group.keep_full = mine <= (pixels(&images[i]), pages[i].2.len());
        }
        let stem = Path::new(&pages[group.chain[0]].0)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page")
            .to_string();
        group.joined = Some((format!("{stem}.{ext}"), content_type, bytes));
    }

    // Each group's kept picture takes the place of its first page; other pages stay.
    let owner: HashMap<usize, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(n, g)| g.chain.iter().copied().chain(g.full).map(move |i| (i, n)))
        .collect();
    let mut placed = HashSet::new();
    let mut out = Vec::new();
    for i in 0..pages.len() {
        match owner.get(&i) {
            None => out.push(Out::Keep(i)),
            Some(&n) if placed.insert(n) => {
                let group = &mut groups[n];
                out.push(match (group.keep_full, group.full) {
                    (true, Some(full)) => Out::Keep(full),
                    _ => Out::New(group.joined.take().expect("every group is joined")),
                });
            }
            Some(_) => {}
        }
    }
    Ok(Some(out))
}

fn pixels(image: &DynamicImage) -> u64 {
    u64::from(image.width()) * u64::from(image.height())
}

/// Grey levels as PIL's "L" conversion gives them, the image read so that strips continue
/// to the right (stacked strips are transposed).
struct Gray {
    w: usize,
    h: usize,
    px: Vec<f64>,
}

impl Gray {
    fn of(image: &DynamicImage, axis: Axis) -> Self {
        let rgb = image.to_rgb8();
        let (w, h) = (rgb.width() as usize, rgb.height() as usize);
        let luma = |p: &image::Rgb<u8>| {
            ((u32::from(p[0]) * 19595 + u32::from(p[1]) * 38470 + u32::from(p[2]) * 7471 + 0x8000)
                >> 16) as f64
        };
        match axis {
            Axis::X => Self {
                w,
                h,
                px: rgb.pixels().map(luma).collect(),
            },
            Axis::Y => {
                let mut px = vec![0.0; w * h];
                for (x, y, p) in rgb.enumerate_pixels() {
                    px[x as usize * h + y as usize] = luma(p);
                }
                Self { w: h, h: w, px }
            }
        }
    }

    fn col(&self, x: usize) -> Vec<f64> {
        (0..self.h).map(|y| self.px[y * self.w + x]).collect()
    }
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn std_dev(values: &[f64]) -> f64 {
    let m = mean(values);
    (values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

/// The column minus its 31-pixel moving average (edges repeated).
fn highpass(x: &[f64]) -> Vec<f64> {
    let n = x.len() as isize;
    let at = |i: isize| x[i.clamp(0, n - 1) as usize];
    let mut sum: f64 = (-15..=15).map(at).sum();
    let mut out = Vec::with_capacity(x.len());
    for i in 0..n {
        out.push(x[i as usize] - sum / 31.0);
        sum += at(i + 16) - at(i - 15);
    }
    out
}

/// Correlation of the fine structure along two columns; None when either is too flat to judge.
fn corr(x: &[f64], y: &[f64]) -> Option<f64> {
    let (a, b) = (highpass(x), highpass(y));
    let (sa, sb) = (std_dev(&a), std_dev(&b));
    if sa.min(sb) < MIN_DETAIL {
        return None;
    }
    let (ma, mb) = (mean(&a), mean(&b));
    let cov = a
        .iter()
        .zip(&b)
        .map(|(p, q)| (p - ma) * (q - mb))
        .sum::<f64>()
        / a.len() as f64;
    Some(cov / (sa * sb))
}

fn column_diff(a: &Gray, ax: usize, b: &Gray, bx: usize, k: usize) -> f64 {
    let mut total = 0.0;
    for y in 0..a.h {
        for dx in 0..k {
            total += (a.px[y * a.w + ax + dx] - b.px[y * b.w + bx + dx]).abs();
        }
    }
    total / (a.h * k) as f64
}

/// How `b` continues `a` to the right: Some((score, repeated columns)) for a seam.
fn seam(a: &Gray, b: &Gray) -> Option<(f64, usize)> {
    if a.h != b.h || a.w.min(b.w) < 3 {
        return None;
    }
    let (last, first) = (a.col(a.w - 1), b.col(0));
    let inside: Vec<f64> = [corr(&a.col(a.w - 2), &last), corr(&first, &b.col(1))]
        .into_iter()
        .flatten()
        .collect();
    if inside.is_empty() {
        return None;
    }
    let neighbour = mean(&inside);
    // Pages sharing a layout line up at every edge, while the two left edges of true strips
    // lie far apart in the picture.
    let layout = [corr(&a.col(0), &first), corr(&last, &b.col(b.w - 1))]
        .into_iter()
        .flatten()
        .fold(None, |best: Option<f64>, c| {
            Some(best.map_or(c, |b| b.max(c)))
        })
        .unwrap_or(0.0);
    // A repeated column matches clearly better than a neighbouring one would.
    let need = OVERLAP_CORR
        .max(neighbour + (1.0 - neighbour) / 2.0)
        .max(layout + LAYOUT_MARGIN);
    for k in 1..=MAX_OVERLAP.min(a.w - 1).min(b.w - 1) {
        if let Some(c) = corr(&a.col(a.w - k), &first) {
            if c >= need && column_diff(a, a.w - k, b, 0, k) <= OVERLAP_DIFF {
                return Some((c, k));
            }
        }
    }
    let across = corr(&last, &first)?;
    // Shared bars can carry the fine structure; the brightness must still not jump at the seam.
    let step = (column_diff(a, a.w - 2, a, a.w - 1, 1) + column_diff(b, 0, b, 1, 1)) / 2.0;
    if column_diff(a, a.w - 1, b, 0, 1) > MAX_JUMP + JUMP_RATIO * step {
        return None;
    }
    let need = MIN_SEAM
        .max(SEAM_RATIO * neighbour)
        .max(layout + LAYOUT_MARGIN);
    (across >= need).then_some((across, 0))
}

/// Ordered chains of pages that continue each other, with the columns each next one repeats.
fn chains(grays: &[Gray]) -> Vec<(Vec<usize>, Vec<usize>)> {
    let mut edges = Vec::new();
    for (i, a) in grays.iter().enumerate() {
        for (j, b) in grays.iter().enumerate() {
            if i != j {
                if let Some((score, k)) = seam(a, b) {
                    edges.push((score, i, j, k));
                }
            }
        }
    }
    edges.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then(y.1.cmp(&x.1))
            .then(y.2.cmp(&x.2))
            .then(y.3.cmp(&x.3))
    });
    let (mut next, mut prev, mut order) = (HashMap::new(), HashMap::new(), Vec::new());
    for (_, i, j, k) in edges {
        if next.contains_key(&i) || prev.contains_key(&j) {
            continue;
        }
        let mut head = i;
        while let Some(&p) = prev.get(&head) {
            head = p;
        }
        if head == j {
            continue;
        }
        next.insert(i, (j, k));
        prev.insert(j, i);
        order.push(i);
    }
    order
        .into_iter()
        .filter(|i| !prev.contains_key(i))
        .map(|start| {
            let (mut chain, mut overlaps) = (vec![start], Vec::new());
            while let Some(&(n, k)) = next.get(chain.last().expect("chain starts non-empty")) {
                chain.push(n);
                overlaps.push(k);
            }
            (chain, overlaps)
        })
        .collect()
}

/// Where each strip starts along the axis, and the joined size.
fn layout(sizes: &[(u32, u32)], overlaps: &[usize], axis: Axis) -> (Vec<u32>, u32, u32) {
    let along = |(w, h): (u32, u32)| if axis == Axis::X { w } else { h };
    let mut offsets = vec![0];
    for (&size, &k) in sizes.iter().zip(overlaps) {
        offsets.push(offsets.last().unwrap() + along(size) - k as u32);
    }
    let total = offsets.last().unwrap() + along(*sizes.last().unwrap());
    let (w, h) = sizes[0];
    match axis {
        Axis::X => (offsets, total, h),
        Axis::Y => (offsets, w, total),
    }
}

/// The joined picture: each strip continues where the previous one's repeated columns begin.
fn composite(images: &[DynamicImage], overlaps: &[usize], axis: Axis) -> DynamicImage {
    let sizes: Vec<(u32, u32)> = images.iter().map(|i| (i.width(), i.height())).collect();
    let (offsets, w, h) = layout(&sizes, overlaps, axis);
    let mut canvas = DynamicImage::new(w, h, images[0].color());
    for (image, offset) in images.iter().zip(offsets) {
        let (x, y) = if axis == Axis::X {
            (offset, 0)
        } else {
            (0, offset)
        };
        image::imageops::replace(&mut canvas, image, i64::from(x), i64::from(y));
    }
    canvas
}

type Joined = (Vec<u8>, &'static str, &'static str, DynamicImage);

/// The joined picture encoded losslessly, or None when it cannot be (CMYK JPEG).
fn stitch(
    strips: &[&Page],
    images: &[&DynamicImage],
    overlaps: &[usize],
    axis: Axis,
    icc: Option<Vec<u8>>,
) -> Result<Option<Joined>> {
    if strips
        .iter()
        .any(|(_, _, bytes)| jpeg_components(bytes) == Some(4))
    {
        return Ok(None);
    }
    let alpha = images.iter().any(|image| image.color().has_alpha());
    let converted: Vec<DynamicImage> = images
        .iter()
        .map(|image| {
            if alpha {
                DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(image.to_rgb8())
            }
        })
        .collect();
    let expected = composite(&converted, overlaps, axis);
    if !alpha && strips.iter().all(|(_, _, b)| jpeg_components(b).is_some()) {
        match jpegtran_join(strips, images, overlaps, axis) {
            Ok(Some(bytes)) => return Ok(Some((bytes, "image/jpeg", "jpg", expected))),
            Ok(None) => {}
            Err(error) => tracing::debug!("jpegtran 拼接不可用，改用无损 WebP: {error:#}"),
        }
    }
    let mut bytes = Vec::new();
    let (content_type, ext, format) = if expected.width().max(expected.height()) <= WEBP_MAX {
        let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        if let Some(icc) = icc {
            let _ = encoder.set_icc_profile(icc);
        }
        encoder.write_image(
            expected.as_bytes(),
            expected.width(),
            expected.height(),
            expected.color().into(),
        )?;
        ("image/webp", "webp", ImageFormat::WebP)
    } else {
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        if let Some(icc) = icc {
            let _ = encoder.set_icc_profile(icc);
        }
        encoder.write_image(
            expected.as_bytes(),
            expected.width(),
            expected.height(),
            expected.color().into(),
        )?;
        ("image/png", "png", ImageFormat::Png)
    };
    let decoded = image::load_from_memory_with_format(&bytes, format)?;
    let same = if alpha {
        decoded.to_rgba8().as_raw() == expected.as_bytes()
    } else {
        decoded.to_rgb8().as_raw() == expected.as_bytes()
    };
    if !same {
        bail!("无损编码回读与拼接结果不一致");
    }
    Ok(Some((bytes, content_type, ext, expected)))
}

/// Component count of a baseline or progressive JPEG, None for anything else.
fn jpeg_components(bytes: &[u8]) -> Option<u8> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        let length = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            return Some(bytes[i + 9]);
        }
        i += 2 + length;
    }
    None
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

/// A private temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "hanabi-stitch-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir)
            .with_context(|| format!("创建临时目录失败: {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(command: &mut Command) -> Result<Vec<u8>> {
    let output = command
        .output()
        .with_context(|| format!("无法运行 {:?}", command.get_program()))?;
    if !output.status.success() {
        bail!(
            "{:?} 失败: {}",
            command.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Pixels with chroma upsampling unsmoothed, so every pixel comes from its own blocks and equal
/// coefficients decode to equal pixels even across a new seam.
fn blocky(path: &Path) -> Result<DynamicImage> {
    let bmp = run(Command::new("djpeg").args(["-nosmooth", "-bmp"]).arg(path))?;
    Ok(DynamicImage::ImageRgb8(
        image::load_from_memory_with_format(&bmp, ImageFormat::Bmp)?.to_rgb8(),
    ))
}

/// Lossless JPEG join with jpegtran's crop and drop; None when its blocks do not line up.
fn jpegtran_join(
    strips: &[&Page],
    images: &[&DynamicImage],
    overlaps: &[usize],
    axis: Axis,
) -> Result<Option<Vec<u8>>> {
    let scratch = Scratch::new()?;
    let mut paths = Vec::with_capacity(strips.len());
    for (n, (_, _, bytes)) in strips.iter().enumerate() {
        let path = scratch.0.join(format!("{n}.jpg"));
        std::fs::write(&path, bytes)?;
        paths.push(path);
    }
    let sizes: Vec<(u32, u32)> = images.iter().map(|i| (i.width(), i.height())).collect();
    let (offsets, w, h) = layout(&sizes, overlaps, axis);
    let canvas = scratch.0.join("canvas.jpg");
    let next = scratch.0.join("next.jpg");
    run(Command::new("jpegtran")
        .args(["-copy", "all", "-crop", &format!("{w}x{h}+0+0"), "-outfile"])
        .arg(&canvas)
        .arg(&paths[0]))?;
    for (path, offset) in paths.iter().zip(&offsets).skip(1) {
        let at = if axis == Axis::X {
            format!("+{offset}+0")
        } else {
            format!("+0+{offset}")
        };
        run(Command::new("jpegtran")
            .args(["-copy", "all", "-drop", &at])
            .arg(path)
            .arg("-outfile")
            .arg(&next)
            .arg(&canvas))?;
        std::fs::rename(&next, &canvas)?;
    }
    let pieces = paths
        .iter()
        .map(|path| blocky(path))
        .collect::<Result<Vec<_>>>()?;
    let joined = blocky(&canvas)?;
    if joined.as_bytes() != composite(&pieces, overlaps, axis).as_bytes() {
        return Ok(None);
    }
    Ok(Some(std::fs::read(&canvas)?))
}

/// Mean grey difference (0-255) after scaling both to a 256-pixel-wide thumbnail with PIL's
/// box filter, as the calibration did.
fn same_picture(joined: &DynamicImage, page: &DynamicImage) -> f64 {
    let w = 256;
    let h = ((256.0 * f64::from(page.height()) / f64::from(page.width())).round() as usize).max(1);
    let a = box_resize(&Gray::of(joined, Axis::X), w, h);
    let b = box_resize(&Gray::of(page, Axis::X), w, h);
    a.iter().zip(&b).map(|(p, q)| (p - q).abs()).sum::<f64>() / a.len() as f64
}

/// PIL's BOX resampling weights for one dimension: (first source index, weights).
fn box_weights(input: usize, output: usize) -> Vec<(usize, Vec<f64>)> {
    let scale = input as f64 / output as f64;
    let filterscale = scale.max(1.0);
    let support = 0.5 * filterscale;
    (0..output)
        .map(|xx| {
            let center = (xx as f64 + 0.5) * scale;
            let start = ((center - support + 0.5) as isize).max(0) as usize;
            let end = ((center + support + 0.5) as isize).min(input as isize) as usize;
            let mut weights: Vec<f64> = (start..end)
                .map(|x| {
                    let t = (x as f64 - center + 0.5) / filterscale;
                    if (-0.5..0.5).contains(&t) {
                        1.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let total: f64 = weights.iter().sum();
            if total > 0.0 {
                weights.iter_mut().for_each(|w| *w /= total);
            }
            (start, weights)
        })
        .collect()
}

fn box_resize(gray: &Gray, w: usize, h: usize) -> Vec<f64> {
    let across = box_weights(gray.w, w);
    let down = box_weights(gray.h, h);
    let mut rows = vec![0.0; gray.h * w];
    for y in 0..gray.h {
        for (x, (start, weights)) in across.iter().enumerate() {
            rows[y * w + x] = weights
                .iter()
                .enumerate()
                .map(|(i, k)| k * gray.px[y * gray.w + start + i])
                .sum();
        }
    }
    let mut out = vec![0.0; w * h];
    for (y, (start, weights)) in down.iter().enumerate() {
        for x in 0..w {
            out[y * w + x] = weights
                .iter()
                .enumerate()
                .map(|(i, k)| k * rows[(start + i) * w + x])
                .sum();
        }
    }
    out
}
