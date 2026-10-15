# The logo of Navette

The crossed chevrons: one that leaves, in blue, and one that comes back, in cream, each with the
dot of the machine it starts from. The SVGs of this folder are the one source of the drawing;
the PNGs are rendered from them by `tools/brand.sh` and committed. To change the logo, edit the
SVGs and run it again. The colours and the transforms are the maintainer's, never redrawn by a
tool.

| Source | For |
| --- | --- |
| `mark.svg` | the mark alone, for a dark background (the default) |
| `mark-on-light.svg` | the mark alone, for a light background |
| `icon.svg` | the icon: the mark on a dark rounded square |
| `icon-full.svg` | the full icon, a blue square with dark strokes |
| `favicon.svg` | the favicon, drawn for 16 to 64 pixels |
| `social-preview.svg` | the social preview of the repository and the banner of the README: the mark, the name, the tagline, and a mode starting, drawn (no capture) |

The colours: blue `#56baef` on a dark background, `#036eae` on a light one, cream `#ece5d8`, the
background `#14120f`.

## What is generated

`png/`: the icon at 128, 256, 512 and 1024 pixels (`icon-*.png`), the full icon at 512, the
favicon at 16 and 32, and `social-preview.png` (1280 by 640). `tools/brand.sh` needs
`rsvg-convert` (librsvg) and draws the text with the fonts of `fonts/` only, through a fontconfig
made for the run. The name in the banner is drawn dot by dot, in rectangles, with the 5 by 8 font
of the corner (`src/corner.rs`), as the app draws its tags.

`png/social-preview.png` is the social preview of the repository: it is uploaded by hand in the
repository's settings on GitHub (Settings, General, Social preview), as GitHub offers no other way.
