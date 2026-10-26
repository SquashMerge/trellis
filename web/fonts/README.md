# Local interface fonts

Trellis serves these fonts and their SIL Open Font License 1.1 texts locally.
No runtime request goes to a font CDN. Inter and Manrope use `-variable.woff2`
names and contain their complete upstream character sets. They have not been
subsetted or instantiated.

## Inter

- Official release: [rsms/inter v4.1](https://github.com/rsms/inter/releases/tag/v4.1),
  tag commit `e3a3d4c57d5ecc01453a575621882a384c1995a3`.
- Download: https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip
- Archive SHA-256: `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e`.
- `inter-variable.woff2` is the archive's `web/InterVariable.woff2`, copied unchanged.
  Normal variable font: `wght` 100–900; `opsz` 14–32.
- `Inter-OFL.txt` is the archive's `LICENSE.txt`, copied unchanged. It also matches
  [the license at the pinned commit](https://github.com/rsms/inter/blob/e3a3d4c57d5ecc01453a575621882a384c1995a3/LICENSE.txt).
  Copyright © 2016 The Inter Project Authors.

## Manrope

- Official source: [Google Fonts `ofl/manrope`](https://github.com/google/fonts/tree/b31870aff700ab7a1d74fa0c6887d95beb9e0037/ofl/manrope),
  pinned commit `b31870aff700ab7a1d74fa0c6887d95beb9e0037`.
- Input: [Manrope[wght].ttf](https://raw.githubusercontent.com/google/fonts/b31870aff700ab7a1d74fa0c6887d95beb9e0037/ofl/manrope/Manrope%5Bwght%5D.ttf),
  internal version **4.505**, normal `wght` 200–800, 742 glyphs.
- Input SHA-256: `3ae11c49db0455a3cc33e37d380f20fdb8c7f8b41dc07625c177e3d87a9d6ae6`.
- The pinned Google Fonts directory provides TTF, not WOFF2. `manrope-variable.woff2`
  is a WOFF2 encoding of that full TTF, produced with fonttools **4.60.1** and
  brotli **1.1.0** in a separate utility environment. No project dependency was added.
- `Manrope-OFL.txt` and `Manrope-FONTLOG.txt` are unchanged copies of
  [OFL.txt](https://raw.githubusercontent.com/google/fonts/b31870aff700ab7a1d74fa0c6887d95beb9e0037/ofl/manrope/OFL.txt) and
  [FONTLOG.txt](https://raw.githubusercontent.com/google/fonts/b31870aff700ab7a1d74fa0c6887d95beb9e0037/ofl/manrope/FONTLOG.txt)
  at the same pinned commit. The FONTLOG asks to be distributed with the font.
  License copyright: 2018 The Manrope Project Authors.

To reproduce, download the pinned TTF as `Manrope[wght].ttf`, save this as
`convert-manrope.py`, then run the command below. This changes only the encoding:
no subsetting, axis instancing or timestamp recalculation.

```python
from fontTools.ttLib import TTFont
import sys

font = TTFont(sys.argv[1], recalcTimestamp=False)
font.flavor = "woff2"
font.save(sys.argv[2], reorderTables=True)
```

```sh
uv run --no-project --with fonttools==4.60.1 --with brotli==1.1.0 python convert-manrope.py 'Manrope[wght].ttf' manrope-variable.woff2
```

## JetBrains Mono (unchanged)

`jetbrains-mono-latin.woff2` and `JetBrainsMono-OFL.txt` are retained byte-for-byte
from the existing operator-provided `Trellis UI.html` design reference. A precise
upstream version was not recorded with those assets; none is claimed here.
Copyright: The JetBrains Mono Project Authors. License: SIL OFL 1.1.

## Shipped file SHA-256

| File | SHA-256 |
| --- | --- |
| `Inter-OFL.txt` | `262481e844521b326f5ecd053e59b98c8b2da78c8ee1bdbb6e8174305e54935a` |
| `JetBrainsMono-OFL.txt` | `a76abf002c49097d146e86740a3105a5d00450b1592e820a1109a8c5680cd697` |
| `Manrope-FONTLOG.txt` | `b6b54bcc1dbd1b18d5afd087ec0b578097644b1c9ec3e0a642d4df7808f449c2` |
| `Manrope-OFL.txt` | `58172e0c0fac2cda8a37b348164bb55e44b0e69051e557e92b1d3f6910141f7b` |
| `inter-variable.woff2` | `693b77d4f32ee9b8bfc995589b5fad5e99adf2832738661f5402f9978429a8e3` |
| `jetbrains-mono-latin.woff2` | `2c32b9b3ee358c119e210f6f5195f9bd34894d78a785ff2e95d60e718e400af4` |
| `manrope-variable.woff2` | `30b83738add8c9edd9e3450b98036a9a8fb5668d0cbd4eb0ce5fe6761197f21f` |
