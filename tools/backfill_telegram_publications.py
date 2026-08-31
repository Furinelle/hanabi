#!/usr/bin/env python3
"""Exact historical Telegram publication matching for Vitrine D1 backfill.

Dry-run is default. --apply writes only unique exact matches through
PUT /api/catalog/publications. Credentials are read from the environment
and never printed.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from typing import Any
from urllib.parse import urlparse


PIXIV_ARTWORK = re.compile(r"/artworks/(\d+)", re.IGNORECASE)
X_STATUS = re.compile(r"/(?:status|i/status)/(\d+)", re.IGNORECASE)
DOUYIN_NOTE = re.compile(r"/(?:note|slides)/(\d+)", re.IGNORECASE)
URL_RE = re.compile(r"https?://[^\s<>()]+", re.IGNORECASE)


def canonicalize_source_url(url: str) -> str | None:
    parsed = urlparse(url.strip())
    host = (parsed.netloc or "").lower()
    if host.startswith("www."):
        host = host[4:]
    path = parsed.path or ""
    if host in {"pixiv.net", "www.pixiv.net"} or host.endswith(".pixiv.net"):
        match = PIXIV_ARTWORK.search(path)
        if match:
            return f"pixiv:{match.group(1)}"
        return None
    if host in {"x.com", "twitter.com", "www.x.com", "www.twitter.com"} or host.endswith(
        ".x.com"
    ) or host.endswith(".twitter.com"):
        match = X_STATUS.search(path)
        if match:
            return f"x:{match.group(1)}"
        return None
    if "douyin.com" in host:
        match = DOUYIN_NOTE.search(path)
        if match:
            return f"douyin:{match.group(1)}"
        return None
    return None


def extract_work_ids(text: str) -> list[str]:
    found: list[str] = []
    for raw in URL_RE.findall(text or ""):
        cleaned = raw.rstrip(".,);]}")
        work_id = canonicalize_source_url(cleaned)
        if work_id and work_id not in found:
            found.append(work_id)
    return found


def message_text(message: dict[str, Any]) -> str:
    for key in ("text", "caption", "message"):
        value = message.get(key)
        if isinstance(value, str) and value.strip():
            return value
    return ""


def group_publications(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    by_group: dict[tuple[int, str], list[dict[str, Any]]] = defaultdict(list)
    singles: list[dict[str, Any]] = []
    for message in messages:
        group_id = str(message.get("media_group_id") or "").strip()
        chat_id = message.get("chat_id")
        if group_id and chat_id is not None:
            by_group[(int(chat_id), group_id)].append(message)
        else:
            singles.append(message)

    publications: list[dict[str, Any]] = []
    for (chat_id, _group_id), group in by_group.items():
        group.sort(key=lambda item: int(item.get("id") or 0))
        captioned = [item for item in group if extract_work_ids(message_text(item))]
        if len(captioned) != 1:
            continue
        work_ids = extract_work_ids(message_text(captioned[0]))
        if len(work_ids) != 1:
            continue
        publications.append(
            {
                "work_id": work_ids[0],
                "chat_id": chat_id,
                "message_ids": [int(item["id"]) for item in group if item.get("id") is not None],
                "publish_state": "full",
            }
        )

    for message in singles:
        work_ids = extract_work_ids(message_text(message))
        if len(work_ids) != 1 or message.get("id") is None or message.get("chat_id") is None:
            continue
        publications.append(
            {
                "work_id": work_ids[0],
                "chat_id": int(message["chat_id"]),
                "message_ids": [int(message["id"])],
                "publish_state": "full",
            }
        )
    return publications


def build_manifest(
    messages: list[dict[str, Any]],
    active_work_ids: set[str],
) -> dict[str, list[dict[str, Any]]]:
    publications = group_publications(messages)
    by_work: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for publication in publications:
        if publication["work_id"] in active_work_ids:
            by_work[publication["work_id"]].append(publication)

    matched: list[dict[str, Any]] = []
    ambiguous: list[dict[str, Any]] = []
    for work_id, rows in sorted(by_work.items()):
        if len(rows) == 1:
            matched.append(rows[0])
        else:
            ambiguous.append({"work_id": work_id, "publications": rows})

    matched_ids = {row["work_id"] for row in matched}
    ambiguous_ids = {row["work_id"] for row in ambiguous}
    missing = [
        {"work_id": work_id}
        for work_id in sorted(active_work_ids)
        if work_id not in matched_ids and work_id not in ambiguous_ids
    ]
    return {"matched": matched, "ambiguous": ambiguous, "missing": missing}


def telegram_peer_chat_id(peer: Any) -> int | None:
    if not isinstance(peer, dict):
        return None
    channel_id = peer.get("ChannelID")
    if isinstance(channel_id, int) and channel_id > 0:
        return -int(f"100{channel_id}")
    chat_id = peer.get("ChatID")
    if isinstance(chat_id, int) and chat_id > 0:
        return -chat_id
    user_id = peer.get("UserID")
    if isinstance(user_id, int) and user_id > 0:
        return user_id
    return None


def normalize_export_messages(messages: list[Any]) -> list[dict[str, Any]]:
    normalized: list[dict[str, Any]] = []
    for item in messages:
        if not isinstance(item, dict):
            raise SystemExit("export contains a non-object message")
        if isinstance(item.get("id"), int) and isinstance(item.get("chat_id"), int):
            normalized.append(item)
            continue
        raw = item.get("raw")
        if not isinstance(raw, dict):
            raise SystemExit("tdl export lacks raw Telegram fields; rerun with --raw")
        message_id = raw.get("ID")
        chat_id = telegram_peer_chat_id(raw.get("PeerID"))
        if not isinstance(message_id, int) or message_id <= 0 or chat_id is None:
            raise SystemExit("tdl raw export contains an invalid message identity")
        grouped_id = raw.get("GroupedID")
        visible_text = raw.get("Message") if isinstance(raw.get("Message"), str) else ""
        entity_urls = [
            entity["URL"]
            for entity in (raw.get("Entities") or [])
            if isinstance(entity, dict)
            and isinstance(entity.get("URL"), str)
            and entity["URL"].strip()
        ]
        normalized.append(
            {
                "id": message_id,
                "chat_id": chat_id,
                "media_group_id": str(grouped_id)
                if isinstance(grouped_id, int) and grouped_id > 0
                else "",
                "text": "\n".join([visible_text, *entity_urls]),
            }
        )
    return normalized


def load_messages(path: str) -> list[dict[str, Any]]:
    with open(path, encoding="utf-8") as handle:
        payload = json.load(handle)
    if isinstance(payload, list):
        return normalize_export_messages(payload)
    if isinstance(payload, dict):
        for key in ("messages", "data", "items"):
            value = payload.get(key)
            if isinstance(value, list):
                return normalize_export_messages(value)
    raise SystemExit("export JSON must be a message list or an object with messages")


def load_active_work_ids(path: str) -> set[str]:
    with open(path, encoding="utf-8") as handle:
        payload = json.load(handle)
    works: list[Any]
    if isinstance(payload, list):
        works = payload
    elif isinstance(payload, dict):
        works = payload.get("works") or payload.get("images") or []
    else:
        works = []
    ids: set[str] = set()
    for item in works:
        if isinstance(item, str) and ":" in item:
            ids.add(item)
        elif isinstance(item, dict):
            work_id = item.get("work_id") or item.get("id")
            if isinstance(work_id, str) and ":" in work_id:
                ids.add(work_id)
    return ids


def apply_matches(endpoint: str, token: str, rows: list[dict[str, Any]]) -> None:
    endpoint = endpoint.rstrip("/")
    for row in rows:
        request = urllib.request.Request(
            f"{endpoint}/api/catalog/publications",
            data=json.dumps(
                {
                    "work_id": row["work_id"],
                    "chat_id": row["chat_id"],
                    "message_ids": row["message_ids"],
                    "publish_state": row["publish_state"],
                }
            ).encode("utf-8"),
            method="PUT",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "User-Agent": "hanabi-backfill/0.11.0",
            },
        )
        try:
            with urllib.request.urlopen(request) as response:
                response.read()
        except urllib.error.HTTPError as error:
            raise SystemExit(f"apply failed for {row['work_id']}: HTTP {error.code}") from error


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Match tdl channel export messages to Vitrine works without fuzzy lookup."
    )
    parser.add_argument("--export", required=True, help="tdl chat export JSON")
    parser.add_argument("--catalog", required=True, help="authenticated Vitrine catalog JSON")
    parser.add_argument("--apply", action="store_true", help="write exact unique matches")
    parser.add_argument("--endpoint", default="", help="Vitrine origin for --apply")
    args = parser.parse_args(argv)

    messages = load_messages(args.export)
    active_work_ids = load_active_work_ids(args.catalog)
    manifest = build_manifest(messages, active_work_ids)
    print(
        json.dumps(
            {
                "matched_count": len(manifest["matched"]),
                "ambiguous_count": len(manifest["ambiguous"]),
                "missing_count": len(manifest["missing"]),
                "matched_work_ids": [row["work_id"] for row in manifest["matched"]],
                "ambiguous_work_ids": [row["work_id"] for row in manifest["ambiguous"]],
                "missing_work_ids": [row["work_id"] for row in manifest["missing"]],
            },
            ensure_ascii=False,
            indent=2,
        )
    )
    if not args.apply:
        return 0
    if manifest["ambiguous"]:
        print("refusing apply: ambiguous works are present", file=sys.stderr)
        return 2
    token = os.environ.get("HANABI_GALLERY_TOKEN") or os.environ.get("VITRINE_INGEST_TOKEN") or ""
    if not token or not args.endpoint:
        print("refusing apply: endpoint and HANABI_GALLERY_TOKEN are required", file=sys.stderr)
        return 2
    apply_matches(args.endpoint, token, manifest["matched"])
    print(f"applied {len(manifest['matched'])} exact mappings")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
