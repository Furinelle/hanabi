# D1-backed Telegram publication deletion for similarity reviews

Date: 2026-08-30

## Context

Hanabi v0.10.6 reviews similar media by source post. Selecting one post keeps every differential page in that post and removes the losing posts from Vitrine. The current prune path is still image-key based, however, and Hanabi does not persist channel message IDs after a successful publication. Vitrine D1 therefore cannot identify the Telegram channel messages that belong to a losing work.

Telegram Bot API cannot search channel history by source URL. Reliable linked deletion requires publication IDs to be captured when messages are sent, persisted in Cloudflare D1, and included in an idempotent whole-work deletion workflow.

## Goals

- Store Telegram channel publication mappings in Vitrine D1 as the authority.
- Treat a similarity choice as a whole-work decision across D1, R2, and Telegram.
- Delete every image and every channel message for each losing work.
- Keep R2 rollback copies and durable receipts before destructive changes.
- Make retries safe when D1, R2, Telegram, or the completion callback fails.
- Backfill exact mappings for existing similarity-review works where channel history proves a unique match.
- Block deletion when a losing work lacks a complete, unambiguous Telegram mapping.

## Non-goals

- No gallery-catalog performance work.
- No Vitrine Worker access to the Telegram bot token.
- No fuzzy deletion by title, author, timestamp, or image similarity.
- No automatic deletion of historical channel messages whose mapping cannot be proven.
- No generic rollback of arbitrary Telegram publications.

## Chosen architecture

Hanabi orchestrates Telegram operations. Vitrine owns work, R2, publication mapping, and deletion-receipt state in D1. The Worker never receives the Telegram bot token.

The workflow is a durable saga rather than a distributed transaction:

1. Vitrine validates and backs up every losing work, writes the whole deletion plan and Telegram targets to D1, hides the works, and removes their active R2 objects.
2. Hanabi deletes the returned Telegram messages.
3. Hanabi reports Telegram completion to Vitrine.
4. Hanabi marks the similarity review decided only after the completion report succeeds.

If a later stage fails, the review returns to `pending`. Replaying the same decision returns the stored plan and retries the incomplete stage.

## D1 schema

Migration `0004_telegram_publications.sql` adds the following tables.

### `telegram_publications`

One work may have more than one channel publication, so `work_id` is not the primary key.

| Column | Type | Meaning |
| --- | --- | --- |
| `id` | TEXT PRIMARY KEY | Stable publication ID derived from work, chat, and anchor message |
| `work_id` | TEXT NOT NULL | Vitrine work ID such as `pixiv:147342918` |
| `chat_id` | INTEGER NOT NULL | Numeric Telegram channel chat ID returned by Telegram |
| `anchor_message_id` | INTEGER NOT NULL | First channel message in this publication |
| `message_ids_json` | TEXT NOT NULL | Every channel message ID returned for all album batches |
| `publish_state` | TEXT NOT NULL | `full` or `partial` |
| `created_at` | TEXT NOT NULL | Publication time |
| `deleted_at` | TEXT | Set after Telegram deletion is confirmed |

Constraints and indexes:

- `UNIQUE(work_id, chat_id, anchor_message_id)`
- index on `(work_id, deleted_at)`
- message IDs must be non-empty, unique positive integers
- the anchor must equal the first stored message ID

Mappings are retained after a work is soft-deleted so retries and audit remain possible.

### `catalog_work_prune_receipts`

| Column | Type | Meaning |
| --- | --- | --- |
| `decision_id` | TEXT PRIMARY KEY | Hanabi similarity token scoped as an immutable idempotency key |
| `keep_work_id` | TEXT NOT NULL | Work retained by the user |
| `remove_work_ids_json` | TEXT NOT NULL | Entire losing works |
| `removed_r2_keys_json` | TEXT NOT NULL | All active R2 keys belonging to losing works |
| `telegram_targets_json` | TEXT NOT NULL | D1 publication IDs, chat IDs, and message IDs |
| `telegram_state` | TEXT NOT NULL | `pending` or `complete` |
| `telegram_error` | TEXT | Last reported Telegram error, sanitized |
| `created_at` | TEXT NOT NULL | Plan creation time |
| `telegram_completed_at` | TEXT | Completion time |

