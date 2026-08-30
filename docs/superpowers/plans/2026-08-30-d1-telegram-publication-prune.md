# D1-backed Telegram Publication Prune Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist Vitrine work-to-Telegram publication mappings in Cloudflare D1 and make a similarity decision delete each losing work's complete D1/R2 representation and every mapped channel message through an idempotent retryable saga.

**Architecture:** Hanabi captures every Telegram channel message returned by publication and includes the mapping in Vitrine ingest plus the local gallery compensation outbox. Vitrine stores mappings and immutable whole-work prune receipts in D1, backs up and removes complete losing works, and returns Telegram targets; Hanabi deletes those messages and finalizes the D1 receipt before completing the private review.

**Tech Stack:** Rust 2021, teloxide 0.13, rusqlite 0.31, Cloudflare workers-rs 0.8, D1, R2, Python 3 stdlib, tdl, Cargo, Wrangler 4.

**Spec:** `docs/superpowers/specs/2026-08-30-d1-telegram-publication-prune-design.md`

## Global Constraints

- Work in `/Users/furina/Documents/Github/hanabi` and `/Users/furina/Documents/Github/vitrine`.
- Preserve the existing uncommitted Vitrine `wrangler.jsonc` D1-binding change exactly; never stage or edit it.
- Never print or persist Telegram tokens, Vitrine bearer tokens, cookies, or plaintext credentials.
- Never infer Telegram message IDs by arithmetic, title, author, timestamp, or image similarity.
- Every destructive operation is bounded, authenticated, idempotent, and blocked when a losing work has no exact active D1 publication mapping.
- Vitrine never receives the Telegram bot token; Hanabi remains the only Telegram mutator.
- Existing `/api/catalog/prune` behavior and receipts remain compatible.
- Use TDD for every behavior change and run complete repository gates before stopping.
- Stage exact paths in every commit; never use `git add -A` in the dirty Vitrine checkout.
- Grok stops after local commits and verification. It must not push, tag, migrate remote D1, SSH, deploy, mutate Telegram, or perform live backfill.

## File and interface map

### Vitrine

- `migrations/0004_telegram_publications.sql`: D1 mappings and whole-work prune receipts.
- `src/ingest.rs`: optional publication validation, fingerprint inclusion, atomic D1 upsert.
- `src/lib.rs`: backfill, whole-work prune/replay, and Telegram-result endpoints.
- `README.md`: API and migration operations.

### Hanabi

- `src/sink/telegram.rs`: publication capture, whole-work saga, Telegram deletion.
- `src/gallery.rs`: publication ingest and prune/finalize clients.
- `src/gallery_outbox.rs`: durable mapping in SQLite and manifests.
- `src/similar_review.rs`: work-level selection contracts.
- `tests/gallery_outbox.rs`, `tests/similar_review.rs`: integration behavior.
- `tools/backfill_telegram_publications.py`: exact historical matching and apply mode.
- `tests/test_backfill_telegram_publications.py`: Python stdlib tests.
- `README.md`, `CHANGELOG.md`, `Cargo.toml`, `Cargo.lock`: behavior and release.

---

### Task 1: Add and locally verify Vitrine D1 schema

**Files:**
- Create: `/Users/furina/Documents/Github/vitrine/migrations/0004_telegram_publications.sql`
- Modify: `/Users/furina/Documents/Github/vitrine/README.md`

**Interfaces:**
- Produces `telegram_publications(id,work_id,chat_id,anchor_message_id,message_ids_json,publish_state,created_at,deleted_at)`.
- Produces `catalog_work_prune_receipts(decision_id,keep_work_id,remove_work_ids_json,removed_r2_keys_json,telegram_targets_json,telegram_state,telegram_error,created_at,telegram_completed_at)`.

- [ ] **Step 1: Prove the new tables do not exist in a pre-0004 local D1**

Run from Vitrine with a checkout before creating migration 0004:

