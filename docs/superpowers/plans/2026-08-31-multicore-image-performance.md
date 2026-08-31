# Hanabi Multicore Image Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Hanabi's CPU-bound image fingerprinting, Telegram media preparation, and offline catalog analysis use all four production-visible Oracle cores without changing observable results or side-effect order.

**Architecture:** Use one Rayon global work-stealing pool sized from process-visible parallelism. Remove the unused per-region detail digest, parallelize deterministic indexed computations, and leave Tokio, Telegram, SQLite, download, and cleanup sequencing unchanged.

**Tech Stack:** Rust 2021, Rayon, Tokio, image 0.25, rusqlite, existing Rust unit/integration tests

**Spec:** `docs/superpowers/specs/2026-08-31-multicore-image-performance-design.md`

## Global Constraints

- Do not hard-code four workers; Rayon must derive the container-visible CPU count and honor `RAYON_NUM_THREADS`.
- Preserve exact `ImageFingerprint` fields, region order, classification decisions, report sorting, and media output order.
- Do not parallelize Telegram sends, SQLite state transitions, source fetching, downloads, or cleanup.
- Do not change any database schema or serialized JSON shape.
- Keep the existing ARM64 target-native Docker release flow and `--pull never` deployment behavior.

---

### Task 1: Deterministic parallel image fingerprints

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/image_dedup.rs`
- Test: `tests/image_dedup.rs`

**Interfaces:**
- Consumes: existing `inspect_image(path: &Path) -> Result<ImageFingerprint>` and `inspect_image_bytes(encoded: &[u8]) -> Result<ImageFingerprint>`.
- Produces: unchanged public functions and serialized types; an internal sequential test reference may be exposed only under `#[cfg(test)]` if necessary.

- [ ] **Step 1: Add a failing equivalence test**

Add a test that constructs at least three deterministic images with different aspect ratios and formats, computes a reference fingerprint using the current sequential grid traversal, computes the production fingerprint, and asserts full `ImageFingerprint` equality including the ordered `regions` vector. Name the test `parallel_fingerprint_matches_sequential_reference`.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test --test image_dedup parallel_fingerprint_matches_sequential_reference -- --exact
```

Expected: FAIL because the sequential reference or parallel production interface required by the test is not yet present. Confirm the failure is caused by the missing behavior, not malformed image data.

- [ ] **Step 3: Add Rayon and split compact visual computation**

Add `rayon = "1"` to normal dependencies. Refactor the private visual fingerprint code so full images compute average hash, difference hash, color key, and detail key exactly as before, while region images compute only average hash, difference hash, and color key. Do not change resize filters, dimensions, quantization, digest encoding, or strict-key composition.

- [ ] **Step 4: Parallelize ordered region generation**

Build the existing 23 region descriptors in the existing grid and row-major order, then use an indexed Rayon parallel iterator to compute them. Collect into `Vec<RegionFingerprint>` in descriptor order. Do not sort by computed values.

- [ ] **Step 5: Run the focused test and existing dedup suite**

Run:

```bash
cargo test --test image_dedup
```

Expected: all tests PASS and fingerprint equality proves no semantic drift.

- [ ] **Step 6: Commit the independently reviewable task**

```bash
git add Cargo.toml Cargo.lock src/image_dedup.rs tests/image_dedup.rs
git commit -m "perf: parallelize image region fingerprints"
```

### Task 2: Ordered parallel media and catalog work

**Files:**
- Modify: `src/sink/telegram.rs`
- Modify: `src/gallery_catalog.rs`
- Test: `src/sink/telegram.rs`
- Test: `tests/gallery_catalog.rs`

**Interfaces:**
- Consumes: Rayon dependency and unchanged `inspect_image` from Task 1.
- Produces: unchanged `prepare_all(files: &[PathBuf]) -> Result<Vec<PathBuf>>` and `scan_catalog(images: &[CatalogImage]) -> Result<CatalogScanReport>`.

- [ ] **Step 1: Add failing output-order tests**

Add `parallel_prepare_all_preserves_input_order` using a mix of a small pass-through image and two oversized/dimension-limited images with distinct filenames. Assert returned paths correspond positionally to inputs and each output satisfies existing Telegram limits. Extend catalog coverage with `parallel_catalog_scan_matches_reference_order`, comparing the candidate report against the current sequential pair traversal for strict and similar fixtures.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test parallel_prepare_all_preserves_input_order
cargo test --test gallery_catalog parallel_catalog_scan_matches_reference_order -- --exact
```

Expected: FAIL because the parallel/reference behavior under test has not been added.

- [ ] **Step 3: Parallelize image inspection and Telegram preparation**

Use indexed Rayon `par_iter()` collection for the image list inside the existing blocking boundary and for `prepare_all`. Preserve fail-fast `Result<Vec<_>>` behavior and positional output. Do not add Tokio tasks or new thread pools.

- [ ] **Step 4: Parallelize catalog scan deterministically**

Inspect catalog images with indexed `par_iter()`. Classify pair ranges in parallel by left index, collecting one ordered vector per left index. Flatten those vectors in left-index order, apply strict-match unions sequentially, and retain the existing final group and pair sorting. Do not mutate `DisjointSet` from Rayon workers.

- [ ] **Step 5: Run focused and cross-feature tests**

Run:

```bash
cargo test --test gallery_catalog
cargo test --test similar_review
cargo test sink::telegram::tests
```

Expected: all tests PASS with unchanged dedup and review behavior.

- [ ] **Step 6: Commit the independently reviewable task**

```bash
git add src/sink/telegram.rs src/gallery_catalog.rs tests/gallery_catalog.rs
git commit -m "perf: parallelize ordered image batches"
```

### Task 3: Documentation and complete local verification

**Files:**
- Modify: `README.md`
- Modify only if dependency resolution changed it: `Cargo.lock`

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces: documented automatic CPU sizing and `RAYON_NUM_THREADS` override; a locally verified candidate ready for independent Oracle benchmarking.

- [ ] **Step 1: Document runtime behavior**

In the running/configuration documentation, state that CPU-bound image fingerprinting and Telegram display-image preparation automatically use the cores visible to the container. Document `RAYON_NUM_THREADS` as an optional operational override and explicitly state that Telegram publication and database transitions remain ordered.

- [ ] **Step 2: Run formatting and inspect the diff**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: both commands exit zero.

- [ ] **Step 3: Run the complete test and lint gates**

Run:

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Expected: all targets PASS and Clippy emits no warnings.

- [ ] **Step 4: Commit documentation**

```bash
git add README.md Cargo.lock
git commit -m "docs: describe adaptive image parallelism"
```

- [ ] **Step 5: Report the exact implementation boundary**

Provide the commit list, changed files, tests run with exit status, and any deviation from this plan. Do not benchmark or deploy from the implementation agent; Oracle benchmark, code review, production backup, deployment, and acceptance belong to the independent reviewer.
