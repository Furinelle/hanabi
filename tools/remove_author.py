#!/usr/bin/env python3
"""Remove an excluded author from the account-side feed Hanabi watches.

X: remove the user from a List.  Pixiv: unfollow the user.

Usage:
    remove_author.py <list-owner gallery-dl.conf> x <list_id> <screen_name>
    remove_author.py <gallery-dl.conf> pixiv <user_id>

Reuses gallery-dl's authenticated API clients, so cookies / refresh tokens stay
in the gallery-dl config and are never printed.  The X config must hold the
cookies of the List owner; other accounts get "not allowed to update this list".
Prints one JSON object on stdout: {"removed": true|false} (false = was not a
member / not followed).
Errors go to stderr with a non-zero exit code.
"""

from __future__ import annotations

import json
import sys

from gallery_dl import config, extractor
from gallery_dl.extractor.twitter import TwitterAPI

# shortcut: X 网页版更新后 queryId 可能失效（接口返回 4xx），需从网页 JS 重新提取。
X_LIST_REMOVE_MEMBER = "_2o1-wSMWGFSi5HUscn1Hw"
X_FEATURES = {
    "profile_label_improvements_pcf_label_in_post_enabled": True,
    "responsive_web_profile_redirect_enabled": False,
    "rweb_tipjar_consumption_enabled": True,
    "verified_phone_label_enabled": False,
    "responsive_web_graphql_timeline_navigation_enabled": True,
}


def x_list_members(api: TwitterAPI, list_id: str) -> set[str]:
    return {user["rest_id"] for user in api.list_members(list_id)}


def remove_from_x_list(list_id: str, screen_name: str) -> bool:
    ext = extractor.find(f"https://x.com/i/lists/{list_id}")
    ext.initialize()
    api = TwitterAPI(ext)
    user_id = api._user_id_by_screen_name(screen_name)
    if user_id not in x_list_members(api, list_id):
        return False
    url = f"{api.root}/graphql/{X_LIST_REMOVE_MEMBER}/ListRemoveMember"
    api._transaction_id(url, "POST")
    response = ext.request(
        url,
        method="POST",
        headers=api.headers,
        json={
            "variables": {"listId": list_id, "userId": user_id},
            "features": X_FEATURES,
            "queryId": X_LIST_REMOVE_MEMBER,
        },
        fatal=False,
    )
    try:
        data = response.json()
    except ValueError:
        data = {}
    if response.status_code >= 400 or data.get("errors") or "data" not in data:
        errors = "; ".join(e.get("message", "") for e in data.get("errors", ()))
        raise RuntimeError(f"ListRemoveMember {response.status_code}: {errors}")
    return True


def unfollow_pixiv(user_id: str) -> bool:
    ext = extractor.find(f"https://www.pixiv.net/users/{user_id}/artworks")
    ext.initialize()
    api = ext.api
    if not api.user_detail(user_id)["user"].get("is_followed"):
        return False
    api.login()
    response = ext.request(
        "https://app-api.pixiv.net/v1/user/follow/delete",
        method="POST",
        data={"user_id": user_id},
        fatal=False,
    )
    if response.status_code >= 400:
        raise RuntimeError(f"pixiv follow/delete {response.status_code}")
    return True


def main(argv: list[str]) -> int:
    match argv:
        case [conf, "x", list_id, screen_name]:
            config.load([conf], strict=True)
            removed = remove_from_x_list(list_id, screen_name)
        case [conf, "pixiv", user_id]:
            config.load([conf], strict=True)
            removed = unfollow_pixiv(user_id)
        case _:
            print(__doc__, file=sys.stderr)
            return 2
    print(json.dumps({"removed": removed}))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except Exception as error:  # noqa: BLE001 - report any failure to Hanabi
        print(f"{type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