Existing `catalog_prune_backups` remains the per-object rollback inventory. The existing image-key prune endpoint stays available for compatibility but Hanabi similarity reviews stop using it.

## Publication capture and ingest

### Telegram result type

Hanabi replaces its transient `Vec<MessageId>` publication result with a structured value containing:

- numeric channel `chat_id` from Telegram responses
- every returned message ID across all batches
- `full` or `partial` publication state

The first message remains the discussion/comment anchor. No message ID is inferred from sequence arithmetic.

### Vitrine ingest metadata

`GalleryClient::ingest` accepts an optional publication object. Vitrine validates it and upserts `telegram_publications` in the same D1 batch that commits the work, images, tags, idempotency receipt, and audit.

The publication data is part of the ingest fingerprint and stable idempotency payload. A successful replay therefore refers to the same publication mapping.

### Gallery compensation outbox

The local `gallery_outbox` schema, queued-work structure, and on-disk manifest gain a backwards-compatible `publication_json` field. Failed Vitrine ingestion retains both media and Telegram mapping until the D1 batch succeeds. Legacy rows default to no publication.

Only flows that actually ingest a Vitrine work write a D1 mapping. Channel-only approvals have no Vitrine work and are outside this deletion feature.

## Whole-work prune API

### Request

`POST /api/catalog/prune-works`, protected by the existing ingest bearer token:

```json
{
  "decision_id": "hanabi-similar-91",
  "keep_work_id": "pixiv:147342918",
  "remove_work_ids": ["douyin:7669678713420921673"]
}
```

Validation requires:

- one active keep work
- 1 to 20 distinct active losing works
- keep work not present in the remove list
- at least one active D1 publication mapping with a complete captured message-ID set for every losing work; both fully and partially published channel posts are valid because Hanabi records every message that Telegram actually created
- no empty work IDs or unbounded payloads

### First execution

Vitrine performs these steps:

1. Load every active image, tag association, and active publication for every losing work.
2. Reject before mutation if any publication mapping is missing or invalid.
3. Copy every losing R2 object to `review-trash/<decision_id>/<original-key>`.
4. Build the immutable receipt, including every Telegram target.
5. In one D1 batch:
   - insert all `catalog_prune_backups`
   - delete all losing `images` rows
   - delete losing `work_tags` associations and maintain tag usage counts
   - soft-delete each losing `works` row with `deleted_at`
   - insert `catalog_work_prune_receipts` with `telegram_state='pending'`
6. Delete the original R2 objects.
7. Return the stored Telegram targets.

The response includes removed work IDs, removed R2 keys, Telegram targets, and `replayed=false`.

### Replay

If `decision_id` already exists, Vitrine verifies that keep/remove inputs match the stored plan, retries deletion of the stored original R2 keys, and returns the stored Telegram targets with `replayed=true`. A conflicting reuse returns HTTP 409.

### Telegram completion

`POST /api/catalog/prune-works/telegram-result` accepts:

```json
{
  "decision_id": "hanabi-similar-91",
  "complete": true
}
```

On success, D1 marks every target publication `deleted_at`, sets the receipt to `telegram_state='complete'`, and records the completion time. A failed attempt may report a sanitized error while leaving the receipt pending.

## Hanabi similarity approval flow

For `ConfirmKeep(post_index)`:

1. Derive one retained `work_id` and the complete set of losing `work_ids` from the review payload.
2. Atomically claim the local review as `processing`.
3. Call Vitrine `prune-works` with the stable decision ID.
4. For every returned publication target, call Telegram `deleteMessage` for every stored message ID.
5. Treat Telegram `message to delete not found` as idempotent success; retry rate limits and transient server errors with the existing Telegram retry policy.
6. Call the Vitrine Telegram-completion endpoint.
7. Remove all losing local fingerprints by `work_id`.
8. Mark the review `decided` and remove its private approval album/control messages.

