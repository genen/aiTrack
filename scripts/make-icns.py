#!/usr/bin/env python3
"""Packs the PNGs in assets/icon into assets/icon/aitrack.icns without iconutil (works on Linux).

An .icns file is the magic 'icns', a big-endian total length, then chunks of (type, length, data);
modern chunk types hold a PNG as is. On a Mac, scripts/make-icns.sh does the same with iconutil.
Build helper only; the product itself is Rust.
"""
import struct
import sys
from pathlib import Path

# chunk type -> PNG size in pixels (ic11..ic14 are the @2x variants of 32/64/128/256 points)
CHUNKS = [
    ("ic07", 128),
    ("ic08", 256),
    ("ic09", 512),
    ("ic11", 64),
    ("ic12", 128),
    ("ic13", 256),
    ("ic14", 512),
]


def build(icon_dir: Path) -> bytes:
    body = b""
    for kind, px in CHUNKS:
        png = (icon_dir / f"aitrack-{px}.png").read_bytes()
        if not png.startswith(b"\x89PNG\r\n\x1a\n"):
            raise SystemExit(f"{px}px file is not a PNG")
        body += kind.encode("ascii") + struct.pack(">I", 8 + len(png)) + png
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    icon_dir = root / "assets" / "icon"
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else icon_dir / "aitrack.icns"
    out.write_bytes(build(icon_dir))
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