```bash
persist_dir=$(mktemp -d /tmp/vitrine-d1-publications.XXXXXX)
npx wrangler d1 migrations apply DB --local --persist-to "$persist_dir"
npx wrangler d1 execute DB --local --persist-to "$persist_dir" --command "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('telegram_publications','catalog_work_prune_receipts') ORDER BY name"
```

Expected: no matching rows.

- [ ] **Step 2: Create migration 0004**

```sql
CREATE TABLE telegram_publications (
  id TEXT PRIMARY KEY,
  work_id TEXT NOT NULL,
  chat_id INTEGER NOT NULL,
  anchor_message_id INTEGER NOT NULL,
  message_ids_json TEXT NOT NULL,
  publish_state TEXT NOT NULL CHECK (publish_state IN ('full','partial')),
  created_at TEXT NOT NULL,
  deleted_at TEXT,
  UNIQUE(work_id, chat_id, anchor_message_id)
);
CREATE INDEX idx_telegram_publications_work_active ON telegram_publications(work_id, deleted_at);
CREATE TABLE catalog_work_prune_receipts (
  decision_id TEXT PRIMARY KEY,
  keep_work_id TEXT NOT NULL,
  remove_work_ids_json TEXT NOT NULL,
  removed_r2_keys_json TEXT NOT NULL,
  telegram_targets_json TEXT NOT NULL,
  telegram_state TEXT NOT NULL CHECK (telegram_state IN ('pending','complete')),
  telegram_error TEXT,
  created_at TEXT NOT NULL,
  telegram_completed_at TEXT
);
CREATE INDEX idx_catalog_work_prune_telegram_state ON catalog_work_prune_receipts(telegram_state, created_at);
```

- [ ] **Step 3: Apply migrations to a fresh local D1 and verify constraints**

Run:

```bash
persist_dir=$(mktemp -d /tmp/vitrine-d1-publications.XXXXXX)
npx wrangler d1 migrations apply DB --local --persist-to "$persist_dir"
npx wrangler d1 execute DB --local --persist-to "$persist_dir" --command "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('telegram_publications','catalog_work_prune_receipts') ORDER BY name"
npx wrangler d1 execute DB --local --persist-to "$persist_dir" --command "INSERT INTO telegram_publications VALUES ('p','pixiv:1',-1001,10,'[10]','invalid','2026-08-30T00:00:00Z',NULL)"
```

Expected: both tables exist and the invalid state insert fails its CHECK constraint.

- [ ] **Step 4: Document `npm run db:local` and operator-only `npm run db:remote`**

State that remote migration requires D1 export/backup and explicit operator approval.

- [ ] **Step 5: Commit exact files**

```bash
git add migrations/0004_telegram_publications.sql README.md
git commit -m "feat: add Telegram publication mapping schema"
```

---

### Task 2: Validate and atomically ingest publication mappings in Vitrine

**Files:**
- Modify: `/Users/furina/Documents/Github/vitrine/src/ingest.rs`

**Interfaces:**
- Produces `ValidatedPublication { id: String, chat_id: i64, anchor_message_id: i64, message_ids: Vec<i64>, publish_state: PublicationState }`.
- Adds optional `telegram_publication` to `IngestMetaRaw` and `ValidatedMeta`.
- Upserts the publication in `commit_d1_batch` with work/images.

- [ ] **Step 1: Add RED validation tests**

```rust
#[test]
fn validates_complete_telegram_publication_mapping() {
    let meta = validate_meta_json(r#"{
      "source":"pixiv","source_id":"1",
      "telegram_publication":{"chat_id":-100123,"message_ids":[41,42],"publish_state":"full"}
    }"#).unwrap();
    let publication = meta.telegram_publication.unwrap();
    assert_eq!(publication.anchor_message_id, 41);
    assert_eq!(publication.message_ids, vec![41, 42]);
}

#[test]
fn rejects_duplicate_telegram_message_ids() {
    let error = validate_meta_json(r#"{
      "source":"pixiv","source_id":"1",
      "telegram_publication":{"chat_id":-100123,"message_ids":[41,41],"publish_state":"full"}
    }"#).unwrap_err();
    assert_eq!(error.message(), "invalid telegram publication");
}
```

