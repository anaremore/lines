# 0002: Prices and quantities are exact; f64 only at the pixel boundary

**Status:** Accepted

**Context:** Vendors encode prices differently (CQG: scaled integer × `correct_price_scale`; Rithmic: doubles). Possible future equities/crypto need fractional quantities and prices spanning roughly $0.00001 to $100k.

**Decision:** Prices are integers in per-instrument units with an exact scale. Quantities are likewise exact and not assumed to be whole contracts. Each provider converts its wire format once, at its edge. `f64` appears only in viewport/pixel math; values bound for the GPU are made relative to the viewport origin before casting to `f32`.

**Consequences:** The DOM ladder indexes by integer ticks. Provider adapters own validation of incoming scales. Core price types must not be defined in terms of any single vendor's units.
