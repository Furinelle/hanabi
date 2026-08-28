# Image similarity and deduplication TDD evidence

## Source and user journeys

No external plan file was supplied. The tests were derived from these user journeys:

1. As the channel owner, I want the same artwork found on Pixiv, X, and Douyin to appear only once, with the highest-resolution pending copy retained.
2. As the approver, I want near-duplicate artwork to remain reviewable, with both images' resolution and file size visible.
3. As the operator, I want fingerprint state to survive restarts and follow pending, published, discarded, expired, restored, and direct-publish lifecycle changes.
4. As the gallery operator, I want a dry, deterministic full-catalog report that separates automatic strict removals from review-only similar pairs.

## RED and GREEN evidence

- RED: `cargo test --test image_dedup` failed with `unresolved import hanabi::image_dedup`; the new behavior tests compiled far enough to reference the intentionally missing production module.
- GREEN: `cargo test --test image_dedup` passed all 6 image comparison and SQLite catalog integration tests.
- Regression: `cargo test --all-targets` passed 94 tests across library, binary, and integration targets.
- Historical scan RED: `cargo test --test gallery_catalog` failed with `unresolved import hanabi::gallery_catalog` before production code existed (`fcd9ba2`).
- Historical scan GREEN: the same target passed all 3 strict/similar/unrelated catalog tests after `f8baf70`.
- Final regression: `cargo test --all-targets` passed 97 tests across library, binaries, and integration targets.
- Static checks: `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` passed.

## Test specification

| # | Guarantee | Test | Type | Result |
|---|---|---|---|---|
| 1 | The same visual content at different resolutions is strict-equal and the larger resolution wins | `strict_same_survives_resolution_change_and_prefers_more_pixels` | unit | PASS |
| 2 | A local visual edit is review-only similarity, never strict equality | `a_small_visual_edit_is_similar_but_never_strict_same` | unit | PASS |
| 3 | Structurally unrelated images are not flagged | `unrelated_images_are_not_marked_similar` | unit | PASS |
| 4 | A higher-quality strict duplicate replaces a pending record, while published history is not silently deleted | `catalog_replaces_pending_lower_quality_but_not_published_history` | SQLite integration | PASS |
| 5 | Similarity notices show both sources, dimensions, and file sizes | `similar_notice_contains_both_sources_resolution_and_file_size` | integration | PASS |
| 6 | A mixed work loses only its strict-duplicate page and keeps unique pages | `mixed_work_drops_only_strict_duplicate_images_and_keeps_unique_ones` | SQLite integration | PASS |
| 7 | Existing pending media is fingerprinted once during upgrade startup | `existing_pending_media_is_backfilled_into_image_catalog` | SQLite integration | PASS |
| 8 | Historical strict groups retain the highest-resolution image and exclude those members from similar review | `strict_group_keeps_the_highest_resolution_and_never_becomes_similar` | integration | PASS |
| 9 | Historical visual edits remain review-only and never enter the automatic removal plan | `edited_image_is_review_only_and_does_not_enter_auto_remove_plan` | integration | PASS |
| 10 | Unrelated historical images produce no findings | `unrelated_images_produce_no_findings` | integration | PASS |

## Coverage and known gaps

The repository does not provide `cargo-llvm-cov`. Native Rust instrumentation plus the local LLVM tools reported for `src/image_dedup.rs`: 87.03% regions, 85.11% functions, and 89.88% lines.

No live Telegram E2E was run because the test environment intentionally has no bot/channel credentials. Telegram network calls remain behind the existing delivery path; the new deterministic image and database behavior is covered without external side effects. Previously published Telegram posts are treated as immutable: they block future strict duplicates but are never silently deleted to replace an older low-resolution post.

## Merge evidence

- RED checkpoints: `d0695f8`, `48bb661`.
- Historical catalog RED/GREEN checkpoints: `fcd9ba2`, `f8baf70`.
- GREEN implementation is validated by the commands above; this report preserves the RED/GREEN mapping if commits are later squashed.
