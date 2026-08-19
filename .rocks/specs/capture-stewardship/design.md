# Capture Stewardship Design

## ADRs

### ADR-0001: One replacement primitive, with the crash guarantee named at the call site

Plant replaced files through two helpers with different guarantees and no name
that said so: `fsutil::atomic_replace` renamed a temporary file without any
fsync, while `state::atomic_write` fsynced the file and its parent directory.
Which guarantee a write got depended on which helper the module happened to
import. The split ran the wrong way: the pending recovery marker was durable
while the capture journal it points at was not, so a power loss could leave a
surviving marker referring to a `state.json` that was absent or truncated.

Both helpers are now one function, `state::replace_file(path, bytes,
Durability)`, where `Durability::Fsync` syncs the file and the parent directory
before returning and `Durability::Rename` does neither. Every call site states
its choice. Fsync-durable writes are the capture journal (`state.json`, which
holds `capture_order`), the pending recovery marker, the recovery index, the
agent-run receipts, and the job attempt fence — everything a recovery pointer
refers to, so a pointer is never more durable than its target. Rename-only
writes are the staged per-turn envelope, the `.meta` drop counters, the sweep
inflight lease, and stored credentials.

The staged envelope stays rename-only deliberately. It is written on the
live-traffic path for every captured turn, and an fsync barrier there is a real
throughput cost paid on every request. A stage lost to a power cut costs one
unrecovered turn; the durable journal and marker still describe consistent
state without it. The rule is therefore narrow rather than blanket: fsync what
recovery depends on, and pay nothing for what recovery can do without.
