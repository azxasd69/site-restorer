"""Unit tests for the build stage: charset conversion, link rewriting, link check (no network)."""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src"))

from core import Engine  # noqa: E402


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = self.tmp.name
        self.mirror = os.path.join(self.out, "mirror")
        self.site = os.path.join(self.out, "site")
        os.makedirs(self.mirror)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, data):
        full = os.path.join(self.mirror, rel)
        os.makedirs(os.path.dirname(full) or self.mirror, exist_ok=True)
        with open(full, "wb") as f:
            f.write(data)

    def build(self):
        eng = Engine("example.com", self.out)
        eng.build()
        return eng

    def read(self, rel):
        with open(os.path.join(self.site, rel), encoding="utf-8") as f:
            return f.read()

    def test_cp1251_page_is_converted_to_utf8(self):
        page = ('<html><head><meta charset="windows-1251"></head>'
                '<body><h1>Привет</h1></body></html>')
        self.write("index.html", page.encode("cp1251"))
        self.build()
        text = self.read("index.html")
        self.assertIn("Привет", text)
        self.assertNotIn("windows-1251", text)
        self.assertIn("utf-8", text)

    def test_css_at_charset_is_rewritten(self):
        self.write("s.css", '@charset "cp1251";\nbody{color:red}'.encode("cp1251"))
        self.build()
        text = self.read("s.css")
        self.assertIn('@charset "utf-8"', text)
        self.assertIn("body{color:red}", text)

    def test_unquoted_attributes_are_rewritten(self):
        self.write("index.html", b"<html><body><img src=/img/a.png>"
                                 b"<a href=/page2/>x</a></body></html>")
        self.write("img/a.png", b"\x89PNG")
        self.write("page2/index.html", b"<html><body>p2</body></html>")
        self.build()
        text = self.read("index.html")
        self.assertIn('src="img/a.png"', text)
        self.assertIn('href="page2/index.html"', text)

    def test_single_quoted_links_are_rewritten(self):
        self.write("index.html", b"<html><body><a href='/page2/'>x</a></body></html>")
        self.write("page2/index.html", b"<html><body>p2</body></html>")
        self.build()
        self.assertIn("href='page2/index.html'", self.read("index.html"))

    def test_utf8_page_keeps_its_text(self):
        self.write("index.html", '<html><head><meta charset="utf-8"></head>'
                                 '<body>Привет</body></html>'.encode("utf-8"))
        self.build()
        self.assertIn("Привет", self.read("index.html"))

    def test_verify_counts_single_quoted_broken_links(self):
        self.write("index.html", b'<html><body><a href=\'/nope.html\'>x</a>'
                                 b'<a href="/nope2.html">y</a></body></html>')
        eng = self.build()
        stats = eng.verify()
        self.assertEqual(stats["pages"], 1)
        self.assertEqual(stats["broken"], 2)


if __name__ == "__main__":
    unittest.main()
