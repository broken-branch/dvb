#!/usr/bin/env python3
"""A stand-in for a private GitHub release repository, for the release dry run and
the engine's tests: serves the release API and the asset API from a directory laid
out as <dir>/v<version>/<file>, and answers 404 (as GitHub does for a private
repository) unless the request carries `Authorization: Bearer <token>`; an asset is
served only to `Accept: application/octet-stream`.

    fake-github.py <dir> <token-file> [port]     prints the port it listens on
"""
import http.server, json, os, re, sys

root, token_file = sys.argv[1], sys.argv[2]
port = int(sys.argv[3]) if len(sys.argv) > 3 else 0
token = open(token_file).read().strip()
REPO = "/repos/broken-branch/dvb"


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, code, body=b"", ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def release(self, tag, base):
        d = os.path.join(root, tag)
        assets = []
        for i, name in enumerate(sorted(os.listdir(d))):
            if os.path.isfile(os.path.join(d, name)):
                # GitHub's field order: url comes before name.
                assets.append({"url": f"{base}/assets/{tag}/{name}", "id": i, "name": name})
        return {"url": f"{base}{REPO}/releases/1", "tag_name": tag, "draft": False, "assets": assets}

    def do_GET(self):
        if self.headers.get("Authorization") != f"Bearer {token}":
            return self.send(404, b'{"message": "Not Found"}')
        base = f"http://127.0.0.1:{self.server.server_port}"
        m = re.fullmatch(REPO + r"/releases/tags/(v[^/]+)", self.path)
        if m and os.path.isdir(os.path.join(root, m[1])):
            return self.send(200, json.dumps(self.release(m[1], base), indent=2).encode())
        if self.path == REPO + "/releases":
            tags = sorted(t for t in os.listdir(root) if t.startswith("v") and os.path.isdir(os.path.join(root, t)))
            return self.send(200, json.dumps([self.release(t, base) for t in tags], indent=2).encode())
        m = re.fullmatch(r"/assets/(v[^/]+)/([^/]+)", self.path)
        if m and os.path.isfile(os.path.join(root, m[1], m[2])):
            if self.headers.get("Accept") != "application/octet-stream":
                return self.send(415, b'{"message": "Unsupported"}')
            data = open(os.path.join(root, m[1], m[2]), "rb").read()
            return self.send(200, data, "application/octet-stream")
        self.send(404, b'{"message": "Not Found"}')


srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
print(srv.server_port, flush=True)
srv.serve_forever()
