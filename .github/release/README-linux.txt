Lines @VERSION@ (pre-alpha), Linux x64
https://github.com/anaremore/lines

A native futures charting platform. Pre-alpha: paper trading only. Public builds
cannot route live orders; data comes from the mock, replays, and public or keyed
sources (Coinbase, Kraken, Databento).

Requirements
  A Vulkan driver, and X11 or Wayland with xkbcommon and fontconfig. On Debian/Ubuntu:
      sudo apt install libvulkan1 libxkbcommon0 libxkbcommon-x11-0 libxcb1 libwayland-client0 libfontconfig1
  xkbcommon and libxcb are linked into the binary, so they are needed even for
  --headless runs; Wayland's library is loaded only when a Wayland window opens.

Run
  ./lines                        CQG-protocol mock (synthetic ES data)
  ./lines --source coinbase      live BTC/USD from Coinbase's public feed (internet)
  ./lines --help                 all options

Licence: FSL-1.1-ALv2, see LICENSE.md. Third-party notices: THIRD_PARTY_LICENSES.txt.
