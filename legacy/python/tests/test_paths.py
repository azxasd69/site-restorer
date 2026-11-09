"""Unit tests for URL -> local path mapping (determinism, Windows limits)."""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src"))

from core import MAX_REL_LEN, url_to_local_path  # noqa: E402


class PathTests(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(url_to_local_path("https://example.com/catalog/tovar/"),
                         os.path.join("catalog", "tovar", "index.html"))

    def test_root(self):
        self.assertEqual(url_to_local_path("https://example.com/"), "index.html")

    def test_query_is_hashed_and_stable(self):
        a = url_to_local_path("https://example.com/list?page=2&sort=asc")
        b = url_to_local_path("https://example.com/list?page=2&sort=asc")
        self.assertEqual(a, b)
        self.assertIn("__q_", a)

    def test_different_queries_differ(self):
        a = url_to_local_path("https://example.com/list?page=2")
        b = url_to_local_path("https://example.com/list?page=3")
        self.assertNotEqual(a, b)

    def test_long_path_is_capped(self):
        deep = "https://example.com/" + "/".join(["segment%02d" % i for i in range(60)])
        rel = url_to_local_path(deep)
        self.assertLessEqual(len(rel), MAX_REL_LEN)

    def test_data_uri_is_capped(self):
        url = "https://example.com/mirror/_data_image_gif;base64," + "A" * 4000
        rel = url_to_local_path(url)
        self.assertLessEqual(len(rel), MAX_REL_LEN)

    def test_reserved_names(self):
        rel = url_to_local_path("https://example.com/con/prn/aux/nul.txt")
        low = rel.lower()
        for bad in ("con", "prn", "aux", "nul"):
            self.assertNotIn(os.sep + bad + os.sep, low)
            self.assertFalse(low.startswith(bad + os.sep))


if __name__ == "__main__":
    unittest.main()
