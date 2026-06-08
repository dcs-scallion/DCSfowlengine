# Fowl engine UI fonts

## Roboto Condensed (Latin + Cyrillic)

Universal label font for **bflib** Discord objective maps (PNG labels and future UI raster text).

| File | Subset | Source |
|------|--------|--------|
| `RobotoCondensed-latin-400.ttf` | Latin (+ Latin-1) | [Fontsource](https://fontsource.org/fonts/roboto-condensed) `latin-400-normal` |
| `RobotoCondensed-cyrillic-400.ttf` | Cyrillic | Fontsource `cyrillic-400-normal` |

Upstream family: [Roboto Condensed](https://fonts.google.com/specimen/Roboto+Condensed) (Christian Robertson, Google).  
License: **SIL Open Font License 1.1** — see [OFL.txt](https://openfontlicense.org/).

Runtime: both files are embedded in `bflib.dll` via `include_bytes!`. Glyph lookup uses the Cyrillic file for `U+0400–U+052F`, otherwise Latin.

### Regenerating subsets

Fontsource CDN (pin version when refreshing):

```text
https://cdn.jsdelivr.net/fontsource/fonts/roboto-condensed@5.2.8/latin-400-normal.ttf
https://cdn.jsdelivr.net/fontsource/fonts/roboto-condensed@5.2.8/cyrillic-400-normal.ttf
```

Optional merge into one TTF (requires [fonttools](https://github.com/fonttools/fonttools)):

```bash
pyftsubset RobotoCondensed-latin-400.ttf --output-file=RobotoCondensed-engine.ttf \
  --unicodes=U+0000-00FF,U+0100-024F
pyftsubset RobotoCondensed-cyrillic-400.ttf --output-file=RobotoCondensed-engine-cyr.ttf \
  --unicodes=U+0400-04FF,U+0500-052F
```

The engine ships the two-file subset pair (~47 KB total) for predictable Latin + Cyrillic coverage (e.g. `OPRRПром.6`).
