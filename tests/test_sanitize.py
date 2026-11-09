"""Unit tests for the address parser (no network, stdlib only)."""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src"))

from sanitize import sanitize_site  # noqa: E402


class SanitizeTests(unittest.TestCase):
    CASES = [
        ("prokupol.ru", "prokupol.ru"),
        ("https://www.prokupol.ru/catalog/?utm_source=x#top", "prokupol.ru"),
        ("  «prokupol.ru»  ", "prokupol.ru"),
        ("Сайт: prokupol.ru — магазин", "prokupol.ru"),
        ("https://web.archive.org/web/20261016223707/https://prokupol.ru/", "prokupol.ru"),
        ("https://www.google.com/url?q=https%3A%2F%2Fprokupol.ru%2F&sa=D", "prokupol.ru"),
        ("зкщлгзщдюкг", "prokupol.ru"),          # RU keyboard layout
        ("PROKUPOL.RU", "prokupol.ru"),
        ("http://prokupol.ru:8080/x", "prokupol.ru"),
        ("prokupol.ru.", "prokupol.ru"),
        ("https://prokupol.ru\nhttps://other.ru", "prokupol.ru"),
    ]

    def test_cases(self):
        for raw, expected in self.CASES:
            with self.subTest(raw=raw):
                host, _note = sanitize_site(raw)
                self.assertEqual(host, expected)

    def test_idn(self):
        host, note = sanitize_site("прокупол.рф")
        self.assertTrue(host.startswith("xn--"), host)
        self.assertEqual(note, "idn")

    def test_rejects_garbage(self):
        for raw in ("", "   ", "prokupol", "http://"):
            with self.subTest(raw=raw):
                host, _note = sanitize_site(raw)
                self.assertIsNone(host)

    def test_does_not_strip_path_on_purpose(self):
        host, _ = sanitize_site("https://example.com/a/b?x=1")
        self.assertEqual(host, "example.com")


if __name__ == "__main__":
    unittest.main()