Also cover zero chat, non-positive IDs, empty IDs, more than 40 IDs, and invalid state.

- [ ] **Step 2: Run RED**

Run: `cargo test telegram_publication -- --nocapture`

Expected: failure because publication metadata does not exist.

- [ ] **Step 3: Implement validation and stable ID**

```rust
pub fn telegram_publication_id(work_id: &str, chat_id: i64, anchor: i64) -> String {
    format!("{work_id}:{chat_id}:{anchor}")
}
```

Derive the anchor from the first ID. Require one non-zero chat, 1 to 40 unique positive message IDs, and `full|partial` state.

- [ ] **Step 4: Include normalized publication JSON in the ingest fingerprint**

A replay with different Telegram IDs must conflict instead of returning an old receipt.

- [ ] **Step 5: Upsert publication in `commit_d1_batch`**

```sql
INSERT INTO telegram_publications (id,work_id,chat_id,anchor_message_id,message_ids_json,publish_state,created_at,deleted_at)
VALUES (?,?,?,?,?,?,?,NULL)
ON CONFLICT(id) DO UPDATE SET message_ids_json=excluded.message_ids_json,publish_state=excluded.publish_state,deleted_at=NULL
```

- [ ] **Step 6: Verify and commit**

Run `cargo test telegram_publication -- --nocapture`, `npm run check`, and `git diff --check`.

```bash
git add src/ingest.rs
git commit -m "feat: persist Telegram publications during ingest"
```

---

### Task 3: Add authenticated publication backfill API

**Files:**
- Modify: `/Users/furina/Documents/Github/vitrine/src/lib.rs`

**Interfaces:**
- Produces `PUT /api/catalog/publications` with existing bearer auth.
- Consumes `{ work_id, chat_id, message_ids, publish_state }`.
- Returns `{ ok, publication_id, idempotent }`.

- [ ] **Step 1: Add RED request-validation test**

```rust
#[test]
fn publication_upsert_requires_complete_ids_shape() {
    let valid = CatalogPublicationRequest {
        work_id: "pixiv:1".into(), chat_id: -100123,
        message_ids: vec![41, 42], publish_state: "full".into(),
    };
    assert!(validate_catalog_publication(&valid).is_ok());
    let duplicate = CatalogPublicationRequest { message_ids: vec![41, 41], ..valid };
    assert_eq!(validate_catalog_publication(&duplicate), Err("duplicate message id"));
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test publication_upsert -- --nocapture`.

- [ ] **Step 3: Implement route, active-work check, and idempotent upsert**

Reject absent or soft-deleted works with 409. Same stable ID and same fields returns `idempotent=true`; conflicting values return 409.

- [ ] **Step 4: Verify and commit**

Run `cargo test publication_upsert -- --nocapture`, `npm run check`, and `git diff --check`.

```bash
git add src/lib.rs
git commit -m "feat: add Telegram publication backfill API"
```

---

### Task 4: Implement recoverable whole-work prune and replay in Vitrine

**Files:**
- Modify: `/Users/furina/Documents/Github/vitrine/src/lib.rs`
- Modify: `/Users/furina/Documents/Github/vitrine/README.md`

**Interfaces:**
- Produces `POST /api/catalog/prune-works`.
- Request: `CatalogWorkPruneRequest { decision_id, keep_work_id, remove_work_ids }`.
- Response: `{ ok, removed_works, removed_r2_keys, telegram_targets, replayed }`.
- Each target is `{ publication_id, work_id, chat_id, message_ids }`.

- [ ] **Step 1: Add RED validation and replay-plan tests**

