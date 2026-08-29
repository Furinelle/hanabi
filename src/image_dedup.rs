use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{MediaItem, SourceKind};

const HASH_SIDE: u32 = 8;
const COLOR_SIDE: u32 = 4;
const MAX_SIMILAR_NOTICES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchKind {
    StrictSame,
    Similar { distance: u32 },
    Partial { distance: u32 },
    Different,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionFingerprint {
    pub width: u32,
    pub height: u32,
    pub average_hash: u64,
    pub difference_hash: u64,
    pub color_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageFingerprint {
    pub content_sha256: String,
    pub strict_key: String,
    pub average_hash: u64,
    pub difference_hash: u64,
    pub color_key: String,
    pub detail_key: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub format: String,
    #[serde(default)]
    pub regions: Vec<RegionFingerprint>,
}

impl ImageFingerprint {
    pub fn quality_cmp(&self, other: &Self) -> Ordering {
        (u64::from(self.width) * u64::from(self.height), self.bytes).cmp(&(
            u64::from(other.width) * u64::from(other.height),
            other.bytes,
        ))
    }

    pub fn dimensions_label(&self) -> String {
        format!("{}×{}", self.width, self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkStatus {
    Pending,
    Published,
}

impl WorkStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Published => "published",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "published" => Some(Self::Published),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSummary {
    pub source: SourceKind,
    pub source_id: String,
    pub title: String,
    pub url: String,
    pub status: WorkStatus,
    pub images: Vec<ImageFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactAction {
    None,
    SkipCurrent(WorkSummary),
    ReplacePending(WorkSummary),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarImage {
    pub current_index: usize,
    pub current: ImageFingerprint,
    pub existing_index: usize,
    pub existing: ImageFingerprint,
    pub existing_work: WorkSummary,
    pub distance: u32,
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupEvaluation {
    pub exact_action: ExactAction,
    pub drop_current_indices: Vec<usize>,
    pub similar: Vec<SimilarImage>,
}

pub fn inspect_image(path: &Path) -> Result<ImageFingerprint> {
    let encoded =
        std::fs::read(path).with_context(|| format!("读取图片失败: {}", path.display()))?;
    inspect_image_bytes(&encoded).with_context(|| format!("检查图片失败: {}", path.display()))
}

pub fn inspect_image_bytes(encoded: &[u8]) -> Result<ImageFingerprint> {
    let format = image::guess_format(&encoded)
        .map(|value| format!("{value:?}").to_ascii_uppercase())
        .unwrap_or_else(|_| "UNKNOWN".into());
    let image = image::load_from_memory(&encoded).context("解码图片失败")?;
    let width = image.width();
    let height = image.height();
    let visual = visual_fingerprint(&image);
    let strict_key =
        hex_digest(format!("{:016x}:{:016x}:{}", visual.0, visual.1, visual.3).as_bytes());
    let regions = split_regions(&image);

    Ok(ImageFingerprint {
        content_sha256: hex_digest(&encoded),
        strict_key,
        average_hash: visual.0,
        difference_hash: visual.1,
        color_key: visual.2,
        detail_key: visual.3,
        width,
        height,
        bytes: encoded.len() as u64,
        format,
        regions,
    })
}

fn visual_fingerprint(image: &image::DynamicImage) -> (u64, u64, String, String) {
    let gray = image.to_luma8();
    let average = image::imageops::resize(&gray, HASH_SIDE, HASH_SIDE, FilterType::Triangle);
    let mean = average.pixels().map(|p| u64::from(p[0])).sum::<u64>() / 64;
    let mut average_hash = 0_u64;
    for (index, pixel) in average.pixels().enumerate() {
        if u64::from(pixel[0]) >= mean {
            average_hash |= 1_u64 << index;
        }
    }
    let difference = image::imageops::resize(&gray, HASH_SIDE + 1, HASH_SIDE, FilterType::Triangle);
    let mut difference_hash = 0_u64;
    for y in 0..HASH_SIDE {
        for x in 0..HASH_SIDE {
            if difference.get_pixel(x, y)[0] > difference.get_pixel(x + 1, y)[0] {
                difference_hash |= 1_u64 << (y * HASH_SIDE + x);
            }
        }
    }
    let rgb = image.to_rgb8();
    let colors = image::imageops::resize(&rgb, COLOR_SIDE, COLOR_SIDE, FilterType::Triangle);
    let color_bytes: Vec<u8> = colors
        .pixels()
        .flat_map(|p| p.0.map(|channel| channel >> 4))
        .collect();
    let details = image::imageops::resize(&rgb, 32, 32, FilterType::Triangle);
    let detail_bytes: Vec<u8> = details
        .pixels()
        .flat_map(|p| p.0.map(|channel| channel >> 4))
        .collect();
    (
        average_hash,
        difference_hash,
        hex_digest(&color_bytes),
        hex_digest(&detail_bytes),
    )
}

fn split_regions(image: &image::DynamicImage) -> Vec<RegionFingerprint> {
    let mut output = Vec::new();
    for (columns, rows) in [(2, 1), (3, 1), (1, 2), (1, 3), (2, 2), (3, 3)] {
        for row in 0..rows {
            for column in 0..columns {
                let x0 = image.width() * column / columns;
                let x1 = image.width() * (column + 1) / columns;
                let y0 = image.height() * row / rows;
                let y1 = image.height() * (row + 1) / rows;
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                let region = image.crop_imm(x0, y0, x1 - x0, y1 - y0);
                let visual = visual_fingerprint(&region);
                output.push(RegionFingerprint {
                    width: x1 - x0,
                    height: y1 - y0,
                    average_hash: visual.0,
                    difference_hash: visual.1,
                    color_key: visual.2,
                });
            }
        }
    }
    output
}

pub fn classify_similarity(left: &ImageFingerprint, right: &ImageFingerprint) -> MatchKind {
    if left.content_sha256 == right.content_sha256 {
        return MatchKind::StrictSame;
    }
    let average_distance = (left.average_hash ^ right.average_hash).count_ones();
    let difference_distance = (left.difference_hash ^ right.difference_hash).count_ones();
    let same_whole_aspect = same_aspect_ratio(left, right, 0.03);
    if same_whole_aspect
        && same_aspect_ratio(left, right, 0.005)
        && left.color_key == right.color_key
        && left.detail_key == right.detail_key
        && average_distance + difference_distance <= 1
    {
        return MatchKind::StrictSame;
    }
    let distance = average_distance + difference_distance;
    if let Some(distance) = partial_distance(left, right) {
        return MatchKind::Partial { distance };
    }
    if same_whole_aspect && average_distance <= 10 && difference_distance <= 10 && distance <= 16 {
        MatchKind::Similar { distance }
    } else {
        MatchKind::Different
    }
}

fn partial_distance(left: &ImageFingerprint, right: &ImageFingerprint) -> Option<u32> {
    region_distance(&left.regions, right)
        .into_iter()
        .chain(region_distance(&right.regions, left))
        .min()
}

fn region_distance(regions: &[RegionFingerprint], whole: &ImageFingerprint) -> Option<u32> {
    regions
        .iter()
        .filter(|region| same_dimensions_aspect(region.width, region.height, whole, 0.03))
        .filter(|region| region.color_key == whole.color_key)
        .filter_map(|region| {
            let average = (region.average_hash ^ whole.average_hash).count_ones();
            let difference = (region.difference_hash ^ whole.difference_hash).count_ones();
            let distance = average + difference;
            (average <= 10 && difference <= 10 && distance <= 16).then_some(distance)
        })
        .min()
}

fn same_dimensions_aspect(
    width: u32,
    height: u32,
    other: &ImageFingerprint,
    tolerance: f64,
) -> bool {
    if height == 0 || other.height == 0 {
        return false;
    }
    let left_ratio = f64::from(width) / f64::from(height);
    let right_ratio = f64::from(other.width) / f64::from(other.height);
    ((left_ratio - right_ratio).abs() / left_ratio.max(right_ratio)) <= tolerance
}

fn same_aspect_ratio(left: &ImageFingerprint, right: &ImageFingerprint, tolerance: f64) -> bool {
    if left.height == 0 || right.height == 0 {
        return false;
    }
    let left_ratio = f64::from(left.width) / f64::from(left.height);
    let right_ratio = f64::from(right.width) / f64::from(right.height);
    ((left_ratio - right_ratio).abs() / left_ratio.max(right_ratio)) <= tolerance
}

fn hex_digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_fingerprints(
            source_kind     TEXT NOT NULL,
            source_id       TEXT NOT NULL,
            image_index     INTEGER NOT NULL,
            title           TEXT NOT NULL,
            source_url      TEXT NOT NULL,
            status          TEXT NOT NULL,
            content_sha256  TEXT NOT NULL,
            strict_key      TEXT NOT NULL,
            average_hash    TEXT NOT NULL,
            difference_hash TEXT NOT NULL,
            color_key       TEXT NOT NULL,
            detail_key      TEXT NOT NULL,
            width           INTEGER NOT NULL,
            height          INTEGER NOT NULL,
            bytes           INTEGER NOT NULL,
            format          TEXT NOT NULL,
            regions_json    TEXT NOT NULL DEFAULT '[]',
            recorded_at     INTEGER NOT NULL,
            PRIMARY KEY(source_kind, source_id, image_index)
         );
         CREATE INDEX IF NOT EXISTS idx_image_fingerprints_strict
           ON image_fingerprints(strict_key);
         CREATE INDEX IF NOT EXISTS idx_image_fingerprints_status
           ON image_fingerprints(status);",
    )?;
    let _ = conn.execute(
        "ALTER TABLE image_fingerprints ADD COLUMN regions_json TEXT NOT NULL DEFAULT '[]'",
        [],
    );
    Ok(())
}

pub fn record_work(
    conn: &Connection,
    item: &MediaItem,
    fingerprints: &[ImageFingerprint],
    status: WorkStatus,
) -> Result<()> {
    let (kind, source_id) = item.dedup_key();
    conn.execute(
        "DELETE FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2",
        params![kind, source_id],
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let title = item.title.as_deref().unwrap_or("(无标题)");
    for (index, fingerprint) in fingerprints.iter().enumerate() {
        conn.execute(
            "INSERT INTO image_fingerprints(
                source_kind,source_id,image_index,title,source_url,status,
                content_sha256,strict_key,average_hash,difference_hash,color_key,detail_key,
                width,height,bytes,format,regions_json,recorded_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                kind,
                source_id,
                index as i64,
                title,
                item.url,
                status.as_str(),
                fingerprint.content_sha256,
                fingerprint.strict_key,
                format!("{:016x}", fingerprint.average_hash),
                format!("{:016x}", fingerprint.difference_hash),
                fingerprint.color_key,
                fingerprint.detail_key,
                i64::from(fingerprint.width),
                i64::from(fingerprint.height),
                fingerprint.bytes as i64,
                fingerprint.format,
                serde_json::to_string(&fingerprint.regions)?,
                now,
            ],
        )?;
    }
    Ok(())
}

pub fn mark_work_status(conn: &Connection, item: &MediaItem, status: WorkStatus) -> Result<()> {
    let (kind, source_id) = item.dedup_key();
    conn.execute(
        "UPDATE image_fingerprints SET status=?3 WHERE source_kind=?1 AND source_id=?2",
        params![kind, source_id, status.as_str()],
    )?;
    Ok(())
}

pub fn remove_work(conn: &Connection, item: &MediaItem) -> Result<()> {
    let (kind, source_id) = item.dedup_key();
    remove_work_key(conn, &kind, &source_id)
}

pub fn remove_work_key(conn: &Connection, source_kind: &str, source_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM image_fingerprints WHERE source_kind=?1 AND source_id=?2",
        params![source_kind, source_id],
    )?;
    Ok(())
}

pub fn evaluate_work(
    conn: &Connection,
    item: &MediaItem,
    current: &[ImageFingerprint],
) -> Result<DedupEvaluation> {
    let existing = load_works(conn)?;
    let current_key = item.dedup_key();
    let exact_works: Vec<&WorkSummary> = existing
        .iter()
        .filter(|work| {
            (work.source.as_str(), work.source_id.as_str())
                != (current_key.0.as_str(), current_key.1.as_str())
        })
        .filter(|work| work_is_strict_same(current, &work.images))
        .collect();

    let exact_action = if let Some(published) = exact_works
        .iter()
        .copied()
        .filter(|work| work.status == WorkStatus::Published)
        .max_by(|left, right| work_quality_cmp(left, right))
    {
        ExactAction::SkipCurrent(published.clone())
    } else if let Some(pending) = exact_works
        .iter()
        .copied()
        .max_by(|left, right| work_quality_cmp(left, right))
    {
        if current_quality_cmp(current, &pending.images) == Ordering::Greater {
            ExactAction::ReplacePending(pending.clone())
        } else {
            ExactAction::SkipCurrent(pending.clone())
        }
    } else {
        ExactAction::None
    };

    let mut drop_current_indices = Vec::new();
    if matches!(exact_action, ExactAction::None) {
        for (current_index, fingerprint) in current.iter().enumerate() {
            let should_drop = existing.iter().any(|work| {
                if (work.source.as_str(), work.source_id.as_str())
                    == (current_key.0.as_str(), current_key.1.as_str())
                {
                    return false;
                }
                work.images.iter().any(|old| {
                    classify_similarity(fingerprint, old) == MatchKind::StrictSame
                        && (work.status == WorkStatus::Published
                            || old.quality_cmp(fingerprint) != Ordering::Less)
                })
            });
            if should_drop {
                drop_current_indices.push(current_index);
            }
        }
    }

    // 同一作品内部也只保留严格同图中画质最高的一张。
    for left in 0..current.len() {
        if drop_current_indices.contains(&left) {
            continue;
        }
        for right in (left + 1)..current.len() {
            if classify_similarity(&current[left], &current[right]) != MatchKind::StrictSame {
                continue;
            }
            let loser = if current[left].quality_cmp(&current[right]) == Ordering::Less {
                left
            } else {
                right
            };
            if !drop_current_indices.contains(&loser) {
                drop_current_indices.push(loser);
            }
        }
    }
    drop_current_indices.sort_unstable();

    let exact_work_keys: HashSet<(String, String)> = exact_works
        .iter()
        .map(|work| (work.source.as_str().to_string(), work.source_id.clone()))
        .collect();
    let mut similar = Vec::new();
    for (current_index, fingerprint) in current.iter().enumerate() {
        if drop_current_indices.contains(&current_index) {
            continue;
        }
        for work in &existing {
            if exact_work_keys.contains(&(work.source.as_str().to_string(), work.source_id.clone()))
                || (work.source.as_str(), work.source_id.as_str())
                    == (current_key.0.as_str(), current_key.1.as_str())
            {
                continue;
            }
            for (existing_index, old) in work.images.iter().enumerate() {
                let matched = classify_similarity(fingerprint, old);
                if let MatchKind::Similar { distance } | MatchKind::Partial { distance } = matched {
                    similar.push(SimilarImage {
                        current_index,
                        current: fingerprint.clone(),
                        existing_index,
                        existing: old.clone(),
                        existing_work: work.clone(),
                        distance,
                        partial: matches!(matched, MatchKind::Partial { .. }),
                    });
                }
            }
        }
    }
    similar.sort_by_key(|value| value.distance);
    similar.dedup_by(|left, right| {
        left.current_index == right.current_index
            && left.existing_work.source == right.existing_work.source
            && left.existing_work.source_id == right.existing_work.source_id
    });
    similar.truncate(MAX_SIMILAR_NOTICES);

    Ok(DedupEvaluation {
        exact_action,
        drop_current_indices,
        similar,
    })
}

fn work_is_strict_same(current: &[ImageFingerprint], existing: &[ImageFingerprint]) -> bool {
    if current.len() != existing.len() || current.is_empty() {
        return false;
    }
    let mut used = vec![false; existing.len()];
    current.iter().all(|candidate| {
        let found = existing.iter().enumerate().position(|(index, old)| {
            !used[index] && classify_similarity(candidate, old) == MatchKind::StrictSame
        });
        if let Some(index) = found {
            used[index] = true;
            true
        } else {
            false
        }
    })
}

fn current_quality_cmp(left: &[ImageFingerprint], right: &[ImageFingerprint]) -> Ordering {
    let score = |values: &[ImageFingerprint]| {
        values.iter().fold((0_u64, 0_u64), |sum, value| {
            (
                sum.0 + u64::from(value.width) * u64::from(value.height),
                sum.1 + value.bytes,
            )
        })
    };
    score(left).cmp(&score(right))
}

fn work_quality_cmp(left: &WorkSummary, right: &WorkSummary) -> Ordering {
    current_quality_cmp(&left.images, &right.images)
}

fn load_works(conn: &Connection) -> Result<Vec<WorkSummary>> {
    let mut stmt = conn.prepare(
        "SELECT source_kind,source_id,image_index,title,source_url,status,
                content_sha256,strict_key,average_hash,difference_hash,color_key,detail_key,
                width,height,bytes,format,COALESCE(regions_json, '[]')
         FROM image_fingerprints
         ORDER BY source_kind,source_id,image_index",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            ImageFingerprint {
                content_sha256: row.get(6)?,
                strict_key: row.get(7)?,
                average_hash: parse_hash(row.get::<_, String>(8)?),
                difference_hash: parse_hash(row.get::<_, String>(9)?),
                color_key: row.get(10)?,
                detail_key: row.get(11)?,
                width: row.get::<_, i64>(12)?.max(0) as u32,
                height: row.get::<_, i64>(13)?.max(0) as u32,
                bytes: row.get::<_, i64>(14)?.max(0) as u64,
                format: row.get(15)?,
                regions: serde_json::from_str(&row.get::<_, String>(16)?).unwrap_or_default(),
            },
        ))
    })?;
    let mut grouped: BTreeMap<(String, String), WorkSummary> = BTreeMap::new();
    for row in rows {
        let (kind, source_id, _index, title, url, status, fingerprint) = row?;
        let source = parse_source(&kind).context("image_fingerprints source_kind 无效")?;
        let status = WorkStatus::from_str(&status).context("image_fingerprints status 无效")?;
        grouped
            .entry((kind, source_id.clone()))
            .or_insert_with(|| WorkSummary {
                source,
                source_id,
                title,
                url,
                status,
                images: Vec::new(),
            })
            .images
            .push(fingerprint);
    }
    Ok(grouped.into_values().collect())
}

fn parse_hash(value: String) -> u64 {
    u64::from_str_radix(&value, 16).unwrap_or(0)
}

fn parse_source(value: &str) -> Option<SourceKind> {
    match value {
        "pixiv" => Some(SourceKind::Pixiv),
        "x" => Some(SourceKind::X),
        "douyin" => Some(SourceKind::Douyin),
        _ => None,
    }
}

pub fn render_review_notice(matches: &[SimilarImage]) -> String {
    if matches.is_empty() {
        return String::new();
    }
    let mut output = String::from("\n\n🔎 相似图片（需人工确认）");
    for value in matches.iter().take(MAX_SIMILAR_NOTICES) {
        output.push_str(&format!(
            "\n• {}：当前 {} · {}；{} {} {} · {}（差异 {}）",
            if value.partial {
                "疑似原图局部"
            } else {
                "整图相似"
            },
            value.current.dimensions_label(),
            format_bytes(value.current.bytes),
            source_label(value.existing_work.source),
            value.existing_work.source_id,
            value.existing.dimensions_label(),
            format_bytes(value.existing.bytes),
            value.distance,
        ));
    }
    output
}

fn source_label(source: SourceKind) -> &'static str {
    match source {
        SourceKind::Pixiv => "Pixiv",
        SourceKind::X => "X",
        SourceKind::Douyin => "抖音",
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
