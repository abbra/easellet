import importlib.util
import io
import json
import math
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import threading
import unittest
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


@unittest.skipUnless(shutil.which("ffmpeg") and importlib.util.find_spec("langdetect"), "ffmpeg and langdetect required")
class OmniVoiceTest(unittest.TestCase):
    def test_audio_conversion_and_server_failure(self):
        out = io.BytesIO()
        with wave.open(out, "wb") as audio:
            audio.setparams((1, 2, 24000, 0, "NONE", "not compressed"))
            audio.writeframes(b"".join(struct.pack("<h", int(12000 * math.sin(2 * math.pi * 440 * i / 24000))) for i in range(24000)))
        wav = out.getvalue()

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if request.get("language") != "fi" or request.get("response_format") != "wav":
                    self.send_error(400)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(wav)))
                self.end_headers()
                self.wfile.write(wav)

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            env = {**os.environ, "LANGS": "fi-FI", "EASEL_OMNIVOICE_URL": f"http://127.0.0.1:{server.server_port}/v1/audio/speech"}
            script = str(Path(__file__).with_name("tts-omnivoice.py"))
            result = subprocess.run([sys.executable, script], input="Hei.".encode(), capture_output=True, env=env, timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(result.stdout), 32000)
            samples = struct.unpack("<16000h", result.stdout)
            self.assertGreater(max(samples), 9000)
            self.assertLess(max(samples), 11000)
            env["LANGS"] = "en"
            failed = subprocess.run([sys.executable, script], input=b"Hello.", capture_output=True, env=env, timeout=20)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(failed.stdout, b"")
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
