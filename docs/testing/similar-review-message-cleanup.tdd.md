# Similar-review message cleanup TDD evidence

## Source and user journey

No external plan file was supplied. The test was derived from this journey:

1. As the approver, I want a successfully completed similar-image review to remove its album and control message, keeping the Telegram chat as clean as ordinary scraped-image approval.

## RED and GREEN evidence

- RED checkpoint `5728ba5`: `cargo test similar_review_cleanup_includes_album_and_control_messages -- --nocapture` failed at compile time because the cleanup message set did not exist.
- GREEN checkpoint `1595311`: the same command passed after the cleanup set combined all album message IDs with the optional control message ID and the success callback deleted that set.
- Regression: `cargo fmt --check && cargo test --all-targets` passed all 109 library, binary, and integration tests.
- Static checks: `cargo clippy --all-targets -- -D warnings` passed.

## Test specification

| # | Guarantee | Test or check | Type | Result |
|---|---|---|---|---|
| 1 | A completed multi-image review schedules every album message and its control message for deletion | `similar_review_cleanup_includes_album_and_control_messages` | unit | PASS |
| 2 | A review without a recorded control message still schedules all album messages | `similar_review_cleanup_includes_album_and_control_messages` | unit | PASS |
| 3 | A failed gallery prune restores the review to pending and does not enter the success cleanup branch | existing callback failure path plus `review_claim_is_single_owner_and_can_be_restored_after_failure` | integration | PASS |

## Coverage and known gaps

The new cleanup-list helper is fully executed by the focused unit test. A numeric repository coverage report was not generated because this checkout has neither `cargo-llvm-cov` nor Rust's matching `llvm-tools-preview` component installed; no dependency was installed solely for this focused callback change.

Telegram deletion is verified in production during rollout by removing any already-decided residual review and preserving all pending review messages. Delete requests use the existing Telegram retry wrapper for rate limits.

## Merge evidence

- RED: `5728ba5`
- GREEN: `1595311`