```rust
#[test]
fn whole_work_prune_is_bounded_and_distinct() {
    let valid = CatalogWorkPruneRequest {
        decision_id: "hanabi-similar-91".into(),
        keep_work_id: "pixiv:2".into(),
        remove_work_ids: vec!["douyin:1".into()],
    };
    assert!(validate_catalog_work_prune(&valid).is_ok());
    let invalid = CatalogWorkPruneRequest {
        remove_work_ids: vec!["pixiv:2".into()], ..valid
    };
    assert_eq!(validate_catalog_work_prune(&invalid), Err("keep work cannot be removed"));
}

#[test]
fn receipt_replay_requires_identical_plan() {
    assert!(same_work_prune_plan("pixiv:2", &["douyin:1".into()], "pixiv:2", &["douyin:1".into()]));
    assert!(!same_work_prune_plan("pixiv:2", &["douyin:1".into()], "pixiv:3", &["douyin:1".into()]));
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test whole_work_prune -- --nocapture`.

- [ ] **Step 3: Implement mutation-free preflight**

Load the active keep work, every active losing work, every losing image/tag, and every active publication. Return 409 before R2 copy when a losing work has no mapping or invalid/empty message JSON. Bound losing works at 20.

- [ ] **Step 4: Back up every losing R2 object**

Reuse `catalog_backup_key` and `copy_catalog_object`. Every backup key must remain `review-trash/<decision_id>/<original-key>`. The source list is all active images for losing work IDs, including pages absent from similarity pairs.

- [ ] **Step 5: Commit one D1 deletion plan**

Build one D1 batch that inserts all `catalog_prune_backups`, deletes all losing `images` and `work_tags`, recalculates affected `tags.use_count` from remaining associations, soft-deletes every losing `works` row, and inserts `catalog_work_prune_receipts` with `telegram_state='pending'`.

- [ ] **Step 6: Delete original R2 objects and implement replay**

After D1 commit, delete stored original keys. Exact replay reads the receipt, retries the same R2 deletes, and returns stored Telegram targets. Conflicting decision reuse returns 409.

- [ ] **Step 7: Run focused, full, and local-D1 gates**

