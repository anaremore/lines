# 0006: Risk checks live in the OMS; prop-firm rules are data

**Status:** Accepted

**Decision:** Pre-trade risk (max quantity, max position, fat-finger price, daily loss) runs in the OMS before any broker, for paper and live alike. Prop-firm rules (trailing drawdown variants, daily loss, contract caps, profit target, consistency, news restrictions) are user-editable config presets, not code, and are labeled as approximations rather than official firm rules. The OMS supports account groups from day one. Paper vs live is visually unmistakable. Orders are never displayed as working before an ack. Flatten and Cancel All are always visible.

**Consequences:** Firm rule changes need no release. The trade copier (roadmap 14) builds on account groups. Server-side firm risk events (rejects, auto-liquidation) must be surfaced clearly.
