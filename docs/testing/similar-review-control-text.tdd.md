# Similar-review control text TDD evidence

## Source and user journey

No external plan file was supplied. The test was derived from this journey:

1. As the approver, I want image source and quality details to appear once in the album caption, while the following message contains only a short prompt and the approval buttons.

## RED and GREEN evidence

- RED checkpoint `e1d5775`: `cargo test similar_review_control_text_does_not_repeat_album_details -- --nocapture` ran the new test and failed because the control text repeated every image label and "请选择处理方式".
- GREEN checkpoint `35a8031`: the same command passed after `similar_review_text` was reduced to the review number, image count, and approval prompt.
- Regression: `cargo fmt --check && cargo test --all-targets` passed all 108 library, binary, and integration tests.
- Static checks: `cargo clippy --all-targets -- -D warnings` passed.
- Live acceptance: the Oracle maintenance run edited all 73 still-pending Telegram control messages in place, preserved their inline keyboards, and explicitly included review token 74.

## Test specification

| # | Guarantee | Test or check | Type | Result |
|---|---|---|---|---|
| 1 | The control message shows the review number and album image count | `similar_review_control_text_does_not_repeat_album_details` | unit | PASS |
| 2 | The control message does not repeat image labels or the old instruction | `similar_review_control_text_does_not_repeat_album_details` | unit | PASS |
| 3 | Existing pending controls retain a keyboard after their text is shortened | Oracle `editMessageText` result validation for 73 controls | live E2E | PASS |

## Coverage and known gaps

The changed formatter's production path is fully executed by the focused unit test. A numeric repository coverage report was not generated because this checkout has neither `cargo-llvm-cov` nor Rust's matching `llvm-tools-preview` component installed; the available Apple LLVM version does not match rustc's LLVM version. No dependency was installed solely for this text-only change.

Telegram albums are sent through `sendMediaGroup`, whose API has no `reply_markup` parameter. Therefore the inline keyboard must remain on the immediately following short text message; a button cannot be attached to the album itself.

## Merge evidence

- RED: `e1d5775`
- GREEN: `35a8031`
