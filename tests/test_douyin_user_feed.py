import importlib.util
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "douyin_user_feed", Path(__file__).parents[1] / "tools/douyin_user_feed.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class BrowserAwemeTest(unittest.TestCase):
    def test_selects_only_the_requested_aweme(self):
        wanted = {"aweme_id": "2", "images": [{}]}
        self.assertIs(
            MODULE._browser_aweme(
                {"aweme_list": [{"aweme_id": "1"}, wanted]}, "2"
            ),
            wanted,
        )
        self.assertIsNone(MODULE._browser_aweme({"aweme_detail": wanted}, "1"))


if __name__ == "__main__":
    unittest.main()
