#!/usr/bin/env python3
"""Regenerate every icon artifact from the master bitmap (FRE-65).

Run from the repo root, after editing `assets/hubro.xcf` and re-exporting
`assets/hubro_transparent.png`:

    python3 scripts/make-icons.py

Produces:
  assets/icons/{32,64,128,256,512}x{...}.png  bundle icons (Dioxus.toml)
  assets/icons/icon.ico                       Windows exe/MSI icon
  assets/favicon.ico                          in-app webview favicon

Everything is a *downscale* of the master's trimmed artwork (413x552), which
is why the master must stay at least as large as the biggest output.

The two .ico files are assembled by hand because ImageMagick writes every
frame as uncompressed BMP, which costs ~270 KB for the 256x256 frame alone.
Windows has read PNG-compressed frames since Vista, but only the 256 frame
conventionally uses them, so we follow the usual hybrid: BMP up to 128, PNG
at 256.
"""

import struct
import subprocess
import sys
from pathlib import Path

MASTER = Path("assets/hubro_transparent.png")
FILL = 0.90  # fraction of the square canvas the artwork spans

BUNDLE_SIZES = [512, 256, 128, 64, 32]
EXE_ICO_SIZES = [16, 32, 48, 64, 128, 256]
FAVICON_SIZES = [16, 24, 32, 48, 64]


def run(*args):
    subprocess.run(args, check=True)


def render(trimmed, size, dest):
    """One square PNG: artwork scaled to FILL of the canvas, centered."""
    run("magick", str(trimmed), "-filter", "Lanczos",
        "-resize", f"x{int(size * FILL)}", "-background", "none",
        "-gravity", "center", "-extent", f"{size}x{size}", "-strip", str(dest))


def ico(frames, dest):
    """Assemble an .ico from (size, payload, is_png) frames.

    A frame's dimension byte is 0 when it is 256 -- the format stores the
    size in one byte, so 256 does not fit and 0 is the agreed sentinel.
    """
    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = len(header) + 16 * len(frames)
    entries, blobs = b"", b""
    for size, payload, _is_png in frames:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32,
                               len(payload), offset)
        blobs += payload
        offset += len(payload)
    dest.write_bytes(header + entries + blobs)


def bmp_frames_from_imagemagick(trimmed, sizes, tmp):
    """Extract ImageMagick's BMP-encoded frame payloads.

    We let ImageMagick build a throwaway .ico at these sizes, then lift each
    frame's bytes back out -- hand-rolling the DIB (doubled height, XOR plus
    AND mask) would be far more error-prone than reusing its encoder.
    """
    scratch = tmp / "bmp.ico"
    src = tmp / "bmpsrc.png"
    render(trimmed, max(sizes), src)
    run("magick", str(src), "-define",
        f"icon:auto-resize={','.join(str(s) for s in sorted(sizes))}",
        "-strip", str(scratch))
    raw = scratch.read_bytes()
    count = struct.unpack_from("<H", raw, 4)[0]
    out = {}
    for i in range(count):
        dim, _, _, _, _, _, length, off = struct.unpack_from("<BBBBHHII", raw, 6 + 16 * i)
        out[dim or 256] = raw[off:off + length]
    return out


def main():
    if not MASTER.is_file():
        sys.exit(f"missing master: {MASTER} (run from the repo root)")
    tmp = Path("target/icon-build")
    tmp.mkdir(parents=True, exist_ok=True)
    icons = Path("assets/icons")
    icons.mkdir(parents=True, exist_ok=True)

    trimmed = tmp / "trimmed.png"
    run("magick", str(MASTER), "-trim", "+repage", str(trimmed))

    for size in BUNDLE_SIZES:
        render(trimmed, size, icons / f"{size}x{size}.png")

    # Windows: BMP through 128, PNG at 256.
    bmp = bmp_frames_from_imagemagick(trimmed, [s for s in EXE_ICO_SIZES if s < 256], tmp)
    png256 = tmp / "f256.png"
    render(trimmed, 256, png256)
    frames = [(s, bmp[s], False) for s in EXE_ICO_SIZES if s < 256]
    frames.append((256, png256.read_bytes(), True))
    ico(frames, icons / "icon.ico")

    # Webview favicon: no 256 frame, so all-BMP is already compact.
    run("magick", str(icons / "64x64.png"), "-define",
        f"icon:auto-resize={','.join(str(s) for s in FAVICON_SIZES)}",
        "-strip", "assets/favicon.ico")

    print("icons regenerated")


if __name__ == "__main__":
    main()
