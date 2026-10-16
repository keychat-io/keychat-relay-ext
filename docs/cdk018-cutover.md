# CDK 0.18 receiver and data cutover

This branch is an implementation for local component verification. No deployment,
production database access, balance migration, real payment or production version
identification has been performed.

## Version and API contract

The implementation starts from fee_v3 `d7ad52d0e53323e0ebca4321f69186685c299eac`.
The old lockfile identifies CDK / cdk-sqlite 0.11.0, keychat-io/cdk kc2
`7288a436dd834fbee214daaf1cc841407b5c16c3`, and the old FFI plugin cdk3
`b0bf1c417288c0b759d306d5600c602c9f656d78`. The plugin also brought in CDK
0.3.0. These dependencies are absent from the new runtime and lockfile.

The receiver pins upstream crates.io cdk, cdk-common and cdk-sqlite **0.18.0**.
No CDK fork is used. The receive API now returns a net `Amount`, rather than the
old fork's transaction object. Transactions and sagas are read from the upstream
wallet database. rust-s3 uses crates.io 0.35.1; its boxed Bucket return is adapted
without changing the SAS HTTP contract.

`EventAdmit`, reply decisions and `Event.cashu = 8` remain unchanged. The nauthz
proto is byte-identical to the local nostr-rs-relay stamp-support baseline at
implementation time. Subscription notifications still use the complete event ID
and the first p tag's recipient values, and only follow a permit.

Gas configuration keeps fee.unit/mints, fee.prices[0].price and top-level kinds
for legacy sat, and adds relay_scope and stamp.enabled/mint/unit/cost_per_event/
kinds. The two payment rules are alternatives. Gas never uses stamps_per_sat; a
recharge ratio of 10 still pays one stamp when cost_per_event is one. Unknown
mint/unit/kind combinations are denied before any mint request. Missing unit is
legacy sat only. SAS only constructs allow-listed sat wallets and keeps its
existing size prices, info and object routes.

Existing fee.unit/mints, size-price tables, top-level kinds and RPC field numbers
retain their meanings. This is deliberately not an unchanged-config startup:
old inline words, old database paths, gas allow_free=true and a missing fixed
relay_scope are rejected. Supply the protected seed reference, a new database
and relay_scope before a controlled cutover. allow_pending never permits an
unconfirmed payment; a legacy mints_file cannot expand the allow-list.

The gas gRPC envelope keeps tonic's existing 4 MiB limit; the token's separate
byte/proof/amount limits still apply. Detached receive/status workers are bounded
to 64 outstanding tasks, including timed-out waiters, and excess requests fail
closed. The configured maximum token amount cannot exceed SQLite's signed
64-bit integer range. SAS does not log upload bodies, request headers or signed
upload URLs.

## New database and controlled seed

Use a fresh path ending in `.cdk018.sqlite`, separate for gas and SAS. Supply a
newly provisioned persistent BIP-39 mnemonic through the environment variable
named by seed_env. Provision it with the service's protected secret mechanism;
do not type it in command history, store it in the TOML template or generate a new
seed on each restart. Never fund a new service with the formerly published sample
mnemonic. Inline words configuration now causes a startup error.

The receiver refuses an existing unmarked database before opening CDK. A new
`.owner` marker records a hash binding database format, seed, service mode and
fixed relay scope. The marker and database are created with mode 0600. An OS
exclusive lock on the marker enforces one active receiver. Keep the marker with
the database on backup/restore; a marker with a missing database is rejected.
The SQLite WAL/SHM sidecars are secret wallet data too and are excluded from Git.
Do not remove it to bypass a startup error, change
seeds/scope in place, share a database between gas and SAS, or run an independent
wallet against it. Use a local filesystem with reliable advisory locking.

CDK wallet state and event associations live in the **same SQLite database**;
the latter use CDK's durable KV API under keychat-receipts-v1. A fixed scope hash
and the full event ID identify a receipt. Canonical proof Y identifiers provide
per-proof claims; token text, memo, padding, A/B encoding and proof order do not
create new payments. Each new business receipt also contains its sole original canonical token as
secret wallet recovery material, bound to scope/event/mint/unit/proof Ys/amount/
price and the complete token contents. It is not exposed by RPC, Debug or logs;
its token allocation is zeroized on drop. The SQLite database, WAL and backups
therefore require the same protection as CDK's own proofs and saga tokens. Legacy mints_file and
untrusted-mint transfer options do not add trusted mints or initiate transfers.

