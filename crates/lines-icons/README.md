# lines-icons

The line icons Lines draws with: [Lucide](https://lucide.dev), embedded with `include_bytes!`
and handed to GPUI by `lines-app` (`icons.rs`: an `AssetSource` for `svg()`, and the bytes
directly for painted icons).

## Source

Lucide's own GitHub release, never another project's copy:

| Release | Asset | SHA-256 of the asset |
|---|---|---|
| `1.61.0` (2026-10-21) | `lucide-icons-1.61.0.zip` | `d4cfe5949e0cf202ec558c3fe2cc6c8373371c1cb8643fac2f7c68ce4153ff4b` |

The repository's `LICENSE` at tag `1.61.0`, split so cargo-about matches each part and ships
its copyright line in `THIRD_PARTY_LICENSES.txt`: `LICENSE-ISC.txt` (Lucide Icons and
Contributors), `LICENSE-MIT.txt` (Cole Bemis, Feather) and `NOTICE-feather.txt` (which Lucide
icons are derived from Feather, and so under MIT; here `log-in`, `maximize` and `x`).

## Changes

One edit to each file: `stroke-width="2"` became `stroke-width="1.6"` (a thinner line at 16 px,
the owner's pick). Shapes, caps (round) and joins (round) are Lucide's.

## Adding an icon

Copy `icons/<name>.svg` from the same release zip, make the same stroke edit, and add it to
`ICONS` in `src/lib.rs`.
