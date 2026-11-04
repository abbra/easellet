import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import sys
import threading
import unittest


class LlmClientTest(unittest.TestCase):
    def test_optional_key_and_model_discovery(self):
        requests = []

        class Handler(BaseHTTPRequestHandler):
            def respond(self, body, status=200):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                requests.append((self.path, self.headers.get("Authorization")))
                self.respond({"data": [{"id": "test-model"}]})

            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append((self.path, self.headers.get("Authorization"), payload["model"]))
                if self.server.strict and "chat_template_kwargs" in payload:
                    self.respond({"error": {"code": "unknown_parameter", "param": "chat_template_kwargs"}}, 400)
                elif self.server.strict or payload.get("chat_template_kwargs", {}).get("enable_thinking") is False:
                    self.respond({"choices": [{"message": {"content": "Hello."}}]})
                else:
                    self.respond({"choices": [{"message": {"content": "", "reasoning_content": "Thinking"}}]})

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            for key, strict in (("secret-key", False), ("", False), ("secret-key", True)):
                server.strict = strict
                requests.clear()
                env = {**os.environ, "EASEL_LLM_URL": f"http://127.0.0.1:{server.server_port}/v1/chat/completions", "EASEL_LLM_API_KEY": key, "EASEL_LLM_MODEL": ""}
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("llm-local.py"))], input="Hello", text=True, capture_output=True, env=env, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), "Hello.")
                auth = f"Bearer {key}" if key else None
                post = ("/v1/chat/completions", auth, "test-model")
                self.assertEqual(requests, [("/v1/models", auth), post] + ([post] if strict else []))
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