## Receive and recovery behavior

1. Validate the event ID and configured paid kind. Free kinds do not consume an
   attached token. Disabled gas denies paid kinds. allow_free is rejected by gas;
   allow_pending is accepted as a legacy setting but never grants a permit.
2. Bound token bytes, decoded size, proof count and integer amount. Parse standard
   A/B; the small legacy A decoder accepts ordinary Base64 with or without
   padding, and rejects mixed alphabets. Match a configured mint/unit rule before
   fetching keysets. Check every proof's keyset unit and net input fees.
3. Serialize receives under a process-owned gate. Persist the event intent and
   every proof claim before calling CDK receive with event/scope/proof metadata.
   A disconnected or timed-out RPC does not cancel the receive worker or release
   its gate. The waiter reports unresolved until the worker or later recovery
   proves receipt. This avoids canceling CDK/SQLite futures mid-operation.
   The worker owns its wallet Arc, OwnedSemaphorePermit and receive mutex until
   the operation ends. A timed-out caller only stops awaiting its JoinHandle;
   it does not release that ownership or clear claims. The 64-worker limit
   includes such workers, and excess work is rejected before spawning.
4. Only a completed CDK incoming transaction with sufficient net amount grants a
   permit. Pending, spent, malformed input, underpayment and network errors do
   not. A confirmed event returns its old permit and does not touch a new ticket.
5. Startup and repeated original-event requests validate the durable original
   token binding before reconciling exact input proof Ys with CDK transactions
   and sagas. A replacement ticket is never used to repair an existing receipt.
   Partial proof claims are completed only if every existing claim belongs to
   the same original event. A conflicting claim denies recovery.
6. An intent before CDK started can continue using that exact original token,
   including after restart. Continuation requires no related transaction of any
   status (including Failed, outgoing or partially overlapping inputs), no
   incomplete saga, no prior recovery output plan or transaction ID, and either
   no local input rows or a complete set of matching Pending inputs belonging to
   one orphan operation without any saga or transaction history. CDK 0.18 writes
   these inputs immediately before add_saga. It writes a SwapRequested saga and
   a Pending transaction BEFORE post_swap; successful receive and recovery never
   delete that transaction. This fixed upstream ordering plus the receiver's
   no-pruning rule provides the no-request evidence; absence of a saga or a
   remote UNSPENT response alone does not authorize a new operation.
7. A normal ProofsPending saga has not sent a swap; upstream only compensates it.
   After compensation and the full no-request checks, the same original token
   can continue. SwapRequested is handled as an existing operation, not replaced.
   Missing blinded outputs/counters cause persistent denial without invoking CDK
   recovery. A durable recovery_attempted guard survives repeated status calls
   and restarts; an unexplained Completed without output custody cannot permit.
8. Before CDK recovery, persist expected output Ys/amounts derived from recorded
   blinded outputs and counters. Afterwards require those exact proofs in local
   custody. CDK's SwapRequested recovery can mark Completed after spent inputs
   even when restore returns no proofs. Repeated status/restarts still deny an
   empty restore. This is a guard for a missing SwapRequested recovery plan or
   anomalous Completed, not a claim that normal ProofsPending compensation marks
   a transaction Completed.
9. Recovery preflights remote proof states without deleting local inputs. A thin
   upstream HTTP transport guard also checks every checkstate response inside
   CDK recovery, including a transition from UNSPENT to PENDING after preflight.
   Pending, mixed, missing or unavailable state preserves the saga and blocks new
   collection. The new-event path first reconciles known outstanding operations.
   An externally spent or invalid-signature ticket stays denied but does not
   permanently block another valid event after supported CDK reconciliation;
   no uncertain saga is manually cleared.

Old unconfirmed receipts without the original token deserialize compatibly but
remain denied. A replacement, even if it looks related, never fills in missing
recovery material. Already confirmed receipts remain idempotent. Malformed
bindings or unrelated local operation evidence are denied for operator review.
Do not delete claims, remove transactions or edit paid flags to bypass this.

If CDK removes a saga after empty restore, the receiver keeps its custody
requirement and denies that event until missing outputs are restored by
controlled wallet recovery. This branch still does not implement a general
operator restore command. There are no automatic refunds, balance merges,
outgoing payments or transaction pruning. Retain receipt history while event
replay must remain safe; monitor database growth.

