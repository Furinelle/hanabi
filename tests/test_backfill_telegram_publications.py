import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from tools.backfill_telegram_publications import build_manifest, normalize_export_messages


class BackfillTelegramPublicationsTest(unittest.TestCase):
    def test_groups_album_and_matches_exact_work(self):
        messages = [
            {
                "id": 41,
                "chat_id": -100123,
                "media_group_id": "g1",
                "text": "From https://www.pixiv.net/artworks/147342918",
            },
            {"id": 42, "chat_id": -100123, "media_group_id": "g1", "text": ""},
        ]
        result = build_manifest(messages, {"pixiv:147342918"})
        self.assertEqual(result["matched"][0]["message_ids"], [41, 42])

    def test_ambiguous_source_is_never_applied(self):
        messages = [
            {
                "id": 41,
                "chat_id": -100123,
                "media_group_id": "g1",
                "text": "https://www.pixiv.net/artworks/1",
            },
            {
                "id": 51,
                "chat_id": -100123,
                "media_group_id": "g2",
                "text": "https://www.pixiv.net/artworks/1",
            },
        ]
        result = build_manifest(messages, {"pixiv:1"})
        self.assertEqual(result["matched"], [])
        self.assertEqual(result["ambiguous"][0]["work_id"], "pixiv:1")

    def test_query_string_is_stripped_and_uncaptioned_later_batch_is_not_inferred(self):
        messages = [
            {
                "id": 41,
                "chat_id": -100123,
                "media_group_id": "g1",
                "text": "https://www.pixiv.net/artworks/2?utm=1",
            },
            {"id": 42, "chat_id": -100123, "media_group_id": "g1", "text": ""},
            {"id": 43, "chat_id": -100123, "media_group_id": "g2", "text": ""},
        ]
        result = build_manifest(messages, {"pixiv:2"})
        self.assertEqual(result["matched"][0]["message_ids"], [41, 42])
        self.assertNotIn(43, result["matched"][0]["message_ids"])

    def test_normalizes_tdl_raw_channel_album(self):
        messages = normalize_export_messages(
            [
                {
                    "id": 41,
                    "type": "message",
                    "raw": {
                        "ID": 41,
                        "GroupedID": 9001,
                        "Message": "source",
                        "PeerID": {"ChannelID": 3664074984},
                        "Entities": [
                            {
                                "Offset": 0,
                                "Length": 6,
                                "URL": "https://www.pixiv.net/artworks/147342918",
                            }
                        ],
                    },
                },
                {
                    "id": 42,
                    "type": "message",
                    "raw": {
                        "ID": 42,
                        "GroupedID": 9001,
                        "Message": "",
                        "PeerID": {"ChannelID": 3664074984},
                        "Entities": None,
                    },
                },
            ]
        )
        self.assertEqual(messages[0]["chat_id"], -1003664074984)
        result = build_manifest(messages, {"pixiv:147342918"})
        self.assertEqual(result["matched"][0]["message_ids"], [41, 42])

    def test_rejects_minimal_tdl_export_without_raw_fields(self):
        with self.assertRaisesRegex(SystemExit, "rerun with --raw"):
            normalize_export_messages(
                [{"id": 41, "type": "message", "text": "https://example.com"}]
            )


if __name__ == "__main__":
    unittest.main()
