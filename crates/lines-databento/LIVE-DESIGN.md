# Databento live connector: design

Status: **built** (`src/live/`, feature `live`; the public `lines` app turns it on and registers
the source, decision 0033), tested on synthetic gateway traffic only. The protocol below was
checked against Databento's docs and databento-rs 0.64.0 on 2026-11-07.


**Changed 2026-11-09:** live now subscribes `mbp-1` by default, not `mbp-10`. Real-time `mbp-10` and `mbo` need Databento's CME Plus plan (support's answer; the gateway refuses them on Standard and closes the connection). `mbp-10` stays behind `LINES_DATABENTO_DEPTH=mbp-10`, and what this document says about it applies then. Wherever it names `mbp-10` as the live book below, read `mbp-1` by default.

## Build-time decision (2026-11-07): trades from the `trades` schema

The review (finding 4) said to take trades from `mbp-10`'s own trade records and subscribe no
`trades`. The build does the reverse: **trades come from the `trades` schema only, and
`mbp-10`'s trade records are ignored.** Why:
- The back-fill on connect is an intraday replay of trades. Replaying a day of `trades` is
  small; replaying a day of `mbp-10` (every depth change, top 10, both sides) would be heavy,
  slow to catch up, and not needed for the book.
- The book needs no replay: Databento's recovery guide recommends natural refresh for `mbp-10`
  (every record carries the whole top 10), so `mbp-10` subscribes without a start.
- Still nothing is counted twice: one source of trades. The reconnect de-duplication (finding 5)
  applies to `trades` exactly as designed.
Trades and book arrive on one socket in the gateway's order; their interleaving within a
millisecond is the gateway's, not the venue's (the importer's ordering note applies).

## Shape

The connector is a `SourceExtension` (decision 0020) named `databento`, registered as a
built-in source by the public `lines` app (decision 0033; off until the user's key exists):

| Part | Choice |
|---|---|
| `Provider` | id `databento`, protocol `databento-live-raw+dbn` (the gateway's line protocol plus DBN; the DBN version is read from each stream's metadata, not fixed in the id; see the review); supports the catalog's CME Group futures (`PRODUCTS`: equity index, rates, energy, metals, FX, grains). Capabilities: trades, BBO, depth (10 levels), history (intraday replay). |
| Session | I/O-free `ProviderSession` (decision 0011). It splits the byte stream into control lines and DBN records, and maps records with this crate's `price`, `defs` and `book` modules, the same code as the importer. |
| Transport | `DynTransport` over a plain TCP socket (`tokio::net::TcpStream`). Databento's live gateway is raw TCP with challenge-response auth, not TLS or WebSocket. It reads bytes in chunks and delivers each chunk as one `Frame::binary`. |
| Symbols | Lines resolves the contract (`roll::resolve_front`, decision 0013) and subscribes by raw symbol (`ESZ6`), so Lines' roll rule stays authoritative. `ES.v.0` (previous-day volume) may later be a cross-check: it is a live source of prior-session volume, a listed Known gap. |
| Availability | Unavailable with a reason when no API key is set ("Databento key not set"). The key comes from `key::ApiKey::load`. |

## Protocol (checked 2026-11-07 against the docs and databento-rs 0.64.0)

- Gateway `glbx-mdp3.lsg.databento.com:13000`: the dataset's lower-case name with dashes,
  port always 13000. Connect to the DNS name, never a cached IP (Databento's advice).
- The server greets with `lsg_version=...` and a challenge `cram=<text>` (one line each).
  Lines are newline-terminated, with fields separated by `|`.
- The client answers (official client's field order):
  `auth=<sha256 hex of "<cram>|<key>">-<bucket id>|dataset=GLBX.MDP3|encoding=dbn|compression=none|ts_out=0|client=<user agent>|heartbeat_interval_s=10|slow_reader_behavior=warn`.
  The bucket id is the key's last 5 characters; keys are exactly 32 characters (reject any
  other length before connecting). `compression` and `client` were missing from the first
  draft. `slow_reader_behavior` can't change after authentication.
- The server replies `success=1|session_id=...` or `success=0|error=...`.
- Subscriptions, each `stype_in=raw_symbol|symbols=ESZ6|snapshot=0|is_last=1` (`start` and
  `snapshot=1` are mutually exclusive; snapshot is MBO only; `is_last=1` marks the last line of
  a split subscription, DBN v3):
  - `schema=definition` with `start=0` (the weekly session; review finding 7);
  - `schema=trades` with `start=<ns>` (the back-fill; see "Reconnect and back-fill");
  - `schema=mbp-10` with no start (natural refresh).
  Trades come from `trades` only (the build-time decision at the top).
- Then `start_session` (no `=0`: the first draft was wrong). After that the socket carries
  DBN: metadata, then records, including `SymbolMappingMsg`, `SystemMsg` heartbeats and
  `ErrorMsg`.

## Credentials stay out of recordings

As with other connectors, credentials only exist in the transport:
- The session sends the auth line with a placeholder.
- The transport replaces the placeholder with the real response to the challenge just before
  writing to the socket.
- The public recorder records frames before `send`, so recordings contain the placeholder,
  never the key, its hash or its last 5 characters.
- An offline check scans a recording for the key.

## Recording and replay

The server-to-client byte chunks are recorded verbatim as `Frame::binary` (provider
`databento`, `.lfr` v3). Replaying one drives the same session code, so decoding is
byte-exact. Chunk boundaries don't matter, because the session buffers partial lines and
records.

## Never drop mbp-10

Set `slow_reader_behavior=warn`. The default, `skip`, drops mbp-1/mbp-10 records when Lines
reads too slowly, and says so only in a `SKIPPED_RECORDS_AFTER_SLOW_READING` system message.
With `warn` the gateway keeps every record and only warns.

If a skip or warning ever arrives anyway:
- **Book:** emit `BookClear`. The next event end (`F_LAST`) carries the whole top 10, so the
  book rebuilds itself exactly (the `book` module already does this after a clear).
- **Trades:** report a `Gap` (dropped trades) to the store and the local data store, never
  silence.

Keep the socket reader on its own task with a bounded buffer large enough for bursts. Decoding
must never block reading. Lines' feed already decodes outside the store lock.

## Reconnect and back-fill (intraday replay), as built

The first draft used Databento's documented exactly-once recovery (remember the last
`ts_event` and count, drop re-deliveries). The build's review found that wrong for Lines: the
replayed trades were added on top of bars the local store already held (double counts after a
restart), and a remembered position shared by feeds lost or dropped trades. As built, the
replay is **history**, the public stitch rule every provider follows:

- Every connection replays `trades` from the feed's `history_from` (the newest stored bar's
  start, a bar boundary), at most 23 h 40 min back (`REPLAY_WINDOW_MS`;
  `LINES_DATABENTO_REPLAY_HOURS` caps it lower), aligned up to a bar boundary when the window
  cuts it. The gateway's window starts at the next 10-minute mark after 24 hours ago (at 17:01
  it took 17:10 the day before at the earliest), so asking for the full 24 hours is refused.
