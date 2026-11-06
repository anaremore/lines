# 0007: Indicators are pure; network I/O only through data-source plugins

**Status:** Accepted

**Decision:** Indicators consume series and emit values incrementally, with no I/O and a per-tick time budget, on threads separate from UI and feed. Third-party API access belongs to data-source plugins, which run async and publish timestamped series that indicators subscribe to. Data-source output is recorded into replay files. The indicator API distinguishes forming bars from final bars.

**Consequences:** Replay stays deterministic; slow APIs cannot stall charts. Exchange redistribution rules apply: outbound requests must not leak licensed market data.
