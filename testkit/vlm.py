#!/usr/bin/env python3
"""A deterministic stand-in for a vision grounding model.

The harness's vision path is: screenshot -> model -> normalized box -> click.
This process is the model end of that path, implemented as a pixel scan instead
of a neural network: it decodes the PNG, finds the region matching the colour
named in the request, and answers in the OpenAI-compatible envelope the
adapter speaks.

That is enough to test everything the harness owns — image transport, schema
validation, DPR/viewport checks, box-to-CSS conversion, action dispatch and
state verification. It says nothing about the quality of a real VLM, and the
docs say so.

    python3 testkit/vlm.py --port 8790
    BH_VLM_URL=http://127.0.0.1:8790/v1/chat/completions bhx task run ...
"""

from __future__ import annotations

import argparse
import base64
import json
import re
import struct
import sys
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COLORS = {
    "red": (229, 72, 77),
    "blue": (37, 99, 235),
    "green": (22, 163, 74),
    "orange": (245, 158, 11),
    "purple": (124, 58, 237),
}
COLOR_WORDS = re.compile(r"\b(red|blue|green|orange|purple)\b", re.IGNORECASE)


def decode_png(data: bytes) -> tuple[int, int, bytes, int]:
    """Minimal PNG decoder: 8-bit RGB/RGBA, no interlacing."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    pos = 8
    width = height = depth = ctype = interlace = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.extend(body)
        elif kind == b"IEND":
            break
    if depth != 8 or ctype not in (2, 6) or interlace != 0:
        raise ValueError(f"unsupported PNG (depth={depth} colour={ctype} interlace={interlace})")
    bpp = 4 if ctype == 6 else 3
    stride = width * bpp
    raw = zlib.decompress(bytes(idat))
    out = bytearray()
    prev = bytearray(stride)
    pos = 0
    for _ in range(height):
        ftype = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        if ftype == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        elif ftype != 0:
            raise ValueError(f"bad PNG filter {ftype}")
        out.extend(line)
        prev = line
    return width, height, bytes(out), bpp


def find_color(pixels: bytes, width: int, height: int, bpp: int,
               rgb: tuple[int, int, int], tol: int = 6) -> tuple[int, int, int, int] | None:
    r0, g0, b0 = rgb
    minx, miny, maxx, maxy = width, height, -1, -1
    for y in range(height):
        row = y * width * bpp
        for x in range(width):
            i = row + x * bpp
            if (abs(pixels[i] - r0) <= tol
                    and abs(pixels[i + 1] - g0) <= tol
                    and abs(pixels[i + 2] - b0) <= tol):
                if x < minx:
                    minx = x
                if x > maxx:
                    maxx = x
                if y < miny:
                    miny = y
                if y > maxy:
                    maxy = y
    if maxx < 0:
        return None
    return minx, miny, maxx - minx + 1, maxy - miny + 1


def locate(text: str, image_url: str) -> dict:
    m = COLOR_WORDS.search(text)
    if not m:
        return {"error": "no colour named in the request"}
    rgb = COLORS[m.group(1).lower()]
    header, b64 = image_url.split(",", 1)
    if "png" not in header:
        raise ValueError("expected a PNG data URI")
    width, height, pixels, bpp = decode_png(base64.b64decode(b64))
    box = find_color(pixels, width, height, bpp, rgb)
    if box is None:
        return {"error": f"no {m.group(1)} region in the screenshot"}
    x, y, w, h = box
    return {
        "bbox_norm": [x / width, y / height, w / width, h / height],
        "confidence": 0.94,
        "reason": f"largest {m.group(1)} region",
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "bhx-vlm-stub/1"

    def log_message(self, fmt: str, *args) -> None:
        if self.server.verbose:
            super().log_message(fmt, *args)

    def _json(self, code: int, value) -> None:
        body = json.dumps(value, ensure_ascii=False).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            return self._json(200, {"ok": True, "backend": "pixel-scan"})
        self._json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path != "/v1/chat/completions":
            return self._json(404, {"error": "not found"})
        length = int(self.headers.get("Content-Length") or 0)
        try:
            req = json.loads(self.rfile.read(length) or b"{}")
            user = req["messages"][1]["content"]
            text = " ".join(part.get("text", "") for part in user if part.get("type") == "text")
            image = next((p for p in user if p.get("type") == "image_url"), None)
            if image is None:
                return self._json(200, envelope({"error": "no image"}))
            result = locate(text, image["image_url"]["url"])
            print(f"vlm: {result}", file=sys.stderr, flush=True)
        except Exception as e:  # a test stub: report, do not crash the suite
            result = {"error": str(e)}
        return self._json(200, envelope(result))


def envelope(result: dict) -> dict:
    return {
        "id": "stub-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": json.dumps(result)},
            "finish_reason": "stop",
        }],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8790)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    httpd = ThreadingHTTPServer((args.host, args.port), Handler)
    httpd.verbose = args.verbose
    print(f"vlm stub on http://{args.host}:{args.port}/v1/chat/completions", flush=True)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
