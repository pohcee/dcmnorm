#!/usr/bin/env python3
"""Draws dcmnorm's icon and the images made from it (needs Pillow):

    python3 packaging/make-brand-images.py

writes, relative to the repository root:

    plugin/.claude-plugin/icon.png                        1024 px  the Claude plugin's listing icon
    docs/images/dcmnorm-icon.png                          1024 px  the same icon for everything else (README, PyPI)
    bindings/node/examples/test-website/public/favicon.png  128 px  the test website's favicon
    docs/images/social-preview.png                        1280x640 GitHub social preview (upload it by hand:
                                                                    Settings > General > Social preview)

The icon: a CT gantry ring around a stack of three image slices (isometric squares), the front
slice showing a pixel grid, on a deep blue rounded tile. Change the drawing here, not the PNGs.
The plugin keeps its own copy because the Claude directory installs plugin/ alone; links from
anywhere else use docs/images/dcmnorm-icon.png, so they don't depend on the plugin's layout.
The plugin icon is the directory's listing icon: the directory took it when the plugin was first
submitted, and redrawing it only changes the copy in the repository.
"""

import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
BG = (16, 42, 82, 255)
RING = (120, 196, 255, 255)


def draw_icon(size: int = 1024) -> Image.Image:
    """The icon at `size` px, drawn on a 1024 px grid at 4x and scaled down (smooth edges)."""
    S, k = 1024, 4
    img = Image.new("RGBA", (S * k, S * k), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    P = lambda pts: [(x * k, y * k) for x, y in pts]
    d.rounded_rectangle([0, 0, S * k, S * k], radius=220 * k, fill=BG)
    d.ellipse([150 * k, 150 * k, 874 * k, 874 * k], outline=RING, width=64 * k)
    cx, a, b = 512, 220, 106  # rhombus centre x, half-width, half-height

    def rhombus(cy):
        return [(cx, cy - b), (cx + a, cy), (cx, cy + b), (cx - a, cy)]

    slices = [(400, (64, 120, 190, 255)), (512, (110, 170, 235, 255)), (624, (236, 245, 255, 255))]
    for cy, color in slices:
        d.polygon(P(rhombus(cy)), fill=color, outline=BG, width=14 * k)
    # pixel grid on the front slice: lines parallel to its edges, endpoints on its edges
    top, right, bottom, left = rhombus(624)
    lerp = lambda p, q, t: (p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t)
    grid = (150, 190, 235, 255)
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    g = ImageDraw.Draw(layer)
    for t in (0.25, 0.5, 0.75):
        g.line(P([lerp(left, top, t), lerp(bottom, right, t)]), fill=grid, width=9 * k)
        g.line(P([lerp(top, right, t), lerp(left, bottom, t)]), fill=grid, width=9 * k)
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).polygon(P(rhombus(624)), fill=255)  # keep the grid inside the slice
    img.paste(layer, (0, 0), Image.composite(layer, Image.new("RGBA", img.size), mask).getchannel("A"))
    d.polygon(P(rhombus(624)), outline=BG, width=14 * k)
    return img.resize((size, size), Image.LANCZOS)


def font(bold: bool, size: int) -> ImageFont.FreeTypeFont:
    name = "DejaVuSans-Bold.ttf" if bold else "DejaVuSans.ttf"
    for base in ("/usr/share/fonts/truetype/dejavu", "/usr/share/fonts/dejavu", "/Library/Fonts"):
        path = Path(base) / name
        if path.is_file():
            return ImageFont.truetype(str(path), size)
    return ImageFont.truetype(name, size)  # let Pillow search its own paths


def draw_social_preview() -> Image.Image:
    """1280x640 (GitHub's recommended size): the icon left, name and tagline right, on the tile colour."""
    W, H = 1280, 640
    img = Image.new("RGB", (W, H), BG[:3])
    icon = draw_icon(400)
    img.paste(icon, (110, (H - 400) // 2), icon)
    d = ImageDraw.Draw(img)
    x, right_margin = 570, 80

    def line(y, text, bold, size, fill):
        # Shrink a line that would run into the right margin (GitHub crops some previews).
        while size > 12 and d.textlength(text, font=font(bold, size)) > W - right_margin - x:
            size -= 1
        d.text((x, y), text, font=font(bold, size), fill=fill)

    line(200, "dcmnorm", True, 112, (255, 255, 255))
    line(350, "Fast, open-source DICOM toolkit in Rust", False, 34, (205, 225, 250))
    line(404, "parse · render · transcode · reformat · network", False, 28, RING[:3])
    line(452, "CLI · Rust · Node.js · Python · Java", False, 28, (150, 190, 235))
    return img


def main() -> None:
    if len(sys.argv) > 1:
        sys.exit(__doc__)
    outputs = {
        ROOT / "plugin/.claude-plugin/icon.png": draw_icon(1024),
        ROOT / "docs/images/dcmnorm-icon.png": draw_icon(1024),
        ROOT / "bindings/node/examples/test-website/public/favicon.png": draw_icon(128),
        ROOT / "docs/images/social-preview.png": draw_social_preview(),
    }
    for path, image in outputs.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        image.save(path, optimize=True)
        print(f"{path.relative_to(ROOT)}  {image.size[0]}x{image.size[1]}")


if __name__ == "__main__":
    main()
