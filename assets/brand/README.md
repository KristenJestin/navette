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
| `mark-pixel.svg` | the second logo: the same mark drawn on a pixel grid, like the corner's tags, centred by construction (the cream chevron is the blue one turned half a turn) |
| `icon-pixel.svg` | the icon of the app: the pixel mark on a night blue rounded square. The archive of a version carries it as `navette.svg`, installed in the icon theme, and every notification names it |
| `favicon.svg` | the favicon, drawn for 16 to 64 pixels |
| `screen.svg` | the desk with a mode running, as in the social preview but without its words, on a transparent ground, for pages that put a title beside it |
| `social-preview.svg` | the social preview of the repository and the banner of the README: the pixel mark, the name, the tagline, and the home desk with a mode running, drawn (no capture) |

The colours: blue `#56baef` on a dark background, `#036eae` on a light one, cream `#ece5d8`, the
background `#14120f`, and night blue `#0d1318` for the banner and the pixel icon.

## What is generated

`png/`: the icon at 128, 256, 512 and 1024 pixels (`icon-*.png`), the full icon at 512, the
favicon at 16 and 32, the pixel icon at 266 and 532, `social-preview.png` (1280 by 640) and `screen.png` (1600 by 1000). `tools/brand.sh` needs
`rsvg-convert` (librsvg) and draws the text with the fonts of `fonts/` only, through a fontconfig
made for the run. The name in the banner is drawn dot by dot, in rectangles, with the 5 by 8 font
of the corner (`src/corner.rs`), as the app draws its tags.

`png/social-preview.png` is the social preview of the repository: it is uploaded by hand in the
repository's settings on GitHub (Settings, General, Social preview), as GitHub offers no other way.
