#!/usr/bin/env python3
# Regenerates plugin/.claude-plugin/icon.png (needs Pillow):
#   python3 packaging/make-plugin-icon.py plugin/.claude-plugin/icon.png
# dcmnorm icon: a CT gantry ring around a stack of three image slices (isometric squares),
# the front slice showing a pixel grid. Deep blue rounded tile.
import sys
from PIL import Image, ImageDraw
S = 1024; k = 4
img = Image.new("RGBA", (S * k, S * k), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
P = lambda pts: [(x * k, y * k) for x, y in pts]
BG = (16, 42, 82, 255)
d.rounded_rectangle([0, 0, S * k, S * k], radius=220 * k, fill=BG)
d.ellipse([150 * k, 150 * k, 874 * k, 874 * k], outline=(120, 196, 255, 255), width=64 * k)
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
img.resize((S, S), Image.LANCZOS).save(sys.argv[1], optimize=True)
