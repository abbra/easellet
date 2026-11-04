import runpy
from pathlib import Path
import unittest


language = runpy.run_path(str(Path(__file__).with_name("tts-chatterbox.py")))["language"]


class ChatterboxLanguageTest(unittest.TestCase):
    def test_language_detection_and_hints(self):
        self.assertEqual(language("fr", "ru-RU"), "fr")
        self.assertEqual(language("unknown", "fi-FI"), "fi")
        self.assertEqual(language("unknown", "en-US"), "en")
        self.assertEqual(language("unknown", "unsupported"), "ru")


if __name__ == "__main__":
    unittest.main()
