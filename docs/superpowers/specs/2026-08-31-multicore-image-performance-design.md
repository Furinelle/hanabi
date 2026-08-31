# Hanabi Multicore Image Performance Design

## Goal

Use the four ARM Neoverse-N1 cores available to the production Hanabi container on `oracle-sjc` for CPU-bound image work without changing deduplication decisions, Telegram ordering, SQLite state transitions, or failure handling.

## Production evidence

The design is based on a live, read-only inspection of `oracle-sjc` on 2026-08-31:

- The host exposes four online Neoverse-N1 cores with no SMT and 23 GiB of memory.
- The Hanabi container has no CPU quota, no cpuset restriction, no memory limit, and sees CPUs `0-3`.
- Hanabi already starts four Tokio runtime workers. Increasing Tokio's worker count alone cannot split one CPU-bound blocking closure across cores.
- The host averaged 93.1% CPU idle during the sampled day and had about 21 GiB available memory. Short non-Hanabi bursts used several cores, so parallel work must remain bounded to the container-visible CPU count.
- The live database contained 38 pending works, of which 35 were single-image works and three were two-image works. Optimizing only album-level parallelism would leave most production work single-core.
- The pending set contained 42 images totaling about 59 MiB, with a maximum encoded size of about 16.5 MiB and dimensions up to 4096 by 4864.

## Chosen architecture

Add Rayon as the single CPU-parallel execution layer. Its global pool must use the process-visible parallelism by default, which is four workers on the current production container, and must continue to honor the standard `RAYON_NUM_THREADS` override. Do not create a second custom CPU pool or hard-code the Oracle core count.

Parallelize work at three nested but bounded levels using the same Rayon pool:

1. Within one image, compute the 23 persisted region fingerprints in parallel. This is the primary production optimization because most pending works contain one image.
2. Across images, inspect fingerprints and prepare Telegram display copies in parallel while collecting into indexed vectors so input order is preserved.
3. In the offline catalog scanner, inspect images and classify pair ranges in parallel, then apply disjoint-set unions and build the report sequentially and deterministically.

Rayon's work-stealing pool handles nested parallel iterators without multiplying the worker count. All filesystem outputs remain one output path per input image, so parallel preparation does not introduce write collisions.

## Remove wasted work

The current region path calls the full-image visual fingerprint helper. That helper calculates a 32 by 32 detail digest, but `RegionFingerprint` does not persist or compare that digest. Split the helper so region computation produces only average hash, difference hash, and color key. Full-image fingerprints must continue to calculate the detail key exactly as before.

This is a required algorithmic reduction, not a semantic change. Existing strict, visual, and partial classification thresholds and fields remain unchanged.

## Determinism and compatibility

- `inspect_image_bytes` must produce exactly the same serialized `ImageFingerprint` as the current implementation for the same bytes.
- Region vector order must remain the existing grid order: `(2,1)`, `(3,1)`, `(1,2)`, `(1,3)`, `(2,2)`, `(3,3)`, then row-major cells inside each grid.
- `inspect_images`, `prepare_all`, and catalog scan outputs must preserve input order.
- Catalog strict groups and similar pairs must retain their current deterministic sorting.
- The SQLite schema, stored JSON shape, similarity thresholds, automatic strict-dedup policy, and human-review policy must not change.
- Tokio task scheduling, Telegram send order, Telegram rate behavior, SQLite transitions, download sequencing, and cleanup sequencing remain unchanged.
- The production release remains an ARM64 target-native Docker build with `--pull never`; no cross-platform or QEMU build is introduced.

## Error handling

Parallel iterators collect `Result` values using the existing fail-fast behavior. Any image inspection or preparation error still fails the enclosing operation. Panics in worker tasks propagate through the existing blocking-task boundary; no fallback to silently incomplete fingerprints or media sets is allowed.

## Tests and benchmark

Tests must prove behavior, not thread implementation details:

- A regression fixture must compare parallel fingerprint output against an independent sequential reference path for multiple image shapes and formats, including region order.
- A preparation test must confirm output ordering when a mix of pass-through and resized images is processed.
- Existing image-dedup and catalog tests must remain unchanged and pass.
- The full repository must pass formatting, all-target tests, and Clippy with warnings denied.

Performance acceptance is measured on `oracle-sjc`, not on the macOS development machine. Build the unchanged base revision and candidate revision natively on Oracle, run both against the same copied set of real pending images from a temporary benchmark directory, and record wall time, CPU utilization, and peak RSS. The benchmark must not modify `/var/lib/hanabi`, Telegram, Vitrine, or the running container.

The candidate is acceptable only when all fingerprints and reports are identical, it demonstrates multicore CPU use, and median wall time over at least five measured runs improves by at least 25% on the real production image set. If the real set is too small for stable timing, repeat the immutable input manifest rather than altering image bytes, and document the repetition count for both revisions.

## Deployment and rollback

After local review and Oracle benchmark acceptance:

1. Back up the current Compose file, config, and SQLite state.
2. Build the exact release tag on `oracle-sjc` using `tools/build_on_target.sh`.
3. Switch the fixed tag with Docker Compose and `--pull never`.
4. Verify image revision and ARM64 architecture, a single running polling owner, restart count zero, SQLite `quick_check`, pending media references, similar-review state, outbox state, and clean logs.
5. Confirm the running Hanabi process has the expected four Tokio workers plus the lazily initialized bounded Rayon workers after exercising an image-processing path.

Rollback uses the prior fixed Docker image and the pre-deployment state/config backups. No schema migration is part of this change.