A gas permit means payment was collected, not that relay message storage
succeeded. Failure between those services remains a possible paid but unconfirmed
publication. No cross-service atomic commit or refund is claimed.

## Preserve old wallets; do not migrate in place

Production's exact commit/image, schema, database path, mint allow-list, seed and
whether SAS runs are still unconfirmed. Determine them before planning cutover.

- For a fee_v3 CDK 0.11 database, retain a read-only copy, the matching above commit
  and lockfile, the seed in controlled storage, and a separately built matching
  legacy inspection/export tool. Old dependencies never enter this runtime.
- For a dev cashu-wallet SQLite database, retain its actual matching dev binary,
  dependency revisions and seed. It is not the same format as fee_v3.
- For early main relay.red/Redb, retain that actual binary and database tooling.
  A filename extension cannot establish compatibility.

On copies only, inspect SQLite sqlite_master/schema/user_version in read-only
mode and compare with the identified legacy version's migrations. Do not run CDK
0.18 against a legacy source. Inventory balances by mint and unit with the
matching version, and compare exports to original record counts. Inspection must
never print proof secrets or credentials into normal logs.

Before a later authorized cutover, an owner must choose to retain the old wallet
for controlled balance management or perform a one-time explicit token export
and receive into the new wallet. Stop the old receiver first, back up its files
and seed, and never let two wallets spend the same proofs. Validate any migration
using disposable copies and test funds first. This branch rewrites no balances or
historical rows. Restarting the old receiver is not a safe rollback once stamp
has been issued or the new receiver has collected new funds.

## Validation and outstanding integration

Run `cargo test -p keychat-relay-ext --locked` and
`cargo check --workspace --all-targets --locked`. Tests bind only loopback,
generate their own mint keys/proofs and use temporary SQLite databases. They
exercise upstream CDK receive and cryptographic output construction, not a mock
of the receiver's own success result.

Fault-injection tests decorate the real upstream WalletDatabase and stop at
nine durable boundaries: intent written, first proof claim, inputs written before
add_saga, saga written, before/after Pending transaction, received outputs written,
Completed transaction written, and before the business paid result. Every case
runs for sat and stamp, closes/reopens SQLite, supplies a replacement token,
checks that only the original proofs were collected once, then reopens again.
Additional tests exercise old rows without material, changed bindings/conflicting
claims, missing SwapRequested plans/anomalous Completed, repeated empty restore,
externally spent inputs in an early saga, bad-ticket isolation, persistent Pending,
UNSPENT-to-PENDING races and unreachable mint recovery.

The suite also covers sat/stamp, net fees, synthetic Base64 compatibility, A/B proof identity,
same-event competing tickets, shared proofs across events, lost reply, missing
metadata, restart, spent/pending/empty restore denial, full event ID, free kinds,
subscriptions and SAS prices. The minimal test mint is not evidence of a real
mint's API compatibility or Lightning settlement. A genuine old-wallet-produced
ordinary-Base64 fixture is still needed; the compatibility test here uses new
CDK protocol objects re-encoded with ordinary Base64, including non-URL-safe
characters. It is NOT a fixture produced by the old cashu-wallet library or an
old wallet database. The cached old implementation has been identified at
cashu-wallet-rs aad7f953ae3f23cde8d514c8e9c2a931ec47189d,
`cashu-wallet/src/wallet/token.rs`: TokenV3Generic::Display calls
STANDARD.encode(serde_json). That source was inspected but no independent old
library generator was built or executed; real old-library fixture coverage is
explicitly outstanding.

Separate local joint runs have already exercised an actual upstream CDK 0.18
stamp mint with bridge, gas and relay, including stamp issuance/receipt, relay
admission and non-1:1 conversion (k=1 and k=10). These are local integration
results using controlled test settlement, not production Lightning settlement.
The component suite separately exercises swap/checkstate/restore and recovery.
Acceptance evidence must match the source and binaries being tested: rebuild
and rerun the applicable joint cases after runtime changes.

Production acceptance still requires the deployed relay/gas version and fixed
scope to be identified, old sat mints' actual keysets/fees to be checked, real
backend small-payment settlement, a copied real legacy database inspection and
migration rehearsal, AWS/S3 upload, and production Nostr authentication and
subscription traffic. The real old-library token fixture remains outstanding as
specified above. Local joint success does not establish these production facts.
