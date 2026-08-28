use std::cmp::Ordering;
use std::path::PathBuf;

use anyhow::Result;
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
        .iter()
        .map(|image| {
            Ok(ScannedCatalogImage {
                fingerprint: inspect_image(&image.path)?,
                image: image.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sets = DisjointSet::new(scanned.len());
    let mut similar_indices = Vec::new();

    for left in 0..scanned.len() {
        for right in (left + 1)..scanned.len() {
            match classify_similarity(&scanned[left].fingerprint, &scanned[right].fingerprint) {
                MatchKind::StrictSame => sets.union(left, right),
                MatchKind::Similar { distance } => {
                    similar_indices.push((left, right, distance, "visual"));
                }
                MatchKind::Partial { distance } => {
                    similar_indices.push((left, right, distance, "partial"));
                }
                MatchKind::Different => {}
            }
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
