#!/usr/bin/env python3
"""Draws Tapo Viewer's icon: the logo from the app (AppMark in ui/app.slint)
in its blue, as assets/app-icon.png (the window icon) and assets/app.ico
(the exe's icon, every size Windows asks for).

Needs Python 3 with Pillow:  python3 tools/make-icon.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

BLUE = (0x00, 0x5F, 0xB8)  # Fluent's accent blue, as in the app's light theme
LENS = (0x00, 0x47, 0x8A)  # the same blue, darker
WHITE = (255, 255, 255)
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
ASSETS = Path(__file__).resolve().parent.parent / "assets"


def draw(size: int) -> Image.Image:
    """The logo at `size` pixels, drawn four times larger and scaled down."""
    s = size * 4
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img, "RGBA")

    def circle(cx, cy, diameter, fill):
        r = diameter / 2
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=fill)

    margin = round(s * 0.03)
    side = s - 2 * margin
    d.rounded_rectangle((margin, margin, margin + side, margin + side), radius=side * 0.27, fill=BLUE)
    centre = s / 2
    # The ring, the lens and its highlight (proportions as in AppMark).
    circle(centre, centre, side * 0.52, WHITE)
    lens = side * 0.29
    circle(centre, centre, lens, LENS)
    spot = side * 0.083
    lens_left = centre - lens / 2
    circle(lens_left + lens * 0.21 + spot / 2, lens_left + lens * 0.21 + spot / 2, spot, WHITE + (191,))
    # The status light in the corner.
    circle(margin + side - side * 0.27 + spot / 2, margin + side * 0.17 + spot / 2, spot, WHITE + (178,))
    return img.resize((size, size), Image.LANCZOS)


def main():
    ASSETS.mkdir(exist_ok=True)
    images = {size: draw(size) for size in SIZES}
    images[256].save(ASSETS / "app-icon.png")
    images[256].save(ASSETS / "app.ico", sizes=[(n, n) for n in SIZES],
                     append_images=[images[n] for n in SIZES if n != 256])
    print(f"wrote {ASSETS / 'app-icon.png'} and {ASSETS / 'app.ico'}")


if __name__ == "__main__":
    main()
