"""Tiny static server for install_test.sh.

Serves files below the given root. GET /releases/latest answers 302 to
/releases/tag/<tag> when the file <root>/LATEST_TAG exists and 404 otherwise.
"""
import functools
import http.server
import os
import sys


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/releases/latest":
            marker = os.path.join(self.directory, "LATEST_TAG")
            if not os.path.exists(marker):
                self.send_error(404)
                return
            with open(marker, encoding="utf-8") as handle:
                tag = handle.read().strip()
            self.send_response(302)
            self.send_header("Location", f"/releases/tag/{tag}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path.startswith("/releases/tag/"):
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        super().do_GET()

    def log_message(self, *args):
        pass


def main():
    root = sys.argv[1]
    port_file = sys.argv[2]
    handler = functools.partial(Handler, directory=root)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    with open(port_file, "w", encoding="utf-8") as handle:
        handle.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
