# lines-fonts

The fonts Lines draws with (decision 0034): IBM Plex Sans (UI text) and IBM Plex Mono
(numbers), regular, medium and semibold. Embedded with `include_bytes!`; loaded by `lines-app`
before its first window opens.

## Source

Unmodified TTFs from IBM's GitHub releases (github.com/IBM/plex), `fonts/complete/ttf/`:

| Release | Asset | SHA-256 of the asset |
|---|---|---|
| `@ibm/plex-sans@1.1.0` | `ibm-plex-sans.zip` | `fb365d910566e6d199cc2c15579a7dd9a267128e18431a394ed81f1970c69200` |
| `@ibm/plex-mono@2.5.0` | `ibm-plex-mono.zip` | `6d23f01257663d8cc49a0d64c22ced630b79e0e2a0ac08a0da86e9a38bbc481c` (as GitHub lists it) |

| File | SHA-256 |
|---|---|
| `IBMPlexSans-Regular.ttf` | `975dcda37d80f038dcd143c22e33ca2d97a0cc5a929aace1c749153b0fe1afa5` |
| `IBMPlexSans-Medium.ttf` | `331c8639d7598b2cde62a911a71db195e30cb655cd6bdf2e324a7e984955f907` |
| `IBMPlexSans-SemiBold.ttf` | `a20caf8286023a6a7a85e40b1d2a4ae9fc3e3b1f9eda8f4c542dd4986af67bb1` |
| `IBMPlexMono-Regular.ttf` | `7c6fbddca4b700be918f5f6183d9bd4464fa427fe435f0b480d77fe2bb8c5a43` |
| `IBMPlexMono-Medium.ttf` | `98fbd727aae340b236955879dabed4d991aac9e8e90b3b2a67ce4a59221cc97c` |
| `IBMPlexMono-SemiBold.ttf` | `f04d7c488ddf7d1fa99f2574efc3406ea4cbe17bb1af3a1ab960f84d0c96a172` |

The npm packages of the same versions ship WOFF/WOFF2 only, which GPUI's text systems don't load.

## License

SIL Open Font License 1.1, `LICENSE.txt` (IBM's copy, verbatim). "Plex" is a Reserved Font
Name: never modify the files or rename the fonts. To update, replace the files from a newer
IBM release and update the hashes above.
