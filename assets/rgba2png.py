"""Convert raw RGBA written by `cota.exe --render-panel` into a PNG.

The binary writes `<name>.rgba` plus a `<name>.rgba.size` sidecar holding
`width height`, for the same reason `--dump-icons` writes raw: a PNG encoder is
a dependency the shipped app has no other use for, and this only ever runs at
development time.

    cota.exe --render-panel target\\panel.rgba --theme dark --scale 3
    python assets/rgba2png.py target\\panel.rgba docs/img/panel.png
"""

import pathlib
import struct
import sys
import zlib


def convert(src, dest):
    src = pathlib.Path(src)
    width, height = map(int, pathlib.Path(f"{src}.size").read_text().split())
    rgba = src.read_bytes()

    expected = width * height * 4
    if len(rgba) != expected:
        raise SystemExit(f"{src}: expected {expected} bytes for {width}x{height}, got {len(rgba)}")

    # Filter type 0 (none) on every row: the image is small and zlib does the
    # work that per-row filters would.
    raw = b"".join(
        b"\x00" + rgba[y * width * 4 : (y + 1) * width * 4] for y in range(height)
    )

    def chunk(tag, data):
        body = tag + data
        return (
            struct.pack(">I", len(data))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    pathlib.Path(dest).write_bytes(png)
    print(f"{dest}: {width}x{height}, {len(png):,} bytes")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: rgba2png.py <input.rgba> <output.png>")
    convert(sys.argv[1], sys.argv[2])
