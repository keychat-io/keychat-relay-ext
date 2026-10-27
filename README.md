# keychat-relay-ext

`keychat-relay-gas` authorizes paid Nostr events through the existing EventAdmit
gRPC API. It accepts configured legacy sat or stamp rules and permits a paid event
only after confirmed receipt using upstream CDK 0.18. The wallet and durable
per-event payment records use one new SQLite database owned by one process.

`keychat-relay-sas` retains the storage authorization HTTP API and sat size prices.
It does not accept stamp. Explicit `allow_free = true` also permits zero-priced
SAS size ranges; gas still rejects `allow_free`. Both services use strict mint allow-lists and protected
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

## SAS public upload and download URLs

SAS accepts two optional environment variables, loaded once at startup:

```sh
MEDIA_PUBLIC_UPLOAD_URL=https://keychat-media.s3-accelerate.amazonaws.com
MEDIA_PUBLIC_BASE_URL=https://cdn.keychat.io
```

- `MEDIA_PUBLIC_UPLOAD_URL` sets the base for the signed PUT `url`. The final
  public host and path participate in SigV4 signing; the service does not replace
  the hostname after signing.
- `MEDIA_PUBLIC_BASE_URL` sets the base for the unsigned GET `access_url`.

Each configured base gets `/<object-key>` appended, preserving an optional path
prefix and accepting a trailing slash. For the example above, an object named
`hash` returns `https://keychat-media.s3-accelerate.amazonaws.com/hash?X-Amz-...`
and `https://cdn.keychat.io/hash`. SAS does not append the bucket name again.
The variables are independent. An unset or empty variable preserves that field's
existing AWS endpoint URL, including its bucket path. Nonempty values must be
absolute HTTPS URLs without credentials, query parameters or fragments; invalid
configuration fails at startup.

Keep the existing AWS credentials, region and endpoint configuration. Both public
endpoints must serve the configured bucket and object keys. For the accelerate
example, configure `AWS_BUCKET=keychat-media` and enable S3 Transfer Acceleration
on that bucket; configure the CDN origin to serve those same keys. These variables
do not enable AWS acceleration, configure the CDN, move objects, or rewrite URLs
in existing messages. Clients still send the returned upload headers unchanged.

### Download acceleration only

To upload new objects through the ordinary S3 endpoint and return accelerated
download URLs for the same bucket, use:

```sh
AWS_BUCKET=keychat-media
AWS_REGION=ap-southeast-1
AWS_ENDPOINT_URL=https://s3.ap-southeast-1.amazonaws.com
MEDIA_PUBLIC_BASE_URL=https://keychat-media.s3-accelerate.amazonaws.com
```

Leave `MEDIA_PUBLIC_UPLOAD_URL` unset. Enable Transfer Acceleration and the
required upload/public-read permissions on `keychat-media` first. The returned
PUT URL uses the ordinary regional endpoint; `access_url` uses the acceleration
endpoint. Both address the same object in `keychat-media`. Existing messages keep
their saved URLs and require their original storage to remain available.

Verify locally without contacting AWS or reading production credentials:

```sh
cargo test -p keychat-relay-ext --bin keychat-relay-sas --locked
```
