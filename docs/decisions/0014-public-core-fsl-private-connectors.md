# 0014: Public repo under FSL; live vendor connectors in a private module

**Status:** Accepted

**Context:** The repo is public on GitHub. Inspectable source builds trust with traders who route orders through Lines. But Lines may be sold, and vendor connections come with conditions: conformance certifies a specific app (Rithmic's `app_name`), and vendor protos and kits are proprietary. A public connector would let anyone build a fork that logs in as the certified app.

**Decision:**
- **License:** Functional Source License 1.1 with Apache 2.0 future license (`FSL-1.1-ALv2`, `LICENSE.md`). Anyone may read, use and modify Lines for any purpose except a competing commercial product; each release becomes Apache-2.0 two years after publication.
- **Open core:** the platform (core, feed framework, mock/replay/Coinbase providers, UI, OMS, paper broker) is public. Live vendor connectors for Rithmic and CQG, plus their protos, app names and kit material, live in a **private** crate or repo. Official builds include them through a Cargo feature; public builds compile and run without them.
- The provider and broker traits are the boundary. Nothing public may depend on a private connector's types.
- **Contributions:** before accepting outside pull requests, add a contributor licence agreement, so the licensor keeps the right to relicense and sell.
- **Repo hygiene:** GitHub secret scanning and push protection on. No credentials, account IDs, vendor usernames, or data derived from licensed feeds in commits, docs, screenshots or issues (extends decision 0010).

**Consequences:** Confirm with Rithmic (and CQG) that a public platform with a private connector is acceptable before building the connector. CI for public builds runs without vendor features. The license choice should get legal review before Lines is sold.
