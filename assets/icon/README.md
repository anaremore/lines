# App icon

Concept "Ladder": a price ladder (DOM) with asks above, the inside market highlighted, and bids below.

| File | Use |
|---|---|
| `lines.svg` | Master, 32 px and up |
| `lines-24.svg`, `lines-16.svg` | Hand-tuned, pixel-aligned small sizes (do not scale the master down to these) |
| `png/lines-{16..1024}.png` | Rendered sizes (Linux, README, docs) |
| `lines.ico` | Windows: 16, 24, 32, 48, 64, 256 |

Palette: ink `#11141B`, signal violet `#8C7BFF`, paper `#ECEAF4`.

Regenerate (ImageMagick 7):

```sh
magick -background none lines.svg -resize 1024x1024 png/lines-1024.png
for s in 32 48 64 128 256 512; do magick png/lines-1024.png -filter Lanczos -resize ${s}x${s} png/lines-$s.png; done
magick -background none -density 384 lines-24.svg -resize 24x24 png/lines-24.png
magick -background none -density 384 lines-16.svg -resize 16x16 png/lines-16.png
magick png/lines-16.png png/lines-24.png png/lines-32.png png/lines-48.png png/lines-64.png png/lines-256.png lines.ico
```

macOS `.icns` is generated at packaging time with `iconutil` from the PNG set.
