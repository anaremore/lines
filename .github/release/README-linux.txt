Lines @VERSION@ (pre-alpha), Linux x64
https://github.com/anaremore/lines

A native futures charting platform. Pre-alpha: not ready for trading. Public builds
cannot route orders and carry only mock, replay and Coinbase public data.

Requirements
  A Vulkan driver, and X11 or Wayland with xkbcommon and fontconfig. On Debian/Ubuntu:
      sudo apt install libvulkan1 libxkbcommon-x11-0 libwayland-client0 libfontconfig1

Run
  ./lines                        CQG-protocol mock (synthetic ES data)
  ./lines --source coinbase      live BTC/USD from Coinbase's public feed (internet)
  ./lines --help                 all options

Licence: FSL-1.1-ALv2, see LICENSE.md. Third-party notices: THIRD_PARTY_LICENSES.txt.