```bash
cargo test whole_work_prune -- --nocapture
npm run check
persist_dir=$(mktemp -d /tmp/vitrine-prune-works.XXXXXX)
npx wrangler d1 migrations apply DB --local --persist-to "$persist_dir"
npx wrangler d1 execute DB --local --persist-to "$persist_dir" --command "SELECT name FROM sqlite_master WHERE type='table' AND name='catalog_work_prune_receipts'"
git diff --check
```

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs README.md
git commit -m "feat: prune complete duplicate works recoverably"
```

---

### Task 5: Add Vitrine Telegram-completion endpoint

**Files:**
- Modify: `/Users/furina/Documents/Github/vitrine/src/lib.rs`

**Interfaces:**
- Produces `POST /api/catalog/prune-works/telegram-result`.
- Consumes `{ decision_id, complete, error }`.
- Returns `{ ok, telegram_state, idempotent }`.

- [ ] **Step 1: Add RED transition test**

```rust
#[test]
fn successful_telegram_result_rejects_error_text() {
    let value = CatalogTelegramResult {
        decision_id: "d1".into(), complete: true, error: Some("unexpected".into()),
    };
    assert_eq!(validate_telegram_result(&value), Err("successful result cannot include error"));
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test telegram_result -- --nocapture`.

- [ ] **Step 3: Implement authenticated atomic completion**

Successful completion loads receipt targets and batch-updates only those `telegram_publications.deleted_at` rows plus receipt state/time. Repeated success returns `idempotent=true`. Failure stores at most 500 sanitized characters in `telegram_error` and leaves state pending.

- [ ] **Step 4: Verify and commit**

Run `cargo test telegram_result -- --nocapture`, `npm run check`, and `git diff --check`.

```bash
git add src/lib.rs
git commit -m "feat: finalize Telegram prune receipts"
```

---

### Task 6: Capture complete Telegram channel publication results in Hanabi

**Files:**
- Modify: `/Users/furina/Documents/Github/hanabi/src/sink/telegram.rs`

**Interfaces:**
- Produces `TelegramPublication { chat_id: i64, message_ids: Vec<i32>, publish_state: PublicationState }`.
- Produces `PublishResult { outcome: PublishOutcome, publication: TelegramPublication }` for direct sends.
- `send_group` and `send_batches` record actual response chat/message IDs.

- [ ] **Step 1: Add RED recording test**

```rust
#[test]
fn publication_capture_records_batches_and_rejects_mixed_chats() {
    let mut builder = TelegramPublicationBuilder::default();
    builder.record(-100123, [41, 42]).unwrap();
    builder.record(-100123, [43]).unwrap();
    assert_eq!(builder.clone().finish(true).unwrap().message_ids, vec![41, 42, 43]);
    assert!(builder.record(-100999, [44]).is_err());
}
```

- [ ] **Step 2: Run RED**

Run: `cargo test publication_capture -- --nocapture`.

- [ ] **Step 3: Implement structured capture**

Capture `message.chat.id.0` and `message.id.0` from every single or album response. Require one chat, non-empty unique positive IDs, and send order. State is `full` after all batches and `partial` after a later failure with at least one created message.

- [ ] **Step 4: Thread capture through direct and pending publication**

Keep the first ID as discussion anchor. Do not change existing retry or partial-publication behavior.

- [ ] **Step 5: Verify and commit**

Run `cargo test publication_capture -- --nocapture`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`.

```bash
git add src/sink/telegram.rs
git commit -m "feat: capture channel publication messages"
```

---

### Task 7: Persist mappings through GalleryClient and Hanabi outbox

**Files:**
- Modify: `/Users/furina/Documents/Github/hanabi/src/gallery.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/src/gallery_outbox.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/src/sink/telegram.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/tests/gallery_outbox.rs`

**Interfaces:**
- Produces serializable `GalleryPublication { chat_id, message_ids, publish_state }`.
- `GalleryClient::ingest(item, files, publication: Option<&GalleryPublication>)` includes `telegram_publication` in meta.
- `QueuedGalleryWork` and manifest expose `publication: Option<GalleryPublication>`.
- Adds `gallery_outbox.publication_json TEXT NOT NULL DEFAULT 'null'`.

- [ ] **Step 1: Add RED restart and old-schema tests**

```rust
#[test]
fn queued_work_preserves_publication_after_restart() {
    let publication = GalleryPublication {
        chat_id: -100123, message_ids: vec![41, 42], publish_state: GalleryPublishState::Full,
    };
    enqueue_with_publication(&outbox, &item(), &files, Some(publication.clone())).unwrap();
    drop(outbox);
    let reopened = GalleryOutbox::open(&db_path).unwrap();
    assert_eq!(reopened.query_queued("pixiv", "1").unwrap().unwrap().publication, Some(publication));
}
```

Add a test opening a pre-column database and assert `publication=None` without row loss.

- [ ] **Step 2: Run RED**

Run: `cargo test --test gallery_outbox publication -- --nocapture`.

- [ ] **Step 3: Implement GalleryPublication and ingest meta**

Serialize state as `full|partial`, include it in stable metadata/idempotency, and retain GalleryClient token redaction.

- [ ] **Step 4: Implement SQLite/manifest compatibility**

Add the column with `ALTER TABLE`, serde-default legacy manifests, and preserve publication through staging recovery, retry, and path rebase.

- [ ] **Step 5: Pass capture through direct and approve-archive flows**

Initial ingest and queued retries must use the identical mapping.

- [ ] **Step 6: Verify and commit**

Run `cargo test --test gallery_outbox`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`.

```bash
git add src/gallery.rs src/gallery_outbox.rs src/sink/telegram.rs tests/gallery_outbox.rs
git commit -m "feat: persist gallery publication mappings"
```

---

### Task 8: Implement Hanabi whole-work prune and Telegram deletion saga

**Files:**
- Modify: `/Users/furina/Documents/Github/hanabi/src/gallery.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/src/sink/telegram.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/src/similar_review.rs`
- Modify: `/Users/furina/Documents/Github/hanabi/tests/similar_review.rs`

**Interfaces:**
- Produces `GalleryClient::prune_similar_works(decision_id, keep_work_id, remove_work_ids)`.
- Produces `GalleryWorkPruneResult { removed_work_ids, removed_r2_keys, telegram_targets, replayed }`.
- Produces `finish_telegram_prune(decision_id)` and `report_telegram_prune_failure(decision_id,error)`.
- Telegram deletion treats only `message to delete not found` as idempotent success.

- [ ] **Step 1: Add RED request-plan test for the four-image example**

```rust
#[test]
fn selecting_pixiv_builds_whole_work_request() {
    let group = multi_page_group();
    let request = work_prune_plan(&group, 2).unwrap();
    assert_eq!(request.keep_work_id, "pixiv:147342918");
    assert_eq!(request.remove_work_ids, vec!["douyin:7669678713420921673"]);
}
```

- [ ] **Step 2: Add RED saga and deletion-classifier tests**

```rust
#[test]
fn review_finishes_only_after_telegram_completion() {
    assert_eq!(next_prune_action(PruneProgress::GalleryPruned), PruneAction::DeleteTelegram);
    assert_eq!(next_prune_action(PruneProgress::TelegramDeleted), PruneAction::FinalizeGallery);
    assert_eq!(next_prune_action(PruneProgress::Complete), PruneAction::FinishReview);
}

#[test]
fn only_message_not_found_is_idempotent_success() {
    assert!(is_idempotent_delete_error("Bad Request: message to delete not found"));
    assert!(!is_idempotent_delete_error("Forbidden: bot is not an administrator"));
}
```

- [ ] **Step 3: Run RED**

Run: `cargo test work_prune -- --nocapture`.

- [ ] **Step 4: Implement bounded Vitrine client types**

Use existing bearer auth and retryable HTTP classification. Require 1 to 20 losing work IDs and non-empty returned Telegram targets.

- [ ] **Step 5: Replace image-key callback path with saga**

For `ConfirmKeep(post_index)`: derive work IDs, call Vitrine, delete every returned `(chat_id,message_id)`, finalize Vitrine, remove local fingerprints by losing work IDs, finish review, then delete private approval messages.

- [ ] **Step 6: Preserve review and fingerprints on failure**

Best-effort report a sanitized error, restore review to pending, and keep fingerprints. Do not remove private approval controls after D1/R2 success alone.

- [ ] **Step 7: Verify and commit**

Run `cargo test work_prune -- --nocapture`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`.

```bash
git add src/gallery.rs src/sink/telegram.rs src/similar_review.rs tests/similar_review.rs
git commit -m "feat: delete duplicate works and channel posts together"
```

---

### Task 9: Build exact historical publication backfill tool

**Files:**
- Create: `/Users/furina/Documents/Github/hanabi/tools/backfill_telegram_publications.py`
- Create: `/Users/furina/Documents/Github/hanabi/tests/test_backfill_telegram_publications.py`
- Modify: `/Users/furina/Documents/Github/hanabi/README.md`

**Interfaces:**
- Consumes `tdl chat export --all --with-content` JSON and authenticated Vitrine catalog JSON.
- Produces `matched`, `ambiguous`, and `missing` manifest arrays.
- Dry-run is default; `--apply` calls `PUT /api/catalog/publications` only for exact rows.

- [ ] **Step 1: Add RED exact-match and ambiguity tests**

```python
def test_groups_album_and_matches_exact_work(self):
    messages = [
        {"id": 41, "chat_id": -100123, "media_group_id": "g1", "text": "From https://www.pixiv.net/artworks/147342918"},
        {"id": 42, "chat_id": -100123, "media_group_id": "g1", "text": ""},
    ]
    result = build_manifest(messages, {"pixiv:147342918"})
    self.assertEqual(result["matched"][0]["message_ids"], [41, 42])

def test_ambiguous_source_is_never_applied(self):
    messages = [
        {"id": 41, "chat_id": -100123, "media_group_id": "g1", "text": "https://www.pixiv.net/artworks/1"},
        {"id": 51, "chat_id": -100123, "media_group_id": "g2", "text": "https://www.pixiv.net/artworks/1"},
    ]
    result = build_manifest(messages, {"pixiv:1"})
    self.assertEqual(result["matched"], [])
    self.assertEqual(result["ambiguous"][0]["work_id"], "pixiv:1")
```

- [ ] **Step 2: Run RED**

Run: `python3 -m unittest tests/test_backfill_telegram_publications.py -v`.

- [ ] **Step 3: Implement strict canonicalization**

Support exact Pixiv artwork IDs, X status IDs, and Douyin note/slides IDs. Strip query strings. Never match titles or authors.

- [ ] **Step 4: Implement album grouping and ambiguity blocking**

One captioned message plus messages sharing its non-empty media group is one publication. A non-album message is one publication. Do not infer uncaptioned later batches by adjacency.

- [ ] **Step 5: Implement safe apply mode**

Read bearer token only from environment. Refuse apply if any requested work is ambiguous. Output counts and work IDs, never credentials.

- [ ] **Step 6: Verify and commit**

Run `python3 -m unittest tests/test_backfill_telegram_publications.py -v`, `python3 -m py_compile tools/backfill_telegram_publications.py`, `cargo test --all-targets`, and `git diff --check`.

```bash
git add tools/backfill_telegram_publications.py tests/test_backfill_telegram_publications.py README.md
git commit -m "feat: backfill exact channel publication mappings"
```

---

### Task 10: Final local integration and release preparation

**Files:**
- Modify: `/Users/furina/Documents/Github/hanabi/CHANGELOG.md`
- Modify: `/Users/furina/Documents/Github/hanabi/Cargo.toml`
- Modify: `/Users/furina/Documents/Github/hanabi/Cargo.lock`
- Modify: `/Users/furina/Documents/Github/vitrine/README.md`

**Interfaces:**
- Hanabi candidate version is `0.11.0`.
- Produces local verification evidence only.

- [ ] **Step 1: Set Hanabi version 0.11.0 and document behavior**

Document D1 mapping, whole-work deletion, retries, and blocking of unmapped legacy works.

- [ ] **Step 2: Run fresh complete gates**

Vitrine:

```bash
npm run check
git diff --check
```

Hanabi:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
python3 -m unittest tests/test_backfill_telegram_publications.py -v
git diff --check
```

- [ ] **Step 3: Audit state and preserve user changes**

```bash
git -C /Users/furina/Documents/Github/hanabi status --short --branch
git -C /Users/furina/Documents/Github/vitrine status --short --branch
git -C /Users/furina/Documents/Github/vitrine diff -- wrangler.jsonc
```

Expected: the original Vitrine `wrangler.jsonc` diff remains and is absent from Grok commits.

- [ ] **Step 4: Commit release files exactly**

```bash
git -C /Users/furina/Documents/Github/hanabi add CHANGELOG.md Cargo.toml Cargo.lock
git -C /Users/furina/Documents/Github/hanabi commit -m "chore: prepare Hanabi v0.11.0"
git -C /Users/furina/Documents/Github/vitrine add README.md
git -C /Users/furina/Documents/Github/vitrine commit -m "docs: document linked publication pruning"
```

- [ ] **Step 5: Stop before production**

Report commit ranges, tests, and remaining dirty files. Do not push, tag, migrate remote D1, backfill live mappings, deploy, SSH, or mutate Telegram.

---

## Operator-only deployment and acceptance checkpoint

This section is excluded from Grok execution.

1. Independently review both repository diffs and commits.
2. Export live D1, inventory controlled R2 objects, and back up Hanabi SQLite/config/Compose.
3. Push reviewed commits and wait for both CI suites.
4. Apply D1 migration 0004 remotely before deploying code that writes new tables.
5. Deploy Vitrine first and verify health/authenticated new routes without mutation.
6. Run Telegram backfill dry-run; require zero ambiguous/missing current-review works before apply.
7. Build Hanabi v0.11.0 natively on Oracle ARM64 and switch its immutable tag.
8. Use one explicitly approved review as the destructive acceptance case.
9. Prove losing works hidden in D1, all original R2 URLs 404, rollback objects present, mapped channel messages absent, retained work/messages intact, D1 receipt complete, SQLite healthy, and no stale active references.
