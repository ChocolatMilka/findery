"""Trim the empty bottom of a screenshot, keeping the page's own padding."""
import os
import sys

from PIL import Image

for path in sys.argv[1:]:
    im = Image.open(path).convert("RGB")
    bg = im.getpixel((2, im.height - 2))
    y = im.height - 1
    while y > 0 and all(im.getpixel((x, y)) == bg for x in range(0, im.width, 8)):
        y -= 1
    im.crop((0, 0, im.width, min(im.height, y + 56))).save(path.replace("-full", ""), optimize=True)
    os.remove(path)
