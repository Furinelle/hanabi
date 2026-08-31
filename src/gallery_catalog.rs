use std::cmp::Ordering;
use std::path::PathBuf;

use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::image_dedup::{classify_similarity, inspect_image, ImageFingerprint, MatchKind};
use crate::model::SourceKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogImage {
    pub work_id: String,
    pub source: SourceKind,
    pub source_id: String,
    pub title: String,
    pub author_name: String,
    pub source_url: String,
    pub page_index: u32,
    pub r2_key: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedCatalogImage {
    pub image: CatalogImage,
    pub fingerprint: ImageFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrictGroup {
    pub keep: ScannedCatalogImage,
    pub remove: Vec<ScannedCatalogImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarPair {
    pub left: ScannedCatalogImage,
    pub right: ScannedCatalogImage,
    pub distance: u32,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogScanReport {
    pub scanned_images: usize,
    pub strict_groups: Vec<StrictGroup>,
    pub similar_pairs: Vec<SimilarPair>,
}

pub fn scan_catalog(images: &[CatalogImage]) -> Result<CatalogScanReport> {
    let scanned = images
        .par_iter()
        .map(inspect_catalog_image)
        .collect::<Result<Vec<_>>>()?;
    let outcomes = pair_outcomes(&scanned);
    build_report(scanned, outcomes)
}

#[cfg(test)]
fn sequential_scan_catalog(images: &[CatalogImage]) -> Result<CatalogScanReport> {
    let scanned = images
        .iter()
        .map(inspect_catalog_image)
        .collect::<Result<Vec<_>>>()?;
    let outcomes = sequential_pair_outcomes(&scanned);
    build_report(scanned, outcomes)
}

fn inspect_catalog_image(image: &CatalogImage) -> Result<ScannedCatalogImage> {
    Ok(ScannedCatalogImage {
        fingerprint: inspect_image(&image.path)?,
        image: image.clone(),
    })
}

enum PairOutcome {
    Strict {
        left: usize,
        right: usize,
    },
    Similar {
        left: usize,
        right: usize,
        distance: u32,
        kind: &'static str,
    },
}

fn classify_right_pairs(scanned: &[ScannedCatalogImage], left: usize) -> Vec<PairOutcome> {
    let mut row = Vec::new();
    for right in (left + 1)..scanned.len() {
        if same_post(&scanned[left], &scanned[right]) {
            continue;
        }
        match classify_similarity(&scanned[left].fingerprint, &scanned[right].fingerprint) {
            MatchKind::StrictSame => row.push(PairOutcome::Strict { left, right }),
            MatchKind::Similar { distance } => row.push(PairOutcome::Similar {
                left,
                right,
                distance,
                kind: "visual",
            }),
            MatchKind::Partial { distance } => row.push(PairOutcome::Similar {
                left,
                right,
                distance,
                kind: "partial",
            }),
            MatchKind::Different => {}
        }
    }
    row
}

fn pair_outcomes(scanned: &[ScannedCatalogImage]) -> Vec<PairOutcome> {
    (0..scanned.len())
        .into_par_iter()
        .map(|left| classify_right_pairs(scanned, left))
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
fn sequential_pair_outcomes(scanned: &[ScannedCatalogImage]) -> Vec<PairOutcome> {
    (0..scanned.len())
        .flat_map(|left| classify_right_pairs(scanned, left))
        .collect()
}

fn build_report(
    scanned: Vec<ScannedCatalogImage>,
    outcomes: Vec<PairOutcome>,
) -> Result<CatalogScanReport> {
    let mut sets = DisjointSet::new(scanned.len());
    let mut similar_indices = Vec::new();
    for outcome in outcomes {
        match outcome {
            PairOutcome::Strict { left, right } => sets.union(left, right),
            PairOutcome::Similar {
                left,
                right,
                distance,
                kind,
            } => similar_indices.push((left, right, distance, kind)),
        }
    }

    let mut groups = std::collections::BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..scanned.len() {
        groups.entry(sets.find(index)).or_default().push(index);
    }
    let mut strict_groups = Vec::new();
    for mut members in groups.into_values().filter(|members| members.len() > 1) {
        members.sort_by(|left, right| quality_cmp(&scanned[*right], &scanned[*left]));
        let keep = scanned[members[0]].clone();
        let remove = members[1..]
            .iter()
            .map(|index| scanned[*index].clone())
            .collect();
        strict_groups.push(StrictGroup { keep, remove });
    }
    strict_groups.sort_by(|left, right| left.keep.image.r2_key.cmp(&right.keep.image.r2_key));

    let mut similar_pairs = similar_indices
        .into_iter()
        .filter(|(left, right, _, _)| sets.find(*left) != sets.find(*right))
        .map(|(left, right, distance, kind)| SimilarPair {
            left: scanned[left].clone(),
            right: scanned[right].clone(),
            distance,
            kind: kind.into(),
        })
        .collect::<Vec<_>>();
    similar_pairs.sort_by(|left, right| {
        left.distance
            .cmp(&right.distance)
            .then_with(|| left.left.image.r2_key.cmp(&right.left.image.r2_key))
            .then_with(|| left.right.image.r2_key.cmp(&right.right.image.r2_key))
    });

    Ok(CatalogScanReport {
        scanned_images: scanned.len(),
        strict_groups,
        similar_pairs,
    })
}

fn same_post(left: &ScannedCatalogImage, right: &ScannedCatalogImage) -> bool {
    left.image.source == right.image.source && left.image.source_id == right.image.source_id
}

fn quality_cmp(left: &ScannedCatalogImage, right: &ScannedCatalogImage) -> Ordering {
    left.fingerprint
        .quality_cmp(&right.fingerprint)
        .then_with(|| right.image.r2_key.cmp(&left.image.r2_key))
}

struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
        }
    }

    fn find(&mut self, value: usize) -> usize {
        if self.parent[value] != value {
            self.parent[value] = self.find(self.parent[value]);
        }
        self.parent[value]
    }

    fn union(&mut self, left: usize, right: usize) {
        let left = self.find(left);
        let right = self.find(right);
        if left != right {
            self.parent[right] = left;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceKind;
    use image::{ImageBuffer, Rgb, RgbImage};
    use std::path::Path;

    fn patterned(width: u32, height: u32) -> RgbImage {
        ImageBuffer::from_fn(width, height, |x, y| {
            let bx = x * 4 / width;
            let by = y * 4 / height;
            Rgb([
                (bx * 53 + by * 17) as u8,
                (bx * 19 + by * 61) as u8,
                (bx * 31 + by * 29) as u8,
            ])
        })
    }

    fn save(path: &Path, image: &RgbImage) {
        image
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    fn entry(path: &Path, source: SourceKind, source_id: &str, key: &str) -> CatalogImage {
        CatalogImage {
            work_id: format!("{}:{source_id}", source.as_str()),
            source,
            source_id: source_id.into(),
            title: source_id.into(),
            author_name: "画师".into(),
            source_url: format!("https://example.test/{source_id}"),
            page_index: 0,
            r2_key: key.into(),
            path: path.into(),
        }
    }

    #[test]
    fn parallel_catalog_scan_matches_reference_order() {
        let dir = tempfile::tempdir().unwrap();
        let low = dir.path().join("low.png");
        let high = dir.path().join("high.png");
        let original_path = dir.path().join("original.png");
        let edited_path = dir.path().join("edited.png");
        save(&low, &patterned(320, 240));
        save(&high, &patterned(1280, 960));
        let original = ImageBuffer::from_fn(640, 480, |x, y| {
            Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        });
        let mut edited = original.clone();
        for y in 200..260 {
            for x in 280..360 {
                edited.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        save(&original_path, &original);
        save(&edited_path, &edited);

        let images = [
            entry(&low, SourceKind::X, "x1", "x/low.png"),
            entry(&high, SourceKind::Pixiv, "p1", "pixiv/high.png"),
            entry(
                &original_path,
                SourceKind::Douyin,
                "d1",
                "douyin/original.png",
            ),
            entry(&edited_path, SourceKind::X, "x2", "x/edited.png"),
        ];
        let expected = sequential_scan_catalog(&images).unwrap();
        let report = scan_catalog(&images).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(report.strict_groups.len(), 1);
        assert_eq!(report.strict_groups[0].keep.image.r2_key, "pixiv/high.png");
        assert_eq!(report.strict_groups[0].remove[0].image.r2_key, "x/low.png");
        assert_eq!(report.similar_pairs.len(), 1);
        assert_eq!(
            report.similar_pairs[0].left.image.r2_key,
            "douyin/original.png"
        );
        assert_eq!(report.similar_pairs[0].right.image.r2_key, "x/edited.png");
    }
}
