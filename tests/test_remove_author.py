"""remove_author.py 的 X 分支：用假的 gallery-dl 客户端，不发任何真实请求。"""
import importlib.util
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import patch

_twitter = types.ModuleType("gallery_dl.extractor.twitter")
_twitter.TwitterAPI = object
_stubs = {
    "gallery_dl": types.ModuleType("gallery_dl"),
    "gallery_dl.config": types.ModuleType("gallery_dl.config"),
    "gallery_dl.extractor": types.ModuleType("gallery_dl.extractor"),
    "gallery_dl.extractor.twitter": _twitter,
}
_stubs["gallery_dl"].config = _stubs["gallery_dl.config"]
_stubs["gallery_dl"].extractor = _stubs["gallery_dl.extractor"]
with patch.dict(sys.modules, _stubs):
    SPEC = importlib.util.spec_from_file_location(
        "remove_author", Path(__file__).resolve().parents[1] / "tools" / "remove_author.py"
    )
    MODULE = importlib.util.module_from_spec(SPEC)
    SPEC.loader.exec_module(MODULE)


class FakeResponse:
    def __init__(self, status_code, body):
        self.status_code = status_code
        self.body = body

    def json(self):
        return self.body


class FakeExtractor:
    def __init__(self, response, member_rounds):
        self.response = response
        self.member_rounds = member_rounds
        self.posts = 0

    def initialize(self):
        pass

    def request(self, url, **kwargs):
        self.posts += 1
        return self.response


class FakeAPI:
    root = "https://x.com/i/api"
    headers = {}

    def __init__(self, ext):
        self.ext = ext

    def _user_id_by_screen_name(self, screen_name):
        return "42"

    def _transaction_id(self, url, method):
        pass

    def list_members(self, list_id):
        return [{"rest_id": rest_id} for rest_id in self.ext.member_rounds.pop(0)]


DECODE_ERROR = {"errors": [{"message": "BadRequest: com.twitter.strato.serialization.DecodeException"}]}


class RemoveFromXListTest(unittest.TestCase):
    def run_remove(self, response, member_rounds):
        ext = FakeExtractor(response, member_rounds)
        with patch.object(MODULE, "TwitterAPI", FakeAPI), patch.object(
            MODULE, "extractor", types.SimpleNamespace(find=lambda url: ext)
        ):
            return MODULE.remove_from_x_list("1", "artist"), ext

    def test_error_response_counts_as_removed_once_the_member_is_gone(self):
        removed, ext = self.run_remove(FakeResponse(200, DECODE_ERROR), [["42", "7"], ["7"]])
        self.assertTrue(removed)
        self.assertEqual(ext.posts, 1)

    def test_error_response_with_member_still_listed_raises(self):
        with self.assertRaisesRegex(RuntimeError, "DecodeException"):
            self.run_remove(FakeResponse(200, DECODE_ERROR), [["42"], ["42"]])

    def test_non_member_is_not_posted(self):
        removed, ext = self.run_remove(FakeResponse(200, {"data": {}}), [["7"]])
        self.assertFalse(removed)
        self.assertEqual(ext.posts, 0)


if __name__ == "__main__":
    unittest.main()