If steps 3 through 6 fail, Hanabi restores the review to `pending` and sends a concise failure notice. It does not remove local fingerprints or finish the review. A retry replays the stored Vitrine receipt and safely repeats Telegram deletion.

The order deliberately commits recoverable D1/R2 state before Telegram deletion. Deleting Telegram first would be irreversible if the D1 mutation then failed.

## Existing publication backfill

Backfill targets the active works referenced by current similarity reviews before enabling destructive buttons.

1. Export the configured publication channel with the user's existing authenticated Telegram client.
2. Parse the exact source link from the first captioned message and canonicalize it to `source:source_id`.
3. Use Telegram `media_group_id` to collect every message in an album.
4. Match the canonical work ID to an active Vitrine D1 work.
5. Write mappings through a protected `PUT /api/catalog/publications` endpoint.
6. Produce a manifest of matched, ambiguous, and missing works.

Only exact unique matches are written. Multi-batch historical posts whose uncaptioned later batches cannot be proven remain incomplete and are blocked from deletion. The operator may resolve them by providing exact channel message links; fuzzy title/author matching is prohibited.

## Security

- Telegram bot credentials remain only in Hanabi.
- Vitrine endpoints use the existing bearer-auth boundary and explicit Hanabi user agent.
- D1 stores numeric chat/message IDs but no Telegram token.
- API errors and receipts never store tokens, cookies, or response bodies containing secrets.
- All destructive requests are bounded and idempotent.

## Compatibility and repository safety

- D1 changes are additive through migration 0004.
- Legacy Vitrine works without mappings remain readable but cannot be removed by whole-work similarity prune.
- Legacy Hanabi outbox rows deserialize with no publication mapping.
- Existing image-key prune receipts and API behavior remain unchanged.
- The current unrelated local `vitrine/wrangler.jsonc` binding change must be preserved and not overwritten during implementation.

## Testing

### Hanabi

- Publication capture records numeric chat ID and every batch message ID.
- Full and partial sends produce correct publication state.
- Outbox restart/retry preserves publication mapping.
- A two-post, two-page-per-post review sends whole work IDs to Vitrine.
- Telegram deletion covers every returned message ID.
- `message not found` is idempotent success.
- A Telegram or completion-callback failure restores the review and preserves local fingerprints.
- Successful completion removes only losing-work fingerprints.

### Vitrine

- Migration creates both D1 tables and indexes.
- Ingest validates and atomically upserts publication mappings.
- Whole-work prune rejects missing or incomplete mappings before any mutation.
- Whole-work prune backs up and removes every image for losing works, including pages not present in the similarity pair.
- Replay returns the same targets and rejects conflicting decision reuse.
- Completion marks publication and receipt state atomically.
- Existing image-key prune tests remain unchanged.

### Live acceptance

- Back up Vitrine D1/R2 manifests and Hanabi SQLite/config/Compose before deployment.
- Backfill current-review mappings and require zero ambiguous/missing targets before enabling their destructive actions.
- On a controlled review, verify losing work hidden from APIs, every D1 association removed or soft-deleted as designed, original R2 URLs return 404, rollback objects exist, all mapped Telegram messages are absent, the retained work remains intact, and receipt state is complete.
- Verify Worker health, Hanabi container revision/restart count, SQLite `quick_check`, no missing pending media, and no fresh error/panic logs.

## Rollback

- R2 originals are restorable from `catalog_prune_backups` under `review-trash`.
- D1 receipt records the exact losing works, associations, objects, and publication targets needed for a scoped restoration tool.
- Telegram deletion is not reversible; this is why incomplete or ambiguous historical mappings are blocked before mutation and why Telegram runs only after D1/R2 backup succeeds.
- Deployment rollback restores the previous immutable Vitrine Worker version and Hanabi image, but completed Telegram deletions require manual reposting rather than message restoration.

## Acceptance criteria

- D1 is the authoritative publication map.
- Every destructive similarity decision operates on complete losing works.
- No losing work can be pruned without an exact active Telegram mapping.
- Repeating the same decision is safe after any intermediate failure.
- The retained work's D1 rows, R2 objects, channel messages, and local fingerprints remain untouched.
- Existing and future mapped works use the same deletion path.