- The replayed trades become bars and go out as one `History` snapshot whose closed `as_of` is
  the newest replayed millisecond; `lines_core::stitch` replaces the stored bars with it and
  drops re-delivered live trades. Nothing is remembered across connections, so feeds, restarts
  and replays can't disturb each other.
- If the switch from replay to live splits a millisecond, a live trade at or before `as_of` is
  added to the snapshot's last bar, re-sent as a corrected `History` (it replaces the bar).
- History older than the replay reaches is a `HistoryGap`. A replay that aged out
  (`REPLAY_DATA_AGED_OUT`), or a start refused as a subscription ("Invalid start time. Must be
  <time> or later, or 0"), is a gap up to the start the gateway names (now, if it names none);
  the next connection starts no earlier (a per-contract floor the provider keeps, only ever a
  lower bound, rounded up to the millisecond). A floor at or after now subscribes live only, no
  replay. A refused start never stops the feed.
- Replayed trades fill bars, not the tape or volume at price (as other providers' history).
- The book clears at each connection and rebuilds at the first `mbp-10` event end.
- Each connection starts with the transport's marker frame (recorded), so a recording that
  spans reconnects replays through one session.
- The replay plan travels in the recording: the transport echoes every line it sends (never
  the auth line) as a recorded inbound frame, and the session takes which subscriptions
  replay, and from when, from those echoes, never from its own settings. A recording decodes
  the same whatever the replaying machine's `LINES_DATABENTO_REPLAY_HOURS`.
- History completes when every replaying subscription reports completion (the dbn crate maps
  Databento's per-schema "Finished {schema} replay" messages to it); if the gateway reports
  fewer, once a completion has come and no trade arrived for 10 s. Known gap: in a market busy
  enough never to pause 10 s, that fallback would wait (a "recent trade ends the replay" rule
  was tried and dropped: right after a brief disconnect it would count re-delivered trades
  twice).
- Known gap: history gaps over closed-market time (a weekend before the replay window) are
  reported as gaps; they aren't trimmed with the sessions calendar yet.

Bars before the session come from the local store's bar index (decision 0019). Databento's
historical `ohlcv-1m` (HTTP, `FrameKind::Http`, URL plus body) is the back-fill for older
history.

## Mapping

Mapping is identical to the importer:
- prices are exact (1e-9 to catalog units; off-grid values counted as `Rejected`);
- the aggressor comes from the trade's side;
- each mbp-10 event end is one atomic `BookClear`/`BookLevel` batch;
- definitions are checked against the catalog (a tick mismatch is a fatal `BadMetadata`);
- instrument ids are mapped per session from `definition` and `SymbolMappingMsg`.

Stale detection uses the gateway heartbeat. The gateway default is 30 s (range 5 to 1800);
Lines sets `heartbeat_interval_s=10` explicitly, so `stale_after` is about 15 s.

## Tests

- Synthetic byte streams: the greeting, the auth exchange with the placeholder, DBN records
  split at every possible chunk boundary, and skipped-records messages.
- Replay determinism.
- The credential-leak check on recordings.
- No live test runs without a user's key and plan.

## Review against the latest docs (2026-11-07)

Checked against Databento's live docs (raw API reference, host and port, slow reader behavior,
recovering after a disconnection, intraday replay) and the official Rust client's `live`
module (databento-rs 0.64.0, Apache-2.0). The protocol section above is corrected in place.
Findings that change the design:

1. **Auth fields.** The auth line carries `compression` and `client` (missing before). Field
   order follows the official client. The bucket id (last 5 of a 32-character key) was right.
2. **`start_session`** is a bare line, not `start_session=0`.
3. *(As built, the gap is a `HistoryGap` from the replay's start to the start the gateway
   names, or to now if it names none; see "Reconnect and back-fill, as built".)*
   **Fatal aged-out replays.** With `warn`, a session that lags past the replay window gets a
   fatal `ErrorMsg` `REPLAY_DATA_AGED_OUT` naming the earliest available `start`. Handle it like
   a disconnect: report a `Gap` from the last applied `ts_event` to that `start`, reconnect,
   and replay from that `start`. `SLOW_READER_WARNING` (a `SystemMsg`) is counted in
   `FeedStats` and surfaced as a header notice, never ignored. `skip` (the default) applies only
   to MBP-1, MBP-10, BBO and CBBO schemas; others always behave as `warn`.
4. *(Superseded by the build-time decision at the top: trades from the `trades` schema.)*
   **One market schema: `mbp-10`, trades from its action `T` records.** MBP-10 carries the
   trades, so subscribing `trades` too would deliver each trade twice (the importer already
   takes trades from mbp-10 when no `trades` files are given). One stream also keeps trades
   and book in the gateway's own order. Fewer subscriptions, same data.
5. *(Superseded: as built, the replay is a closed history snapshot; nothing is
   de-duplicated by `ts_event` and count. See "Reconnect and back-fill, as built".)*
   **Recovery, per Databento's guide.** For mbp-10 the guide recommends natural refresh
   (stateless book), but Lines also needs every trade. Since trades come from mbp-10, a
   reconnect replays mbp-10 from the lowest remembered `ts_event` and de-duplicates exactly
   as designed (drop older records; drop the first N at the remembered `ts_event`). Book
   records are full top-10 states, so the book is exact again at the first replayed event end.
   Natural refresh is the fallback only after `REPLAY_DATA_AGED_OUT` or an outage beyond the
   window (`HistoryGap`).
6. **Replay window.** Intraday replay covers the last 24 hours, *less over weekends or other
   unpublished periods*. GLBX.MDP3 keeps the whole weekly session for `mbo` and `definition`.
   `start=0` asks for all that is available. Gap reports must use what the gateway says is
   available, not assume 24 hours.
7. **Definitions on connect.** A plain live `definition` subscription isn't guaranteed to send
   the current definition before the first trade. Subscribe `definition` with `start` at the
   current weekly session's open (kept for GLBX.MDP3). The tick/catalog check then runs before
   any market record is applied; market records before it are held, not dropped.
8. **Limits.** Standard plans allow 10 simultaneous connections per dataset per team; a gateway
   accepts at most 5 new connections per second per IP; subscriptions above 10 per second are
   delayed. Lines runs one feed per contract (`FeedRegistry`), so first build: one connection
   per contract, capped at 8 (availability reason "Databento connection limit"), reconnect
   backoff with jitter and at least 250 ms between connects. A shared session for all
   contracts is the later optimisation.
9. **DBN version.** The clients' default became DBN v3 in 2025, and v4 is in development (a
   new TBBO record variant). The session reads the version from the stream metadata. It
   decodes the versions it knows (v3) with the `dbn` crate's upgrade policy and refuses any
   other with a fatal, clearly worded error, never a guess. Recordings keep the metadata, so a
   replay knows its version. The protocol id is therefore `databento-live-raw+dbn` (no version).
10. **Unchanged and confirmed:** raw TCP with CRAM (the key never crosses the network), the
    gateway host rule, `slow_reader_behavior=warn`, and the credential placeholder kept out of
    recordings. (`ts_event` + count de-duplication was superseded at build time.)

Still to verify at build time (not stated in the docs read): the maximum symbols per
subscription line (the client chunks them; Lines subscribes one symbol, so it doesn't matter
yet), and whether `SymbolMappingMsg` precedes the first record of each instrument on a replay
with `start`.

Sources: databento.com/docs/api-reference-live (raw API, host and port),
databento.com/docs/api-live/basics/slow-reader-behavior,
databento.com/docs/api-live/basics/recovering-after-a-disconnection,
databento.com/docs/api-live/basics/intraday-replay, docs.rs/crate/databento 0.64.0
(`src/live/protocol.rs`, `src/live.rs`, `src/lib.rs`), databento.com/docs/release-notes.
