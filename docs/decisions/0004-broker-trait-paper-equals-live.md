# 0004: Paper trading is a broker behind the same trait as live

**Status:** Accepted

**Decision:** The UI (DOM, chart trading) emits order intents to the OMS. The OMS talks to a `Broker` trait implemented by `PaperBroker`, `ReplayBroker`, and live brokers. Paper acks are asynchronous with configurable latency. One order lifecycle everywhere: submitted → acked → working → partially filled → filled / cancelled / rejected.

**Consequences:** UI work done on paper transfers to live unchanged. Replay + paper gives deterministic, scriptable trading tests. Live routing is gated behind a Cargo feature until vendor conformance passes.
