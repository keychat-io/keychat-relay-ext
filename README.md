# keychat-relay-ext

`keychat-relay-gas` authorizes paid Nostr events through the existing EventAdmit
gRPC API. It accepts configured legacy sat or stamp rules and permits a paid event
only after confirmed receipt using upstream CDK 0.18. The wallet and durable
per-event payment records use one new SQLite database owned by one process.

`keychat-relay-sas` retains the storage authorization HTTP API and sat size prices.
It does not accept stamp. Both services use strict mint allow-lists and protected
persistent seed configuration; the checked-in TOML files contain no secrets.

Build and verify with:

```sh
cargo check --workspace --all-targets --locked
cargo test -p keychat-relay-ext --locked
```

Tests use loopback-only temporary mints and generated test proofs. Read the
[receiver and data cutover guide](docs/cdk018-cutover.md) before provisioning a
service. Existing CDK 0.11, cashu-wallet SQLite or Redb databases must be preserved
with their matching tools and must never be opened by this new runtime.
