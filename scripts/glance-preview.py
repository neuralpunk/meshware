#!/usr/bin/env python3
"""Turn the tray icons into a PNG contact sheet you can actually look at.

`apps/director/src-tauri/src/glance.rs` draws the mesh as a 44x44 bitmap.
Whether that bitmap *reads* — five dots where there should be six, one of
them amber — is not a question an assertion answers, so this renders every
shape from `glance::previews()` at 1x and at 4x on both a light and a dark
panel, which is the pair of backgrounds a system tray actually composites
against.

Usage:
    cargo run -q -p meshware-director --example glance-dump \
        | python3 scripts/glance-preview.py out.png
"""
import sys

from PIL import Image, ImageDraw

PAD = 10
ZOOM = 4
LABEL = 14


def read_icons(stream):
    icons = []
    lines = [line.rstrip("\n") for line in stream if line.strip()]
    i = 0
    while i < len(lines):
        parts = lines[i].split()
        if parts[0] != "ICON":
            i += 1
            continue
        name, w, h = parts[1], int(parts[2]), int(parts[3])
        raw = bytes.fromhex(lines[i + 1])
        icons.append((name, Image.frombytes("RGBA", (w, h), raw)))
        i += 2
    return icons


def sheet(icons, bg, ink):
    cell_w = max(icon.width for _, icon in icons) * ZOOM + PAD * 2
    cell_h = max(icon.height for _, icon in icons) * ZOOM + PAD * 2 + LABEL
    out = Image.new("RGBA", (cell_w * len(icons), cell_h), bg)
    draw = ImageDraw.Draw(out)
    for i, (name, icon) in enumerate(icons):
        big = icon.resize((icon.width * ZOOM, icon.height * ZOOM), Image.NEAREST)
        x = i * cell_w + PAD
        out.alpha_composite(big, (x, PAD))
        # …and the same icon at its real size, which is the only size that
        # matters, tucked in the corner.
        out.alpha_composite(icon, (x, PAD))
        draw.text((x, cell_h - LABEL + 2), name, fill=ink)
    return out


def main():
    dest = sys.argv[1] if len(sys.argv) > 1 else "glance.png"
    icons = read_icons(sys.stdin)
    if not icons:
        raise SystemExit("no icons on stdin")
    dark = sheet(icons, (24, 27, 34, 255), (190, 200, 215, 255))
    light = sheet(icons, (238, 240, 244, 255), (60, 70, 90, 255))
    out = Image.new("RGBA", (dark.width, dark.height + light.height))
    out.alpha_composite(dark, (0, 0))
    out.alpha_composite(light, (0, dark.height))
    out.save(dest)
    print(f"{len(icons)} icon(s) -> {dest}")


if __name__ == "__main__":
    main()
