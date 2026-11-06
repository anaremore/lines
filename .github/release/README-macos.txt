Lines @VERSION@ (pre-alpha), macOS Apple silicon (arm64)
https://github.com/anaremore/lines

A native futures charting platform. Pre-alpha: not ready for trading. Public builds
cannot route orders and carry only mock, replay and Coinbase public data.

Contents
  Lines.app    double-click to start with the CQG-protocol mock (synthetic ES data)
  lines        the same program as a command-line binary

Opening an unsigned app
  This build is not signed or notarized yet, so macOS blocks it on first launch.
  Either right-click Lines.app, choose Open, then Open again in the dialog;
  or clear the quarantine flag in Terminal:
      xattr -dr com.apple.quarantine Lines.app lines

Run from Terminal
  ./lines                        mock data
  ./lines --source coinbase      live BTC/USD from Coinbase's public feed (internet)
  ./lines --help                 all options

Licence: FSL-1.1-ALv2, see LICENSE.md. Third-party notices: THIRD_PARTY_LICENSES.txt.
