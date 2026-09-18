"""Turn `cota.exe --dump-icons <dir>` output into viewable contact sheets.

Kept out of the Rust binary on purpose: a PNG encoder is a dependency the
shipped app has no use for, and this only ever runs at development time.

    cota.exe --dump-icons target\\icons
    python assets/preview.py target\\icons
"""

import pathlib
import struct
import sys
import zlib

SCALE = 5
ORDER = ["2", "19", "50", "80", "96", "100", "paused", "unknown", "claude"]
# Representative Windows 11 taskbar colours -- the track is tuned against
# these, so judging it on white would be judging the wrong thing.
THEMES = (("dark", (32, 32, 32)), ("light", (243, 243, 243)))


def png(width, height, rgba):
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

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main(directory):
    root = pathlib.Path(directory)
    edge = int((root / "EDGE").read_text())

    for theme, bg in THEMES:
        cells = [(root / f"{name}-{theme}.rgba").read_bytes() for name in ORDER]
        width = edge * SCALE * len(cells) + 8 * (len(cells) + 1)
        height = edge * SCALE + 16

        out = bytearray()
        for _ in range(width * height):
            out += bytes(bg) + b"\xff"

        for i, src in enumerate(cells):
            ox, oy = 8 + i * (edge * SCALE + 8), 8
            for y in range(edge * SCALE):
                for x in range(edge * SCALE):
                    si = ((y // SCALE) * edge + (x // SCALE)) * 4
                    alpha = src[si + 3]
                    di = ((oy + y) * width + ox + x) * 4
                    for c in range(3):
                        out[di + c] = round(
                            src[si + c] * alpha / 255 + bg[c] * (255 - alpha) / 255
                        )

        path = root / f"sheet-{theme}.png"
        path.write_bytes(png(width, height, bytes(out)))
        print(f"wrote {path}")

    print("left to right:", " ".join(ORDER))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "target/icons")
