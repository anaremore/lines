# 0008: User plugins run as WebAssembly components in Wasmtime

**Status:** Accepted (implementation at roadmap 10b)

**Decision:** The plugin ABI is a versioned WIT world. Guests cannot open sockets; HTTP goes through a host function that enforces a per-plugin manifest (allowed domains, secrets, rate limits). Secrets live in the OS keychain and are injected by the host; plugins never see them. Broker, OMS, and vendor credentials are not linked into the plugin host. Epoch interruption and memory caps enforce budgets.

**Consequences:** Plugins are portable across OSes and guest languages. The WIT world becomes a public contract once third parties depend on it.
