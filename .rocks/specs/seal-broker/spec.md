# Seal Broker Specification

## Requirements

### Requirement: Sole holder of seal-store credentials

The broker MUST be the only component holding credentials that reach the seal
store. Plant MUST NOT acquire an AWS dependency or a seal-store grant, and the
broker MUST NOT be reachable as a plant subcommand, because plant runs inside
every vended computer and therefore inside every agent sandbox. The broker's
grant MUST remain write-only — `s3:ListBucket` and `s3:PutObject` on the seal
prefix, and no `s3:GetObject` — until a read route genuinely exists.

#### Scenario: Existence is checked without a read grant

- WHEN the broker needs the size the store holds for a key
- THEN it issues a key-scoped `list-objects-v2`, which returns key and size
- AND it does not issue `head-object` or `get-object`, which `s3:GetObject`
  would be required to authorise

#### Scenario: A seal is stored in a single part

- WHEN the broker stores a seal
- THEN it issues `put-object` rather than a multipart transfer
- AND the IAM policy needs no `s3:AbortMultipartUpload` to clean up a failure

### Requirement: Tenancy from tailnet identity

Every seal route MUST resolve a tenant from the calling headscale node before
the route is chosen, and MUST refuse a caller whose address is outside the
tailnet ranges. The broker MUST NOT accept a bearer token, a shared secret, or
any other durable client credential. A loopback caller MAY be accepted as a
fixed tenant only when an explicit development setting names one, and the broker
MUST announce that setting at startup.

#### Scenario: A cluster-internal caller is refused

- WHEN a request arrives from an address outside the tailnet ranges
- THEN the broker responds 403 naming the address
- AND no tailscaled lookup and no store call is made

#### Scenario: A tailnet caller becomes a tenant

- WHEN a request arrives from a tailnet address
- THEN the broker asks the local tailscaled which node owns that address
- AND the node's short name becomes the tenant for the request

#### Scenario: Liveness and metrics need no tenant

- WHEN `/healthz` or `/metrics` is requested from any address
- THEN the broker responds without resolving a tenant
- AND the response discloses counts and ages only, never seal content

### Requirement: Idempotent size-checked upload

An upload MUST be compared against the store's own view of the key, and MUST be
skipped when the store already holds that key at that byte count. Key presence
alone MUST NOT be treated as sufficient, because a re-sealed session keeps its
key and changes its bytes. The broker MUST NOT impose any size ceiling below
S3's single-object limit.

#### Scenario: An unchanged seal is not rewritten

- WHEN a seal is offered whose key and byte count the store already holds
- THEN the broker reports `unchanged` and writes nothing

#### Scenario: A re-sealed session is stored again

- WHEN a seal is offered whose key the store holds at a different byte count
- THEN the broker stores it and reports `uploaded`

#### Scenario: An oversized seal is not skipped

- WHEN a seal larger than plant's commit cap is offered
- THEN it is compared and stored on the same terms as any other seal

### Requirement: Only a seal key is writable

The broker MUST accept only keys of the form
`sessions/YYYY/MM/DD/<session-id>/<seal file>`, and MUST reject anything else
before consulting the store. The set of accepted seal files MUST be a named
configurable value rather than a literal buried in a matcher.

#### Scenario: A key outside the seal layout is refused

- WHEN a PUT names a key that is not a seal path
- THEN the broker responds 400 and makes no store call

#### Scenario: Widening the seal set is configuration

- WHEN the accepted seal files are configured to include a second seal type
- THEN keys naming that file are accepted with no code change

### Requirement: Server-side staleness detection

The broker MUST export per-tenant ages for scraping, because a dead client
cannot report its own death and an absent client ledger line is
indistinguishable from a healthy quiet period. Liveness MUST advance on any
authenticated request, so a tenant with nothing new to upload reports healthy.
Durability progress MUST advance only on a stored seal, and MUST NOT be emitted
for a tenant that has stored nothing.

#### Scenario: A reconciling tenant with nothing to send reports healthy

- WHEN a tenant reconciles and every seal is already present
- THEN its contact age is exported and resets
- AND no upload age is exported for it

#### Scenario: A tenant that has never spoken has no series

- WHEN the broker has not been reached by a tenant since it started
- THEN no age series exists for that tenant, so `absent()` catches it rather
  than a zero reading claiming health

### Requirement: Client failure is loud

The seal-push client MUST treat an unreachable broker as a failed run and never
as a skip, MUST enumerate seals in full rather than incrementally, and MUST NOT
cap the work it attempts. A failure on one seal MUST NOT prevent the remaining
seals being offered, and MUST be carried in the run's exit status.

#### Scenario: The broker is down

- WHEN the client cannot reach the broker
- THEN the run fails and says so, leaving a failed ledger record

#### Scenario: One seal fails and the rest proceed

- WHEN one seal is rejected mid-run
- THEN the remaining seals are still offered
- AND the run's summary counts the failure and its exit status is non-zero

