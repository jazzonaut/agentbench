# Branding

The application icon, and the one script that produces every form of it.

| File | What it is |
| --- | --- |
| `source.png` | The artwork as supplied, untouched. 1254x1254, 24-bit RGB, **no alpha channel**. |
| `agentbench.ico` | Seven frames — 16, 20, 24, 32, 48, 64, 256. Linked into both executables by `build.rs` and served by the dashboard at `/favicon.ico`. |
| `agentbench.png` | 512x512 RGBA. Not an icon frame: the file to hand to anything that wants the artwork with its transparency already resolved. |
| `make-icon.py` | Regenerates the two above from `source.png`. |

## Regenerating

```
python branding/make-icon.py
```

Needs [Pillow](https://pypi.org/project/pillow/) and nothing else. Deterministic — running it twice
produces byte-identical files — so a rerun that changes anything means an input changed.

Of the files here only `agentbench.ico` is named in the crate's `include` list in `Cargo.toml`, because it
is the only one a build needs. `source.png`, `make-icon.py` and the master PNG are development inputs and
are not shipped to crates.io — the artwork alone is most of a megabyte. This file ships regardless, since
Cargo includes a README from any packaged directory.

## Why the script is not two lines

Two things about the supplied artwork force the work, and both are the kind of thing that looks like
over-engineering until you try the obvious version.

**The surround cannot be removed with a colour key.** `source.png` has no alpha: it is a cream rounded
tile drawn on a near-white square. The surround is `#FEFEFE` and the tile is `#FEFAF5` — nine levels apart
in blue, four in green — and the tile is not uniform, its blue channel ranging 242..247 across the face.
Any threshold loose enough to catch the surround eats most of the tile. Measured, by flooding from the
corner at each candidate threshold and counting what it reached:

| blue threshold | reached | share of canvas | verdict |
| --- | --- | --- | --- |
| 246 | 276,390 px | 17.58% | leaks — eats a ~10px ring of tile |
| **248** | **225,955 px** | **14.37%** | clean, and the loosest that is |
| 250 | 224,349 px | 14.27% | clean |
| 252 | 223,277 px | 14.20% | clean |

Nor can the corner be redrawn from a guess: fitting a circular rounded rectangle to the measured edge
gives a radius of about 240 from an arc sample and about 354 from the enclosed area, which is what a
superellipse looks like when you insist it is a circle. So the mask is a flood fill inwards from the
border, which measures whatever shape is actually there, and the white dot in the needle hub survives
because it is an interior hole the fill never reaches.

**The glyph has to be refitted or the small frames are unreadable.** The mark occupies 769 of the tile's
1182 pixels, so at 16px it lands in about ten and comes out a pale smudge. The script crops the glyph out
and recentres it on a freshly drawn tile with a tenth of the width as margin. Same mark, same colours,
same corner radius — recovered from the original's area rather than chosen — just framed to use the space
it has. Every frame from 16 to 256 is better for it.

## Why the frames are not all PNG

The `.ico` container allows either uncompressed bitmaps or embedded PNGs per frame. PNG frames are a
Vista-era addition and 256 is the size the convention was introduced for; below that, bitmaps are what
every mainstream icon toolchain emits and what nothing has ever failed to read. So 16..64 are bitmaps and
256 is a PNG. Going all-bitmap would add a quarter of a megabyte for the 256 frame alone.

Pillow picks one encoding for a whole file, so `make-icon.py` writes two throwaway `.ico`s and assembles
the real one from their frames. That is still far less error-prone than hand-rolling bitmap headers,
stride padding and mask conventions.
