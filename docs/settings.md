# Settings reference

Every Settings row shows a brief description, always (at most 60 characters; owner, 2026-11-12). This page keeps the full text each row used to show, and the detail the brief one leaves out. Plugin settings describe themselves (Settings › Plugins shows each plugin's own text).

## General

- **Updates**: Automatic: download a newer release in the background and ask to restart (never during a trade). Notify only: say a newer release exists. Off: no request at all.
  Automatic never restarts on its own: it asks, and never while a position or order is open.
- **Time zone**: Times in charts, time & sales, the journal and strategies. Sessions, the VWAP reset and expiries stay on the exchange's clock.

## Appearance

- **Theme**: System follows the OS light or dark mode.
- **Dark palette**: Up and down colors in the dark theme. Colorblind uses blue and orange; direction also shows by shape.
- **Light palette**: Up and down colors in the light theme.
- **UI zoom**: The size of text, rows, headers, menus and panels in every window (Ctrl+= / Ctrl+− / Ctrl+0). The chart's own time and price zoom stay on the wheel and drag.
- **UI font**: Text in menus, headers, labels and dialogs, in every window. Prices, sizes, times and P&L stay in IBM Plex Mono. A font that is no longer installed shows IBM Plex Sans.

## Keys

- **Key bindings**: Every key and its command. Click a key to change it.

## Charts

- **Bar period**: The timeframe a new chart starts with.
- **Trade history on new charts**: When on, a new chart shows your closed trades on its contract (Alt+E on a chart).
- **Drawing tools on new charts**: When on, a new chart shows the drawing toolbar (each chart: its menu's Show).
- **Drawing tools position**: Where the drawing toolbar sits on every chart: a column at the left or a row on top.
- **Studies**: What a new chart shows: set with "Save as default for new charts" in a chart's studies menu (Ctrl+I).
- **Hollow candles**: Which direction's candle bodies are hollow (the other is filled), so direction never relies on color alone.

## DOM

- **Grouping**: Ticks per row in a new DOM (G cycles them in a DOM).
- **Trail in new DOMs**: A new DOM starts with the price trail column on.
- **Join buttons**: Join bid and Join ask in the DOM's trading panel.
- **Columns**: The columns a new DOM shows. The price column always shows, and the trail column shows while the trail is on. Per DOM: right-click a column header, or the palette's "Columns:" commands.

## Order entry

- **Default quantity**: The order size of a product you haven't set one for, in every open pane.
- **P&L display**: How positions and the trading panel show profit and loss.
- **Order tags on the chart**: Where working orders' tags sit: beside the price axis, or at the plot's left edge with the position's.
- **Bracket presets**: Stop and target pairs per account and product. The DOM panel's Edit… edits the same list in place.
- **Bracket units**: Show bracket distances in ticks or points (stored in ticks, exact).
- **Disable trading when a DOM loses focus**: Off by default (↺ shows the default): the trading switch stays as you set it.
- **Disable trading on an account change**: Off by default.
- **Disable trading on a contract change**: Off by default (a roll, a picker switch, a linked pane).

## Accounts

- **Account**: The account the page shows (Alt+A or its picker steps through them): paper accounts, plus each broker's (read-only while observed), and its rules.
- **Guardrail lockout**: While locked out you can tighten your guardrails; loosening waits until the session resets.
- **Daily loss limit**: Your own limit per trade date, in whole dollars (realized and open, fees included). Empty: off.
- **Max position**: Contracts per contract month (ES Dec and ES Mar are each capped), worst case: position plus open orders on one side. Empty: off.
- **Largest order**: The most contracts one new order (or a resize) may have on this paper account; the core refuses a larger one from every way in: DOM, chart, trade panel, keys, strategies. Exits that only close the position are never held to it. Empty: no limit. Paper accounts only.
- **Max trades per day**: Round trips per trade date. Empty: off.
- **Cooldown after losses**: Losing trades in a row that start a cooldown. Empty: off.
- **Cooldown length**: How long entries wait after the losing streak, in minutes.
- **No entries before the close**: No new entries this many minutes before the session closes. Empty: off.
- **News blackout**: Around high-importance releases that move the account's product (CPI, the jobs report, FOMC; official schedules bundled with Lines): Warn shows it; Block refuses new entries; Flatten also closes positions in affected products when the window opens. Exits are never blocked. While the calendar can't answer (out of date), Block and Flatten refuse entries.
  The windows come from the economic calendar bundled with Lines. While that calendar is out of date and can't say whether a release is near, Block and Flatten refuse new entries (Warn only warns): the rule fails closed. Exits (orders that only reduce a position) are never blocked.
- **News blackout: minutes before**: The blackout starts this many minutes before the release.
- **News blackout: minutes after**: The blackout ends this many minutes after the release.
- **News blackout: medium releases too**: The blackout also covers PPI, GDP and PCE (charts mark them either way).
- **Prop rules**: Approximations, not a firm's official rules. During a breach rules can be tightened, not loosened, until the session resets.
  This is the same tighten-only rule as a guardrail lockout.
- **Prop preset**: The preset's rules for this account; the rows below override single values. The account size is fixed.
- **Prop rule values**: The preset's values in force: daily loss, trailing drawdown, max micros, profit target. Override… changes single values.
- **Prop daily loss limit**: Whole dollars per trade date. Empty: the preset's.
- **Prop trailing drawdown**: Whole dollars (the preset's mode stays). Empty: the preset's.
- **Prop max micro contracts**: One standard contract counts as 10. Empty: the preset's.
- **Prop profit target**: Whole dollars; shown in the DOM's rule meters, doesn't restrict orders. Empty: the preset's.
- **Reset paper account**: Flattens and cancels everything on it, then back to its starting balance: today's P&L and prop progress cleared, guardrails and any lockout kept. The old file is kept as a dated .bak. Paper only.

## Connections

- **Data sources**: Each source connects only when a pane shows one of its symbols. A source's switch applies when Lines restarts; its settings, the next time it connects. Secrets are kept in the system's keychain.
- **Coinbase**: Public crypto prices (BTC/USD, ETH/USD). Connects only while a pane shows one; offline, those panes say so. Applies when Lines next starts.
- **Kraken**: Public crypto prices, spot and perpetuals. Connects only while a pane shows one; offline, those panes say so. Applies when Lines next starts.
- **Databento**: CME futures through your own Databento account, billed by Databento. Off until your API key is set up (DATABENTO_API_KEY, or the OS keychain entry lines-databento); without it Lines makes no Databento requests. Applies when Lines next starts.
  Lines reads the key from the `DATABENTO_API_KEY` environment variable, else from the OS keychain entry `lines-databento`. Settings never shows the key.
- **Databento key**: Whether this run found your Databento API key (DATABENTO_API_KEY, or the OS keychain entry lines-databento; a key added later counts from the next start). The key itself is never shown.

## Plugins

- **Installed plugins**: Settings each installed plugin describes, applied at once.

## Data folder

- **Data folder**: Where the store, recordings, paper accounts, journal, logs and backups live. A new folder is used after restart; moving copies and checks everything, then removes the old copy. Not while a position or order is open.
- **Local store**: Keeps live feeds' trades and best bid/ask per contract in the data folder, for the bars at startup and replays.
- **Store budget**: Beyond it, the oldest trade dates are removed (after lowering it, at the next start).
- **Depth recorded for**: Products whose order book is stored too (large).

## About

- **Version**: Updates are signed and checked before they run; this build updates itself only if it carries the project's update key.
- **Roll back**: Go back to the version the last update replaced, with the settings, workspaces and trading files it had. Waits while anything is trading.
- **Latency HUD**: Feed, paint, order and ack latency in chart headers.
- **Settings and logs folder**: Export diagnostics (palette) bundles the logs with secrets removed.
