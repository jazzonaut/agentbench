#!/usr/bin/env python3
"""Regenerate agentbench.ico and agentbench.png from source.png.

Run from anywhere; paths are resolved relative to this file.

    python branding/make-icon.py

Two things happen here that are not obvious, and both are forced by what the supplied artwork is.

**The surround has to be knocked out, and it cannot be done with a colour key.** source.png is 24-bit RGB
with no alpha channel: a cream rounded tile drawn on a near-white square. The surround is #FEFEFE and the
tile is #FEFAF5 — nine levels apart in blue, four in green — and the tile is not even uniform, its blue
channel ranging 242..247 across the face. Any threshold loose enough to catch the surround catches most of
the tile with it. Redrawing the corner analytically does not work either: fitting a circular rounded
rectangle to the measured edge gives a radius of about 240 from an arc sample and about 354 from the
enclosed area, which is the signature of a superellipse rather than a circular arc. So the mask comes from
a flood fill inwards from the border, which measures whatever shape is actually there.

**The glyph has to be refitted, or the small frames are unreadable.** The artwork carries a lot of tile
padding — the gauge and brackets occupy 769 of 1182 pixels across, so at 16px they land in about ten. The
result is a pale smudge rather than a recognisable mark. Everything below is therefore recomposed: the
glyph is cropped out and centred on a freshly drawn tile with a tenth of the width as margin. Same mark,
same colours, same corner radius — measured off the original rather than chosen — just framed to use the
space it has.
"""

import math
import struct
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "source.png"
MASTER = HERE / "agentbench.png"
ICON = HERE / "agentbench.ico"

# Blue-channel level at or above which a pixel counts as surround, for the flood fill only.
#
# Chosen by measurement, not taste. Flooding from (0, 0) at each candidate and counting what it reaches:
#
#     246 -> 276,390 px (17.58%)  leaks: eats a ~10px ring of tile
#     248 -> 225,955 px (14.37%)  clean
#     250 -> 224,349 px (14.27%)  clean
#     252 -> 223,277 px (14.20%)  clean
#
# 248 is the loosest value that is stable — 248, 250 and 252 differ only by the 2.7k pixels of the
# antialiased edge, while 246 falls off a cliff into the tile. Taking the loosest stable one keeps the
# knocked-out region as close to the true edge as the artwork allows.
KNOCKOUT = 248

# How far a pixel's channels must spread before it counts as part of the glyph rather than the tile.
#
# The tile spans #FEFAF5 to #FEFEFE, a spread of at most 9. The strokes are #2563EB blue, #10B981 green and
# an amber tick, all of which spread by well over 150. Anything in between is an antialiased stroke edge and
# belongs to the glyph too, so the threshold sits low — 30 is comfortably clear of the tile and catches the
# faintest edge pixel. Deliberately not keyed on the specific colours: a recoloured mark should still be
# found by this without anyone remembering to update a list.
SATURATION = 30

# Fraction of the tile's width left clear around the glyph on each side.
#
# The value the whole refit turns on. Too little and the mark touches the rounded corners; too much and the
# small frames go back to being a smudge. A tenth reads cleanly at 16px and still looks unhurried at 256.
MARGIN = 0.10

# Frames written as uncompressed device-independent bitmaps.
#
# 16, 20, 24 and 32 are the sizes Windows asks for in the notification area, the small-icon views and the
# taskbar at 100%, 125% and 150% scaling; they are real frames rather than downscales of 256 because that
# is the entire reason for shipping a multi-frame icon. 128 is omitted deliberately: nothing requests it
# and Windows interpolates it from 256 without a visible seam, so it would be tens of kilobytes of file
# for no pixel anybody sees.
BITMAP_SIZES = [16, 20, 24, 32, 48, 64]

# Frames written as embedded PNGs.
#
# Only 256, and the split is not a style choice. PNG-compressed frames inside an .ico are a Vista-era
# addition, and 256 is the size the convention was introduced for and the only one every consumer is
# guaranteed to decode; the smaller frames stay as bitmaps, which is what every mainstream icon toolchain
# emits and what nothing has ever failed to read. Going the other way — bitmaps all the way up — would add
# a quarter of a megabyte for the 256 frame alone, since a 256x256 32bpp DIB is 262,144 bytes of pixels
# before any header.
PNG_SIZES = [256]

# Side of the RGBA master, which is not an icon frame: it is the file to hand to anything that wants the
# artwork with its transparency already resolved — a README, a repository social preview, a future
# freedesktop icon — without re-running any of this.
MASTER_SIZE = 512


def knocked_out(source: Image.Image) -> Image.Image:
    """The artwork as RGBA, with everything outside the tile made transparent."""
    # Lookup tables rather than lambdas: `point` takes either, and a 256-entry table is both the faster
    # path through Pillow and the one whose types an editor can follow.
    binary = source.split()[2].point([255 if level >= KNOCKOUT else 0 for level in range(256)])
    # `thresh=0` because the image is already binary: the fill spreads through exactly-equal pixels and
    # stops at the first one below the threshold, which is the tile's edge. 128 is simply a third value,
    # distinguishable from both 0 and 255, marking what the fill reached.
    ImageDraw.floodfill(binary, (0, 0), 128, thresh=0)
    # Only the region connected to the border becomes transparent. The white dot at the centre of the
    # needle hub is also near-white, but it is an interior hole rather than part of the surround, so the
    # fill never reaches it and it stays opaque — which a colour key would have got wrong.
    alpha = binary.point([0 if marked == 128 else 255 for marked in range(256)])
    out = source.convert("RGBA")
    out.putalpha(alpha)
    return out


