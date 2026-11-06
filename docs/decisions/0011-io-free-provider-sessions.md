# 0011: Providers are I/O-free sessions; every inbound byte flows through recorded frames

**Status:** Proposed

**Context:** Milestone 3 needs two providers behind one trait (CQG protobuf over WSS; Coinbase JSON over WSS plus REST history). Decision 0007 requires data-source output to be recorded into replay files. Decision 0009 requires deterministic replays for screenshots.

**Decision:** A provider is a `Provider` (id, capabilities, instrument→vendor-symbol mapping, session factory) plus a per-connection `ProviderSession` that does no I/O. The session turns inbound frames into normalized `MarketEvent`s already in catalog-instrument units. It requests sends and HTTP GETs as `Outgoing` values. One generic driver performs all I/O. HTTP responses come back as `Http` frames on the same stream, so recordings capture them and replay feeds them to the same session code. During replay, the session's clock is the recorded frame time.

**Consequences:** Live, mock and replay run identical session code, and Coinbase REST history replays deterministically. Sessions are unit-testable without sockets. A provider whose protocol needs I/O the driver doesn't offer (e.g. a second socket) requires extending the driver, not the session. Recording format v2 stores provider, protocol and instrument; v1 files are rejected. Data with no wire protocol (e.g. Sierra Chart imports) uses the replay-only `lines` provider, whose frames are normalized events (`lines-events-1`, including order-book depth), one frame per atomic batch.
