# Durable gas admission and batch collection

Gas's checked-in template enables local admission followed by background collection.
A permit means the original ticket has been durably reserved for this event and
relay scope; it does **not** mean funds have already been collected. This accepts
the risk of early admission preceding collection. It adds no remote anti-fraud,
refund, replacement-charge or relay-storage transaction guarantee. SAS retains
its synchronous confirmed-receipt behavior.

The gas-only `[batch]` settings are `enabled`, `size = 10`,
`max_wait_seconds = 30`, and `max_pending = 100` per canonical mint/unit.
Omitting the section preserves synchronous behavior. The queue counts pending
and in-flight unique events. Existing accepted event replays remain permitted at
capacity, consume no new ticket, and never reset the original queue age. Free
kinds do not consume tickets. All current allow-list, kind, unit, token bounds,
amount and per-input net-fee checks remain in force. Queue buckets never combine
different mints or units. Aggregate proof count, canonical token bytes and amount
stay within the existing input limits; unusually large tickets can produce a
smaller batch. Ten ordinary one-proof tickets produce one actual merged CDK
receive/swap, not ten receives. The oldest ticket becomes eligible at 30 seconds;
worker/network availability can delay completion beyond that eligibility time.

## Storage and ordering

The existing CDK SQLite KV store, protected owner marker, seed and fixed scope
remain the sole state owners. There is one process and one serialized background
wallet worker. No new service, database or per-message spawned retry task exists.
Original ticket material is stored only in protected wallet state and is never
included in service logs. Protect and back up the database, owner file and seed
together.

Admission has a short local registration lock, separate from the wallet network
gate. It uses cached keyset metadata and CDK's pure fee calculation. Unknown or
ambiguous keysets reject retryably and request refresh by the bounded worker,
outside the local lock. Startup performs local reconstruction before serving;
metadata warmup and legacy-operation network recovery run in the background.
Neither a cached new admission nor an accepted replay waits for a stalled swap,
wallet status request or recovery network operation.

The event receipt (including original token and accepted timestamp) is written
before canonical proof claims, and all claims must be durable before a permit.
A failed or cancelled write marks the local index for reconstruction. Startup or
the next registration scans receipts, completes partial claims without replacing
another event's owner, and rebuilds a small unfinished-work index. New admissions
use that index and direct receipt/claim lookups, not a scan of historical receipts.
Proof claims use canonical Ys, so alternate A/B encodings cannot spend another
event's reservation.

Before network collection, an immutable `gas_batch_<digest>` receipt records the
fixed member IDs, union of original proofs, mint, unit and checked total price.
Its namespace cannot collide with the 64-hex real event IDs. This manifest is
written before member assignments. Reconstruction validates the exact member
originals and union and completes partial assignments. A member cannot belong to
two manifests. Proof claims stay owned by original events; the batch validates
those fixed owners instead of replacing their claims. Successful batch receipt
state is persisted before marking member receipts paid. A crash during member
finalization resumes those updates without another collection.

## Recovery and failures

Synthetic batches reuse the current receipt/original-token binding, CDK saga,
exact input matching and recovered-output custody guards. An uncertain receive
recovers the original operation; it never swaps a replacement ticket or counts
SPENT alone as collection. Empty restore, mixed/PENDING state, missing recovery
plans and unreachable mints remain unresolved. One uncertain wallet operation
can delay further collection, while local admission remains available up to the
queue bound.

A failed batch may collect each original member separately only after the batch
is definitively resolved. The evidence is either an exact scoped CDK Failed
transaction with compensated saga/reservations/inputs, or a typed definitive CDK
local preparation error plus the existing complete no-request evidence (no
related transaction, saga, input rows or recovery outputs). There is no
error-string matching or manual uncertain-saga clearing. Only that recorded
failed parent is exempted from a child's overlap check. Invalid original members
remain accepted-but-failed with a durable failure record; good original members
can collect. A replay preserves the earlier permit even for a failed collection
and never consumes a new token.

Old confirmed per-event receipts retain their permits. Old unresolved
synchronous receipts retain their original recovery semantics and are recovered
separately, never converted into accepted queue admissions. Disabling batching
refuses startup if asynchronous collection/finalization is outstanding. After
all asynchronous work is resolved, accepted replays still retain their permits.
Rolling this database back to an older binary that does not understand batch
metadata is unsupported; it could collect member originals a second time.

For the configured Stamp wallet only, gas uses the public SDK rate configuration:
`stamp_requests_per_minute = 120`, `stamp_burst = 10`. This is a bounded explicit
budget for the operator's own mint and one serialized worker; the operator must
provision that mint accordingly. Legacy sat wallets and SAS retain CDK's default
budget. No CDK fork or global limiter disabling is used. Cached admission does
not perform info/key refresh. Structured redacted logs distinguish accepted
queue count, collected event count/net amount by mint/unit, pending work and
unresolved/definitive collection failures; original synchronous metrics retain
their existing meaning.

Tests use generated TestMint keys/proofs, actual CDK receive/recovery and temporary
SQLite only. They exercise real scheduler wakeup at ten events, persisted 29/30
second age via injected scheduler time, replay/canonical claim races, bucket and
fee isolation, aggregate/queue/in-flight limits, worker key refresh, stalled-swap
Handler permits/subscriptions, and interrupted admission/manifest/receive/member
finalization across restart. They do not establish production mint compatibility,
real-funds settlement or deployment readiness.
