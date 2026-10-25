# Local brand assets

`trellis-logo-dark-v1.webp` is a web-sized derivative of the owner-supplied
`docs/design/workbench/trellis-logo-dark-v1.png` in the external design packet.
The 1254×1254 original remains in that packet, not in the shipped assets.

- Original SHA-256: `8575b5113db18c203eee8f81a77db6c554d0e6977e23fc123a966042b3992e41`.
- Crop box: left 310, top 215, right 945, bottom 1040 pixels; retains clear space around the artwork.
- Output: **600×780**, **22,528 bytes**, WebP quality 90 / method 6.
- Output SHA-256: `80a1a1004959d1e0ed7578e51452c566e695d08973869152e5767f9aa50afb7a`.
- Conversion: Pillow 12.0.0 in an external utility environment; no project dependency.

```python
from PIL import Image
image = Image.open("trellis-logo-dark-v1.png").convert("RGB")
image = image.crop((310, 215, 945, 1040))
image = image.resize((600, 780), Image.Resampling.LANCZOS)
image.save("trellis-logo-dark-v1.webp", quality=90, method=6)
```

`trellis-mark-transparent-v1.png` is the supplied provisional compact symbol,
unchanged. The full lockup is Connect-only; no favicon is shipped.
