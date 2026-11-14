#!/usr/bin/env python3
"""Renders resource/usage.png, the help overlay. Run after changing key bindings.

    python3 resource_dev/generate_usage.py

The viewer draws the image at half size, so everything here is in 2x pixels.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

SCALE = 2
WIDTH = 900 * SCALE
MARGIN = 30 * SCALE
KEY_COLUMN = 460 * SCALE
LINE = 21 * SCALE
GAP = 12 * SCALE
TEXT = (17, 24, 39)
RULE = (200, 200, 200)

REGULAR = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial.ttf", 17 * SCALE)
ITALIC = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial Italic.ttf", 17 * SCALE)

INTRO = [
    "Move the pointer to the bottom edge to reveal the controls.",
    "Click the 'i' there, or the Help menu, to toggle this screen.",
    "Click the sun/moon to change the UI shade.",
    "Click the bar at the bottom to jump to an image in the folder, or hold",
    "and drag to scrub through the folder.",
    "Scroll over the bottom bar or the thumbnails to move through the folder;",
    "scroll over the gamma slider to change gamma.",
]

# (description, keys); None starts a new group.
ROWS = [
    ("Quit Emulsion", "Esc"),
    None,
    ("Previous Image", "\u2190 |or| A |or| PgUp |or| Mouse Back"),
    ("Next Image", "\u2192 |or| D |or| PgDown |or| Mouse Forward"),
    ("Step Through the Folder", "Horizontal Scroll |or| Thumb Wheel"),
    None,
    ("Zoom In", "Scroll Up"),
    ("Zoom Out", "Scroll Down"),
    None,
    ("Show at Original Scale", "Q |or| 1"),
    ("Best Fit - Use Original Size if Fits", "E"),
    ("Fit to Borders", "F"),
    ("Fit to Borders, Then Original Scale", "Middle Click on the Image"),
    None,
    ("Darken Gamma", "[ |or| -"),
    ("Brighten Gamma", "] |or| +"),
    ("Reset Gamma", "\\ |or| Double or Middle Click the Slider"),
    None,
    ("Folder Thumbnails: Off, Auto-Hide, Pinned", "T"),
    None,
    ("Presentation Playback", "P"),
    ("Shuffled Presentation Playback", "Alt+P"),
    ("Animation Playback", "Alt+A |or| Alt+V"),
    None,
    ("Pan Camera", "Left Mouse Button |or| Space"),
    ("Full-Screen Mode", "Double Click |or| F11 |or| Enter"),
    None,
    ("Toggle Antialiasing", "S"),
    ("Automatic Antialiasing", "Alt+S"),
    None,
    ("Move Image to the Trash", "Del"),
    ("Copy Image to the Clipboard", "Cmd+C"),
]


def draw_keys(draw, x, y, text):
    """Draws a key description, rendering the `|or|` separators in italic."""
    for index, part in enumerate(text.split("|or|")):
        if index:
            draw.text((x, y), "or", font=ITALIC, fill=TEXT)
            x += draw.textlength("or", font=ITALIC) + 6 * SCALE
            part = part.strip()
        draw.text((x, y), part, font=REGULAR, fill=TEXT)
        x += draw.textlength(part, font=REGULAR) + 6 * SCALE


def main():
    height = MARGIN * 2 + len(INTRO) * LINE + GAP * 3
    height += sum(LINE if row else GAP for row in ROWS)
    image = Image.new("RGBA", (WIDTH, int(height)), (255, 255, 255, 255))
    draw = ImageDraw.Draw(image)

    y = MARGIN
    for line in INTRO:
        draw.text((MARGIN, y), line, font=REGULAR, fill=TEXT)
        y += LINE
    y += GAP
    draw.line([(MARGIN, y), (image.width - MARGIN, y)], fill=RULE, width=SCALE)
    y += GAP * 2

    for description, keys in (row or ("", "") for row in ROWS):
        if not description:
            y += GAP
            continue
        draw.text((MARGIN, y), description, font=REGULAR, fill=TEXT)
        if keys:
            draw_keys(draw, KEY_COLUMN, y, keys)
        y += LINE

    out = Path(__file__).resolve().parent.parent / "resource" / "usage.png"
    image.save(out)
    print(f"wrote {out} ({image.width}x{image.height})")


if __name__ == "__main__":
    main()