def corner_radius(art: Image.Image) -> float:
    """The tile's corner radius, as a fraction of its side, recovered from its area.

    An area fit rather than an edge trace. The corner is a superellipse, so no single radius reproduces it
    exactly and the question is which approximation to draw instead; matching the enclosed area is the fit
    that leaves the redrawn tile the same visual weight as the original, which is what the eye compares.
    Measures 0.2144 on the supplied artwork.
    """
    alpha = art.split()[3]
    box = alpha.getbbox()
    assert box is not None, "the knockout removed everything"
    width, height = box[2] - box[0], box[3] - box[1]
    opaque = sum(alpha.point([0] + [1] * 255).get_flattened_data())
    # area = width * height - 4 * radius^2 * (1 - pi/4), the four corners being what the rounding removes.
    radius = math.sqrt((width * height - opaque) / (4 * (1 - math.pi / 4)))
    return radius / ((width + height) / 2)


def glyph_box(art: Image.Image) -> tuple[int, int, int, int]:
    """Bounding box of the coloured mark, ignoring the tile it sits on."""
    red, green, blue, _ = art.split()
    lightest = ImageChops.lighter(ImageChops.lighter(red, green), blue)
    darkest = ImageChops.darker(ImageChops.darker(red, green), blue)
    saturated = ImageChops.difference(lightest, darkest).point(
        [255 if spread > SATURATION else 0 for spread in range(256)]
    )
    box = saturated.getbbox()
    assert box is not None, "no glyph found: every pixel is as grey as the tile"
    return box


def refitted(art: Image.Image) -> Image.Image:
    """The glyph, centred on a freshly drawn tile with [`MARGIN`] clear on each side."""
    radius = corner_radius(art)
    glyph = art.crop(glyph_box(art))
    side = round(max(glyph.size) / (1 - 2 * MARGIN))

    out = Image.new("RGBA", (side, side), tile_colour(art))
    out.alpha_composite(glyph, ((side - glyph.width) // 2, (side - glyph.height) // 2))
    # The corners are cut last, over the pasted glyph rather than under it. The crop is a rectangle of tile
    # with the mark on it, so pasting it can only ever square off a corner it reaches; masking afterwards
    # means the tile's shape is guaranteed by construction rather than by MARGIN being large enough.
    mask = Image.new("L", (side, side), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, side - 1, side - 1), radius=round(side * radius), fill=255
    )
    out.putalpha(mask)
    return out


def tile_colour(art: Image.Image) -> tuple[int, int, int, int]:
    """The tile's own colour, sampled where nothing is drawn on it."""
    box = art.split()[3].getbbox()
    assert box is not None, "the knockout removed everything"
    # Just inside the top edge, at the horizontal centre: above the gauge and below the rounded corners, so
    # the only thing there is tile.
    red, green, blue, _ = art.getpixel(((box[0] + box[2]) // 2, box[1] + (box[3] - box[1]) // 12))
    return (red, green, blue, 255)


def frames(art: Image.Image, sizes: list[int], bitmap: bool) -> list[tuple[bytes, bytes]]:
    """Encode `sizes` as ICO frames, returned as (directory entry, payload) pairs.

    Pillow writes a whole .ico or nothing, and it picks one encoding for every frame in the file. So the
    two encodings are produced as two throwaway files and the real one is assembled from their parts —
    which is cheaper and far less error-prone than hand-rolling the bitmap frames, headers, stride padding
    and mask conventions included.
    """
    options: dict[str, object] = {"sizes": [(size, size) for size in sizes]}
    if bitmap:
        options["bitmap_format"] = "bmp"
    buffer = BytesIO()
    art.save(buffer, format="ICO", **options)
    encoded = buffer.getvalue()

    count: int = struct.unpack_from("<H", encoded, 4)[0]
    out: list[tuple[bytes, bytes]] = []
    for index in range(count):
        entry = encoded[6 + index * 16 : 22 + index * 16]
        length, offset = struct.unpack_from("<II", entry, 8)
        out.append((entry, encoded[offset : offset + length]))
    return out


def main() -> None:
    art = refitted(knocked_out(Image.open(SOURCE).convert("RGB")))

    master = art.resize((MASTER_SIZE, MASTER_SIZE), Image.Resampling.LANCZOS)
    master.save(MASTER, format="PNG", optimize=True)

    # The mask is binary, so the edge is a hard staircase at full size. It does not need feathering by
    # hand: resampling to each frame size is what produces the antialiased edge, and because the knocked-out
    # pixels are near-white and the tile is cream, the pale fringe an unpremultiplied RGBA resize normally
    # leaves is a difference of four levels — invisible.
    entries = frames(art, BITMAP_SIZES, bitmap=True) + frames(art, PNG_SIZES, bitmap=False)

    # ICONDIR: reserved 0, type 1 for an icon, then the frame count.
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = len(header) + len(entries) * 16
    directory = bytearray()
    for entry, payload in entries:
        # Every field but the offset survives from the throwaway file; only the offset has to be restated,
        # because the payloads have been moved into a file with a different number of frames ahead of them.
        directory += entry[:12] + struct.pack("<I", offset)
        offset += len(payload)
    body = b"".join(payload for _, payload in entries)
    _ = ICON.write_bytes(header + bytes(directory) + body)

    print(f"{MASTER.name}: {MASTER_SIZE}x{MASTER_SIZE}, {MASTER.stat().st_size:,} bytes")
    listed = ", ".join(str(size) for size in BITMAP_SIZES + PNG_SIZES)
    print(f"{ICON.name}: {listed}, {ICON.stat().st_size:,} bytes")


if __name__ == "__main__":
    main()
