# delocal — v1 Design

> Draft 16 · 23 September 2026 · Status: **for review** · Changes from draft 15: the `stamp`, a Lamport timestamp seeded by mtime, is the first key of the winner rule, because the simulator disproved draft 15's claim that one total order was enough (§7.1, §7.6, §7.8); delete-vs-modify restated under the stamp (§7.6); I7 added (§14.1).
>
> Changes from draft 15: the scan fast path compares the exec bit (§7.3); one total order for every concurrent pair, identical content included, with the argument for why the merged version is then a function of its vector (§7.2, §7.6).
>
> Changes from draft 14: content is requested by hash, not by version, and want-list sources are keyed by content (§7.5, §12); batches carry `seq_low` and acknowledgements are the contiguous watermark, so the protocol is correct over a lossy, reordering transport (§7.4, §12); `IndexRemoved` hook (§11); CI policy for the random sweep (§14.1).
>
> Changes from draft 13: given-up wants retry on a new source or connection (§7.5); the folder state is the unit of persistence and restart, index writes are durable and `seq` never rewinds (§11, §13); simulator crash model, I2 scoped to announced content, I4 defined by losing versions (§14.1).
>
> Changes from draft 11: catch-up carries announced records only and `have_up_to` replaces the ack (§7.4); want-list sources, selection, concurrency limits, progress-based deadlines, and the in-flight rule with its `revert` exception (§7.5, §8.3).
>
> Changes from draft 10: the sender's H1 denominator is the tracked count at the last announcement; a paused folder stays paused until `approve` or `revert`; frozen paths keep every incoming version; rule changes never release anything by themselves; the paused batch has a reserved id (§8.1). Quarantine stores raw incoming entries and `approve` re-classifies (§8.2). `revert` restores records with their original `seq`, removes never-announced adds, and puts reverted paths in flight (§8.3).
>
> Changes from draft 9: rule 5 of the winner rule orders on kind as well (§7.6); the conflict copy's `prev_hash` follows §7.1 rather than being fixed at `EMPTY` (§7.6); the conflict-name split and truncation rules are spelled out (§7.6).
>
> Changes from draft 8: the winner rule is a total order in five steps (§7.6); the conflict copy is this machine's change (§7.6); an occupied conflict path displaces to trash (§7.6); a displacement target that appears late is `ChangedUnderneath` (§7.5); non-empty directory losers noted as known behaviour (§7.6).
>
> Changes from draft 7: directories and symlinks carry no mtime (§7.1, §7.5); a commit that finds the file changed underneath is re-evaluated after the next observation rather than dropped (§7.5); H1 excludes directories on both sides of the ratio, and the sender cannot see exec-only changes (§8.1).
>
> Changes from draft 6: batch entries ordered by `seq`, `seq_high` on every batch, the decision doubles as the acknowledgement, and the connect-time watermark exchange (§7.4, §12); the sender's summary counts local changes only (§7.4).
>
> Changes from draft 5: conflicts resolve to the merged version `M = merge(W, L)` with no increment, replacing `W′` (§7.6); the displacing commit (§7.5); adopted records are announced, so live batches and catch-up are the same mechanism (§7.4).
>
> Changes from draft 4: `prev_hash` on every entry and the metadata-only rule in conflicts (§7.1, §7.6); mtime-only changes and the mtime-precision shim (§7.3); tombstone `hash` and `mtime_ns` defined (§7.1); NFC normalisation is the host's job (§7.1, Appendix A); brake classification of mods via `prev_hash` (§8.1).
>
> Changes from draft 3: `seq` assignment rule (§7.1); merged-record fields on identical-content concurrency (§7.2); metadata-only applies (§7.5) and their brake treatment (§8.1); paused folders keep receiving and pending batches grow (§8.1); `deny` vector construction (§8.2); rename detection listed as out of scope (§3.2); simulator moved to its own crate (§14.1, Appendix B).
>
> Changes from draft 2: NodeId representation and ordering defined (§4); `author_host` carried on entries and used for conflict names (§7.1, §7.4, §7.6, §11); engine takes time and randomness as inputs (§7); serde allowed in the engine (Appendix B); simulator gets its own CI job (§14.1).
>
> This file is the source of truth for v1. Claude Code builds from it. When behaviour changes, this document changes first, then the code. Anything marked **[decision]** is a judgement call made while drafting that has not been discussed yet and should be confirmed or overruled. Anything marked **[verify]** is a claim about a third-party system that must be checked against reality during the relevant phase.

---

## 0. One paragraph

`delocal` keeps a folder identical across all of your machines. It runs over your tailnet, so it has no accounts, no device IDs, no ports to open and no config file. You install it with one command, run `delocal up`, and your machines find each other. It is a single static Rust binary with a background service and a small, modern CLI in the spirit of `tailscale`, `atuin` and `herdr`. It is built to never lose a file: every remote change that overwrites or deletes something goes through a local trash, and any change large enough to be a mistake is held for approval before it spreads.

The name: in physics a *delocalized* particle is not confined to one position but spread across many at once. In plain English it also reads as "not local". Both are the point.

---

## 1. Principles

These decide arguments. When two options are otherwise equal, the earlier principle wins.

1. **Never lose a file.** Sync spreads mistakes as fast as it spreads work. Trash before overwrite, hold before mass change, tombstones so nothing comes back from the dead. Correctness bugs are release blockers; missing features are not.
2. **Nothing to configure.** Sensible defaults for everything. Config exists (`delocal rules`) but a user should be able to run delocal for a year without touching it.
3. **Tailscale does networking. delocal does sync.** Discovery, NAT traversal, encryption, authentication and identity all come from Tailscale. delocal never reimplements any of them. The Tailscale integration lives in one module so it can be swapped later.
4. **Full mesh, no primary.** Every machine talks to every machine it can reach. Any machine can be off. There is no server, hub or "source of truth" machine in v1.
5. **Quiet.** Nothing prints unless asked. `delocal status` tells you everything. The user should not have to know delocal exists until they want to.
6. **Boring over clever.** Whole-file transfers, counting-based safety rules, SQLite. Cleverness (deltas, content analysis) comes later and only where it pays.

---

## 2. The user story

Dyllan has a desktop at home, a laptop, a Raspberry Pi and a NAS, all on one tailnet. He is in a coffee shop on the laptop and drops a few files into `~/Sync` for a tool on the desktop to pick up. They are on the desktop before he has closed the lid. Later he edits a 2 GB dataset on the laptop; delocal notes it and waits until the laptop is back on the home LAN before sending it, because 2 GB over coffee-shop wifi through a relay is not worth it. At home it goes across in a minute. At no point does he run a command, open a UI or think about which machine has what.

One day a script on the Pi goes wrong and deletes most of `~/Sync`. The Pi's own delocal notices that 80% of the folder just vanished, pauses, and sends nothing. `delocal status` on any machine says so. He runs `delocal revert` on the Pi and the files come back from the desktop. Nothing was lost anywhere.

---

## 3. Scope

### 3.1 In v1

- N machines, full mesh, two-way sync, over Tailscale
- Linux (x86_64, aarch64) and macOS (arm64, x86_64)
- Files, directories, symlinks (as symlinks), executable bit
- Version vectors, tombstones, deterministic conflict resolution
- Batched changes with receiver-side approval for large or delete-heavy batches
- Local trash with restore, and `history`
- LAN / direct / relay awareness with size rules
- Filesystem watching plus periodic rescans
- `.delocalignore` (gitignore syntax)
- One-line install, service install on first `up`, `delocal update`
- Resumable whole-file transfers

### 3.2 Not in v1 (deliberately)

- Windows
- Sharing with other Tailscale users (only your own machines)
- Selective sync, per-machine subsets of a folder
- Modes other than two-way (`send-only`, `receive-only`, `archive`) — data model reserves them
- Approve-once-for-all-machines
- Delta / block-level transfer
- Non-Tailscale networking
- Web portal or GUI
- Extended attributes, ACLs, ownership, non-exec permission bits
- Hard links (synced as separate files)
- Rename detection: a rename is a delete plus an add, and the content is transferred again

### 3.3 Requirements on the user's machines

- Tailscale installed, logged in, and running
- All machines belong to the same Tailscale user (see §5)
- A user session that can run a background service (see §16 on `loginctl enable-linger` for headless boxes)

---

## 4. Vocabulary

| Term | Meaning |
|---|---|
| **Machine / node** | One installation of delocal. Identified by a **node ID**. |
| **Node ID** | 16 random bytes generated on first `up`, stored in the state dir. Independent of Tailscale so it survives a Tailscale reinstall. Ordered by byte order; wherever this document says one node ID is "larger" than another, it means this. Displayed as 32 lowercase hex characters; the short form is the first 8. |
| **Tailscale identity** | The Tailscale stable node ID and the login name of the user who owns it. Used for trust, never for versioning. |
| **Folder** | A directory tree that is kept identical across a set of machines. Has a random **folder ID**, a name (its basename by default), and a per-machine local path. |
| **Member** | A machine that participates in a folder. |
| **Entry** | One path inside a folder: file, directory or symlink. |
| **Version** | A version vector: a map from node ID to a counter. Every entry has one. |
| **Batch** | A set of entry versions announced together by one machine. The unit of approval and of history. |
| **Held batch** | A batch a receiver has decided not to apply until a human approves it. Its versions are **quarantined**. |
| **Paused folder** | A folder whose owner machine has stopped sending because its own outgoing batch tripped the brake. |
| **Tier** | How a peer is currently reached: `lan`, `direct` or `relay`. |
| **Trash** | `.delocal/trash/` inside a folder. Where old copies go before delocal overwrites or deletes them. |
| **Tombstone** | The index record of a deleted entry. Carries the version of the deletion. Kept so a deleted file cannot be resurrected by a machine that was offline. |

---

## 5. Identity and trust

**Node identity.** On first `up`, delocal generates a random 128-bit node ID and stores it in `node.json` in the state dir. This is the identity used in version vectors and in history. It never changes for the life of the installation.

**Tailscale identity.** From the Tailscale local API (§6.1) delocal learns its own Tailscale stable node ID, its own user, and the same for every peer. Peers are displayed by their Tailscale hostname.

**Trust rule (v1).** An incoming connection is accepted only if the connecting Tailscale IP resolves (via `whois`) to a node owned by **the same Tailscale user as this machine**. Everything else is refused before the delocal handshake. There is no invitation flow, no pairing code, no approval prompt, because Tailscale has already established that these are your machines.

**[decision] Tagged devices.** Tailscale devices that are tagged (typical for servers) are owned by the tailnet rather than a user, so they fail the same-user test. v1 does not trust them by default. Escape hatch: `delocal machines trust <hostname>` on any of your machines marks that Tailscale node as trusted; the trust list is itself synced between your machines. This needs confirming — many homelab boxes are tagged, and requiring one command for them may be the right trade-off or may be friction worth removing.

**Binding.** On first successful handshake, delocal records `node ID ↔ Tailscale stable node ID`. If a known node ID later appears from a different Tailscale node, the connection is refused and `status` shows a warning. This catches a state dir restored from a backup onto a second machine, which would otherwise corrupt version vectors.

**Encryption and authentication** of the transport are WireGuard's job. delocal's listener binds only to the machine's Tailscale IPs, never `0.0.0.0`. The docs recommend a Tailscale ACL that restricts the delocal port to the user's own devices, but delocal does not depend on it.

---

## 6. Networking

### 6.1 Tailscale integration

All Tailscale-specific code lives in `crates/delocal/src/tailscale/` behind one trait:

```rust
trait Tailscale {
    fn self_info(&self) -> Result<SelfInfo>;        // ips, stable id, user, hostname
    fn peers(&self) -> Result<Vec<Peer>>;           // hostname, ips, stable id, user, online, tags, path info
    fn whois(&self, ip: IpAddr) -> Result<WhoIs>;   // stable id, user, tags
    fn watch(&self) -> impl Stream<Item = Event>;   // optional: state changes; else poll
}
```

Preferred implementation: the **Tailscale LocalAPI**, an HTTP API on a local socket that the `tailscale` CLI itself uses. On Linux the socket is `/var/run/tailscale/tailscaled.sock`. **[verify]** The macOS GUI builds (App Store and standalone) do not expose the socket the same way; they use a localhost port plus a token. Confirm the discovery mechanism during Phase 3.

Fallback implementation: shell out to `tailscale status --json` and `tailscale whois --json <ip>` and parse. Both implementations must pass the same test suite against recorded fixtures. Neither is a stable public API, which is exactly why they are isolated here.

Fields used: own IPs and user; per peer: hostname, Tailscale IPs, online flag, stable ID, user, tags; per peer path: the current direct address if any and the relay in use if any. **[verify]** field names against the installed Tailscale version in Phase 3; treat every field access as something that may need a fallback.

### 6.2 Discovery

Every 30 seconds, and immediately on a Tailscale state change if the API offers events, delocal fetches the peer list. For each online peer that passes the trust rule:

- If the peer is known to run delocal: connect (subject to §6.3 dial rule).
- If unknown: TCP-probe the delocal port with a 2 s timeout. Failure backs off (30 s, 1 m, 5 m, cap 5 m) so a tailnet full of non-delocal machines costs nothing noticeable.

There is no discovery protocol of delocal's own. The tailnet is the directory.

### 6.3 Transport

- TCP on port **41831** by default **[decision]**, bound to the machine's Tailscale IPs only. Configurable via `rules`, but every machine must use the same port in v1.
- Exactly one connection per pair of machines. **Dial rule:** the machine with the lexically smaller node ID dials; the other only listens. If both somehow connect, the connection initiated by the larger ID is dropped.
- Framing: `u32` big-endian length prefix, then a `postcard`-serialized message (`serde`). Maximum frame 16 MiB; file data is chunked below that.
- `Hello` is the first message in each direction and carries the protocol version, node ID, hostname and delocal version. Incompatible protocol versions are refused with a `Goodbye{reason}`; `status` shows "update needed on <machine>".
- Keepalive ping every 15 s; a connection with no traffic for 45 s is dropped and re-dialed with backoff.

### 6.4 Connection tiers

Recomputed for every connected peer every 30 s from the Tailscale peer path info:

| Tier | Rule |
|---|---|
| `lan` | Reached directly **and** the peer's current address is a private range (RFC 1918, link-local, or on the same subnet as one of this machine's non-Tailscale interfaces) |
| `direct` | Reached directly, but not `lan` |
| `relay` | Traffic goes through a DERP or peer relay |

If path information is unavailable, assume `relay` (the most conservative).

### 6.5 Size rules per tier

Metadata (batches, index) always flows on every tier. File data is subject to:

| Tier | Default |
|---|---|
| `lan` | Everything |
| `direct` | Files over **1 GiB** are deferred |
| `relay` | Files over **50 MiB** are deferred **[decision]** |

A deferred file stays in the receiver's want-list, marked `waiting for lan` (or `waiting for direct`), appears in `status`, and is fetched when a peer holding that version is reachable on a suitable tier. Thresholds are per folder via `delocal rules`. Because the receiver decides what to pull, the rule is evaluated where the bandwidth is spent.

---

## 7. Sync model

This section is the core of the system and the part the simulator (§14) exists to prove. The engine that implements it is pure: it takes events (scan results, incoming messages, timer ticks) and returns actions (write file, send message, record history). It does no I/O itself. It also takes time and randomness as inputs: timestamps and fresh identifiers (batch IDs, node IDs) arrive in events, generated by the host. The engine never reads a clock or a random number generator. This is what makes it testable by a deterministic simulator.

### 7.1 The index

Per folder, one record per entry the machine knows about, including deleted ones:

| Field | Notes |
|---|---|
| `path` | Relative, forward slashes, no leading `./`. NFC-normalised **by the host** before it reaches the engine; the engine treats paths as opaque UTF-8 |
| `kind` | `file` / `dir` / `symlink` |
| `size` | bytes (0 for dir; target length for symlink) |
| `mtime_ns` | Files only, as observed or received. **0 for directories and symlinks**: a directory's mtime changes whenever a child is created or removed, so syncing it would make every file change ripple into a directory "touch" on every machine, forever; symlink timestamps are not worth the platform differences. For a tombstone, the time the deletion was observed on the machine that made it |
| `exec` | bool; the only permission bit synced |
| `hash` | BLAKE3 of content (files), of the target string (symlinks); the all-zero sentinel `EMPTY` for directories and tombstones. `EMPTY` is never the hash of a file (BLAKE3 of empty input is not all zeros) |
| `prev_hash` | The `hash` of the version this one replaced, as it was when the change was made; `EMPTY` if the path did not exist. Set by the author, carried with the entry. A version whose `hash == prev_hash` is a **metadata-only change** (a touch). Used by the conflict rule (§7.6) and the brake (§8.1) |
| `stamp` | A Lamport timestamp seeded by the modification time, in nanoseconds. For a content change: `max(mtime_ns, stamp of the record being replaced + 1)`; for a metadata-only change, a tombstone, a directory or a symlink: `stamp of the record being replaced + 1`; for a path with no record: `mtime_ns` (or 1 when the kind has none). Set by the author, carried with the entry; a merged record inherits the winner's. Strictly increasing along every machine's chain of versions for a path, which is what makes the winner rule (§7.6) converge. Never compared to a clock |
| `version` | version vector (§7.2) |
| `deleted` | bool — this record is a tombstone |
| `modified_by` | node ID that produced this version |
| `author_host` | Tailscale hostname of `modified_by` as that machine reported it when it made the change. Lowercase, `[a-z0-9-]` only, at most 63 characters. Carried with the entry so every machine has identical data; used for conflict-copy names and display only, never for comparison |
| `seq` | this machine's per-folder sequence number when this record was last written locally |

Plus, per folder per remote member: the highest `seq` of theirs we have received, used for catch-up (§7.4).

**When `seq` advances.** A record gets a new `seq` only when it describes committed local state: after a local change is observed, after a fetched file has been renamed into place, after a deletion or a metadata-only change has been applied. Accepting a batch never advances `seq`. This is what makes "a member's index announces version X" mean "that member can serve X" (§7.5).

### 7.2 Versions

```
Version = BTreeMap<NodeId, u64>
```

- **Local change to an entry:** take the entry's current version (an absent entry has the empty version), and set `v[self] = v[self] + 1`, with a missing key read as 0. There is no counter outside the entry's own vector. The per-folder `seq` in §7.1 is for index catch-up only and never participates in version comparison. Deletion is a local change like any other; it produces a tombstone with a new version.
- **Comparison** of `a` and `b`, treating missing keys as 0:
  - `a == b` → **equal**
  - every `a[k] ≥ b[k]` and not equal → **a dominates**
  - every `b[k] ≥ a[k]` and not equal → **b dominates**
  - otherwise → **concurrent** (a conflict, unless the content hashes are equal — see §7.6)
- **Merge** (used when concurrent versions have identical content): component-wise maximum, no increment. Two machines merging the same pair independently produce the same result, so they converge without talking. The merged record takes its metadata (`mtime_ns`, `modified_by`, `author_host`, `prev_hash`) from the side the §7.6 winner rule ranks higher: the same total order that decides conflicts, applied whole, never a separate tie-break (see "One order for everything" in §7.6). If the file's mtime on disk then differs from the record, the host sets it (a metadata-only apply, §7.5).

Wall-clock time never participates in ordering. See §7.8.

### 7.3 Local change detection

- `notify` watches every folder root recursively. Events are debounced (2 s of quiet per path).
- A **full scan** runs at daemon start, every hour **[decision]**, and on `delocal scan`. Watchers drop events under load; the scan is the ground truth.
- Fast path: a file whose `size`, `mtime_ns` **and exec bit** match the index is unchanged; a directory or symlink whose kind matches is unchanged. Anything else is hashed. The exec bit is in the fast path because `chmod` changes neither size nor mtime: if its watcher event is dropped, a fast path on size and mtime alone would never notice, and the index would disagree with the disk forever. `stat` returns the mode anyway, so this costs nothing.
- **A change in mtime alone** (size and hash unchanged) is still a change: it produces a new version with `hash == prev_hash` and propagates, so that every machine holds the same `mtime_ns` for the same version and the conflict tie-break stays deterministic. Receivers apply it as a metadata-only apply (§7.5); it is invisible to the brake (§8.1); and it loses to any real content change in a conflict (§7.6).
- **mtime precision shim [Phase 2].** Filesystems with coarse timestamps (FAT, exFAT, some network mounts) cannot store the record's `mtime_ns` exactly, so a received file would look touched on the next scan, gain a new version, propagate, and loop forever. After every `Write` or `SetMeta` the host reads back the mtime the filesystem kept; if it differs from the one requested, the host records the pair and reports the requested value to the engine on later scans while the stored value is unchanged. The engine never sees the discrepancy.
- **Stability check:** a file is not hashed until its mtime has been unchanged for 2 s, and if it changes during hashing the hash is discarded and retried. This avoids announcing half-written files.
- Hashing is BLAKE3, parallel across files, streamed for large ones.
- Always ignored: `.delocal/` at the folder root. User rules: `.delocalignore` at the folder root, gitignore syntax via the `ignore` crate. Default rules shipped for every folder: `.DS_Store`, `._*`, `*.swp`, `*~`, `.#*`, `.Trash*`.
- Directories are tracked so that empty directories sync. Symlinks are synced as symlinks and never followed. Files that cannot be read (permissions) are skipped and counted in `status`.
- **Folder root guard:** each folder has a marker `.delocal/folder.json`. If the marker is missing on a scan (the drive was unmounted, the directory was deleted wholesale), the scan aborts, the folder is paused with a clear status message, and **no deletes are announced**. This is the cheapest protection against "unmounted disk → mass delete everywhere" and it is non-negotiable.

### 7.4 Batches

Announcing changes is always done in batches. A batch is metadata only; file data is pulled separately.

**Forming a batch (sender).** Detected local changes accumulate. When there has been 2 s of quiet, or 10 s since the first change in the window, whichever comes first, the accumulated entries become one batch:

```
Batch {
  id: BatchId (random 128-bit),
  folder: FolderId,
  source: NodeId,
  created_at: unix time (informational),
  entries: [ { path, kind, size, mtime_ns, exec, hash, version, deleted, modified_by, author_host } ],
  summary: { adds, mods, dels, bytes }
}
```

Before sending, the sender runs the **brake pre-check** (§8.1) against its own batch. If it trips, the folder is **paused**: the batch is kept locally, nothing is sent, `status` says so, and the user resolves it with `approve` or `revert` (§8.3).

Otherwise the batch is sent to every connected member of the folder and recorded in history.

**Adopted records are announced too.** A batch carries every record written since the last batch: local changes with their coalesced kind (§7.1), and records adopted from peers (§7.5) as they stand. Announcing adoptions is what lets a change reach a machine that is not connected to its source (A → B → C while A and C cannot see each other), and it is what carries a conflict's merged version `M` (§7.6). Receivers drop equal and dominated versions, so the redundancy costs one comparison per entry. This makes a live batch and catch-up the same mechanism: both are "records with `seq` above what this peer has acknowledged".

**Receiving a batch.** For each entry, compare its version with the local record (absent = empty version, dominated by everything):

- incoming **dominates** → candidate to apply
- incoming **dominated** or **equal** → ignore (we already have it or newer)
- **concurrent** → conflict handling (§7.6) produces zero or more candidate actions

The set of candidates is the batch's **apply set**. The brake (§8.1) is evaluated over the apply set. Result: `accepted` (apply set goes into the want-list) or `held` (versions quarantined, §8.2). The receiver replies `BatchDecision { id, decision, seq_high }`, and records the batch in history either way.

**Ordering and acknowledgement.** Entries in a batch are ordered by the sender's `seq`, not by path, and a batch that would exceed 10,000 entries is split on `seq` boundaries, so every batch covers a contiguous range of the sender's `seq` and its `seq_high` is a true watermark. The receiver's `BatchDecision` is the acknowledgement: `Accepted` and `Held` both mean "I have your records up to `seq_high`" (held versions sit in quarantine and are not re-sent). There is no separate ack message. Every batch also carries `seq_low`, the `seq_high` of the sender's previous batch for that folder (0 for the first), so a sender's batches chain. A receiver whose highest contiguous `seq` from that sender is below a batch's `seq_low` has a **gap**: it processes the batch anyway (versions are self-describing, so order does not affect correctness) but acknowledges only the highest contiguous `seq`, tracking received ranges above it until the gap fills. A sender whose batch is acknowledged below its `seq_high` re-sends `records_since(ack)` to that peer at the next tick, exactly as catch-up does. `have_up_to` is the same contiguous watermark. This makes the protocol correct over a transport that loses or reorders messages, rather than relying on TCP's ordering across reconnects. Receivers sort their apply set by path before applying, which is where §7.5's parents-before-children order comes from. The sender's `summary` counts only this machine's own local changes, classified as in §8.1; adopted records it relays are not counted, since they already passed this machine's receiver brake, and every receiver computes its own counts over its own apply set anyway.

**Catch-up.** When two members connect, each tells the other the highest `seq` of theirs it holds, per folder (`have_up_to` in `FolderMeta`, §12), and each sends every index record with `seq` greater than that, packaged as one or more batches (max 10,000 entries each, split on `seq`). Only **announced** records take part (`seq` at or below the last announced `seq`): unannounced local changes go through the live path and its pre-check, so a paused folder's pending set can never leak through a reconnect. The peer's `have_up_to` **replaces** the sender's memory of acks rather than being combined with it; it is the truth about what the peer holds, and a lower value means the peer lost state and needs the records again. These go through exactly the same brake. A brand-new member receiving the whole folder sees a batch of pure adds, which the count rule ignores (§8.1), so first sync is never held by count; it can still be held by size.

**Why batches and not a live index stream:** every batch is a natural unit for approval, for `history`, for `review` and for `revert`. Syncthing streams index updates continuously and has no such unit, which is why its safety story is weaker. The cost is up to 10 s of latency on a change, which is acceptable.

### 7.5 Applying changes

**Want-list.** Accepted entries go into a per-folder want-list, one want per path: the entry to end up with (`M` for a conflict), its apply mode, and its `sources`: the members that announced a version **with the wanted content** at that path, meaning the batch's source, the version's author, and any later batch carrying a version at that path with the same hash. Because fetches are by hash (below), whoever announced `W` is a source for `M`. The engine does not model every peer's index; a source that has moved on answers `NotAvailable` and is dropped from that want. **Selection**: among connected, non-excluded sources the best tier wins (`lan`, then `direct`, then `relay`), ties broken by smaller node ID so two machines with the same view pick the same source; a source whose tier limit (§6.5) the file exceeds is skipped. No candidate → the want is *without source*; every candidate skipped → *deferred*, naming the least demanding tier that would allow it. Both are re-evaluated whenever a peer connects, disconnects or changes tier. At most `max_fetches_per_peer` (default 4) and `max_fetches_per_folder` (default 16) fetches run at once, held in `Rules`, so the engine, not the host, decides parallelism and a deadline means something. **Deadlines**: a fetch with no progress reported for 60 s, or a commit not reported within 30 s, returns the want to *wanted* and reselects, without excluding the source (the stall may have been ours); the host reports fetch progress at most every few seconds. Transfers resume from the temp file's offset, so a false stall costs a round trip, not the bytes. A second hash mismatch from a second source gives the want up; it retries when a new source announces the version, when a peer connects, or when the index changes, so a transient corruption never strands a file.

**In flight.** A path whose want is in a short-lived state (*wanted*, *blocked* on ordering, *fetching*, *committing*) is in flight: observations of it are ignored until the commit succeeds or fails, and the scan bracket's deletion pass skips it. A path whose want is *deferred*, *without source* or *given up* is observable: those states can last for days, and hiding a local edit for days would be a silent loss of sync. The one exception is a want created by `revert` (§8.3): the file is in the trash, so an `Absent` observation is ignored in every state (it is the trash move), while an observed file at the path is a real local change that dominates the restored version and cancels the want.

**Fetching a file.**

1. Pick a source (prefer `lan`, then `direct`, then `relay`).
2. Send `RequestFile { folder, path, hash, offset }`. Content is requested **by hash, not by version**. The source serves the file at `path` if its index record there is live with that hash and the file on disk still matches the record (size and mtime); failing that, any live file in the folder with that hash; failing that, `NotAvailable`. Requesting by hash is what lets a conflict's merged version `M` be fetched from the winner's holders before they have merged (`M` has `W`'s content), lets every concurrent holder of the same bytes serve them, and lets the host satisfy a fetch from its own disk when the content is already local (a rename arrives as a delete plus an add; the host may copy from the old path or the trash and report `Fetched Ok` without asking anyone) [Phase 2]. The `offset` is the size of any partial temp file from an earlier attempt, so transfers resume.
3. Data arrives as `FileData` chunks (1 MiB) into `.delocal/tmp/<hash-prefix>-<random>`. The source refuses (`NotAvailable`) if it has no live file with that hash; the receiver removes that source from the want and tries another. A source that later announces a version with the wanted content becomes a source again.
4. On completion, verify BLAKE3 against the expected hash. Mismatch → discard, retry once from a different source, then give up on that item until the index changes.
5. Set mtime and exec bit on the temp file.

**Committing.**

6. Check the target path is still what the index said it was when the decision was made (same `size` and `mtime_ns` for a file; same kind for a directory or symlink; or absent). If not, the local file changed underneath us: abort the commit and report `ChangedUnderneath`. The same outcome is reported if the commit's displacement target (§7.6) exists when the host gets there. The engine keeps the incoming entry and re-evaluates it against the index after the next observation of that path arrives, which will find a new local version and classify the pair as a conflict (§7.6) or as dominated. The incoming version is never dropped: the sender has already been acknowledged for it and would not send it again.
7. If a file exists at the target, **move it to trash** (§8.4), or, when the commit resolves a conflict and the existing file is the losing content, to the conflict-copy path (§7.6). Same filesystem, so this is a rename either way. The engine says which in the commit action; the host never chooses.
8. `rename(tmp, target)`. Ensure parent directories exist (creating them as index entries if they arrived in the same batch).
9. Update the index record and `fsync` the parent directory.

**Metadata-only applies.** An incoming version that dominates the local one but has identical content (content equality as in §7.6: kind, hash, exec) needs no fetch, no trash and no write. The host sets the file's mtime to the record's `mtime_ns` (files only; directories and symlinks carry no mtime, §7.1, so for them this is an index-only update with no disk action) and the index adopts the incoming version, `modified_by` and `author_host`. This is how a conflict's merged version `M` (§7.6) lands on a machine that already holds the winning content, and how `deny` bumps (§8.2) land on machines that hold the same copy. A version that differs only in the exec bit is applied the same way, by changing the bit, with no transfer.

**Deletes.** Move the current file to trash, write the tombstone to the index. Directories are removed only when empty and only after all children in the batch have been processed. Order: creates process parents before children; deletes process children before parents.

**Symlinks.** Written with `symlink(target, tmp)` then rename. Never followed.

**Crash safety.** The index is updated only after the rename. Temp files are content-addressed enough to resume; anything in `tmp/` that no want-list item claims is removed at daemon start.

### 7.6 Conflicts

Two versions of the same path are in conflict when they are **concurrent** (§7.2) **and** their hashes differ (kind, hash, and for files exec bit; mtime is not compared). Concurrent versions with identical content are not a conflict: both sides merge vectors and move on. This rule matters because it is how independently-made identical changes, and the conflict-copy mechanism itself, converge without producing duplicates.

**One order for everything, and it must be monotone.** The winner rule below is a total order on entries and it is applied to **every** concurrent pair, whether or not their content differs; for identical content it only decides which side's metadata the merged record carries. One order is necessary but not sufficient. Draft 15 argued that a merged record's content is then the maximum, under the order, of the increment-created versions in its causal past, and so a function of its vector. The simulator showed that argument was wrong (seed 2, seed 111): it only holds if every increment ranks **above** the version it replaces, and rules that look at content do not have that property. A delete ranks below the live file it replaces, a touch below the edit, a symlink or directory (no mtime) below the file. A machine that meets the newer increment alone and a machine that meets the older one first then compare different frontiers, and pairwise maximum over different sets gives different content under the same vector; each side then drops the other's as `Equal`, forever.

The fix is a first key that is strictly increasing along every chain: the **stamp** (§7.1), a Lamport timestamp seeded by the modification time. A content change stamps `max(mtime_ns, previous stamp + 1)`; a metadata-only change, a tombstone, a directory or a symlink stamps `previous stamp + 1`; a merged record inherits the winner's stamp. Because an increment's stamp exceeds the stamp of the record it was made from, and that record's content is by induction the maximum over its own past, every increment ranks above everything in its causal past. With that, the merged record's content **is** the maximum over the increment-created versions in its vector's past, pairwise maximum computes it exactly (the past of a union is the union of the pasts, since a node's versions form a chain), and two machines holding the same vector hold the same content. That is what lets a receiver drop an `Equal` version unread. The remaining rules are tie-breaks among versions with equal stamps, which is where they were always meant to apply: two branches that diverged from the same ancestor at the same moment.

What this means for the user: the most recent edit wins, by a clock that can only move forward along each machine's history; a deletion or a touch never leaps ahead of the file it replaced, so a concurrent edit that is newer than the deleted file's last edit wins, and an older one loses and survives as a conflict copy rather than undoing the deletion. The simulator checks the consequence directly (I7): on every node, equal vectors at a path imply equal content and deletion state.

**Deterministic winner.** Every machine must pick the same winner without communicating:

1. The version with the larger `stamp` wins.
2. Tie: if exactly one side is a tombstone, the live side wins.
3. Tie: if exactly one side is a metadata-only change (`hash == prev_hash`, §7.1), the other side wins. A real edit is never demoted to a conflict copy by a touch made at the same moment.
4. Tie: the version with the larger `mtime_ns` wins.
5. Tie: the version whose `modified_by` node ID is larger (byte order, §4) wins.
6. Tie: larger `hash`, then `kind` (a symlink and a file with the same bytes tie on hash), then `exec` set, then `size`, `prev_hash`, `author_host`, so the rule is total. Two concurrent versions from one author should be impossible (a node's versions of a path form a chain), so this step exists only to make the rule total; the simulator counts how often it fires, and any count above zero is a bug to find.

**Actions.** Let `W` be the winning version and `L` the losing one, and let `M = merge(W, L)` (§7.2): `W`'s content fields (kind, size, mtime_ns, exec, hash, modified_by, author_host, prev_hash) under the component-wise maximum of the two vectors. `M` dominates both `W` and `L`, and every machine computes the same `M` from the same two inputs, so no increment is needed and no machine has to be told what the others decided. (This is the identical-content merge of §7.2 with a rule for whose content to keep. `deny` in §8.2 does increment, because the choice it encodes is the user's and two machines could choose differently.)

- A machine that currently **holds L** locally fetches `W`'s content, then commits in one host operation: the existing file is moved to the conflict-copy path instead of the trash, and `W`'s content is renamed in (§7.5 step 7). The index adopts `M` at the original path and records the conflict copy as a local add at the conflict path: `L`'s kind, size, mtime_ns, exec and hash, `prev_hash` as §7.1 defines it (`EMPTY` unless peers last saw a live entry at the conflict path, for instance an earlier copy deleted inside the current window), a fresh version, and **this machine** as `modified_by` and `author_host`, because the copy is this machine's change (`L`'s author survives in the copy's name). The path is never absent in between, so no scan can mistake the displacement for a deletion. If the conflict path already holds a live record when the conflict is classified (another `L`-holder's copy arrived first), the displaced file goes to the trash instead and the record already there is the conflict copy; if the target appears between classification and commit, the host reports `ChangedUnderneath` (§7.5 step 6) and the entry is re-evaluated.
- A machine that currently **holds W** locally adopts `M` as a metadata-only apply (§7.5): nothing changes on disk.
- A machine that holds **neither** (an older version, or nothing) applies `M` as an ordinary change and never creates a conflict copy.

Every machine announces `M` once it has adopted it (§7.4); receivers already hold it and ignore the equal version. Conflict copies made by several `L`-holders have the same path and the same content, so their versions merge under §7.2.

**Conflict copy name** is deterministic so that several machines renaming `L` independently produce the same path and the same content, which then merge. It is built only from data carried with `L` itself, never from anything looked up locally:

```
<stem>.conflict-<L mtime as UTC YYYYMMDD-HHMMSS>-<L.author_host><ext>
report.conflict-20260922-143005-laptop.xlsx
```

If `author_host` is empty (should not happen, but the format must be total), the short form of `L.modified_by` is used instead. The name is split at the last `.` that is not its first character, so `archive.tar.gz` becomes `archive.tar.conflict-…gz`; a dotfile with no other dot (`.bashrc`) and any directory take the suffix on the whole name. If the result would exceed 255 bytes, the stem is cut first and then the extension, at character boundaries, so the component stays valid and always contains `.conflict-`.

**Special cases.**

- **Delete vs modify:** decided by the stamp like everything else. A tombstone's stamp is its predecessor's plus one, so a concurrent edit made after the file's last edit wins and the deletion is dropped; an edit older than that loses, and because the machine holding it displaces it to a conflict copy before applying the tombstone, the edit survives in the folder under the conflict name. A file that someone is editing never vanishes; at worst it is renamed.
- **Modify vs modify on a directory** cannot happen; directories carry no content.
- **File vs directory at the same path:** the winner rule decides; the loser is renamed with the conflict suffix. Expected to be vanishingly rare. If the loser is a non-empty directory, displacing it moves its children too: their old records are tombstoned and the moved children appear as adds on the next scan, which may trip the brake on that machine. Known behaviour, accepted; the brake is the safety net.
- **Case-insensitive filesystems (macOS default):** two index paths that differ only by case cannot both exist. Neither is applied; `status` reports the pair and the user resolves it on a case-sensitive machine. **[decision]** This is the Syncthing approach and is good enough for v1.

### 7.7 Tombstones

A deleted entry keeps its index record with `deleted = true` and the deletion's version. Tombstones are announced, stored, and compared like any other version. A machine that was offline and still has the file will, on catch-up, receive a tombstone that dominates its version and delete (to trash) locally; it will not resurrect the file. A machine that **modified** the file while offline has a concurrent version → delete-vs-modify → the file survives everywhere.

**[decision] Retention:** tombstones are kept indefinitely in v1. Each is a few hundred bytes; a folder with a million historical deletions would carry a few hundred megabytes of index, which is tolerable for v1. Pruning (for example, once every member has acknowledged a tombstone's `seq`) is a v1.1 item and must never be done unilaterally.

### 7.8 Clocks

Wall-clock time is used for exactly four things: local change detection (mtime vs index), seeding the `stamp` of a content change (§7.1), the conflict tie-breaks below the stamp, and preserving mtimes on received files. It never orders events; version vectors do. If a peer's `Hello` timestamp differs from local time by more than 5 minutes, `status` shows a warning, because the tie-break is less trustworthy.

---

## 8. Safety net

### 8.1 The brake

A batch is **held** if either rule trips, evaluated over the batch's apply set (receiver) or the batch itself (sender pre-check):

| Rule | Default |
|---|---|
| **H1 · count** | `dels + mods ≥ 50` **and** `dels + mods ≥ 25%` of the folder's tracked entries before the batch. Tracked means live and not a directory; directories are excluded from both sides of the ratio, since a directory and its tombstone have the same (empty) content |
| **H2 · size** | total bytes of adds + mods `> 20 GiB` **[decision]** |

- Adds do not count toward H1. Adds are almost never destructive, and exempting them keeps first sync and bulk imports quiet.
- H1 cannot trip on a folder with zero tracked entries.
- A batch entry counts as a `mod` only if it changes content: its `hash` differs from its `prev_hash`, or its kind or exec bit differs from the record it replaces. Metadata-only changes (touches, §7.3) and metadata-only applies (§7.5) count as neither `mods` nor `dels` for H1 and contribute no bytes to H2, on the sender pre-check and the receiver alike. Exec-bit-only changes count as `mods` and contribute no bytes on the receiver, which can see the record being replaced. The sender pre-check cannot (after coalescing it no longer holds the replaced record, and `hash == prev_hash`), so it classifies them as metadata-only; a mass `chmod` is not destructive, and the receiver check still counts it.
- Thresholds are per folder: `delocal rules ~/Sync --hold-count 50 --hold-pct 25 --hold-size 20G`. Setting `--hold-count 0` disables H1 for that folder. Changing the rules never releases a held batch or unpauses a folder by itself: `status` says what would now pass, and `approve` releases it. Every release is an explicit act, wherever the rule change came from (rules travel with folder metadata, §9.1).

**Sender pre-check.** The same rules run on the machine where the changes happened, before anything is sent. If they trip, the folder is **paused** on that machine: nothing leaves, and `status` everywhere shows `paused on <machine>: 812 deletes pending — delocal review on <machine>`. This is cheap and it is what makes `revert` trivially safe: no other machine has seen the damage. The sender's H1 denominator is the tracked count **as of its last announcement**, not the current one: after `rm -rf` of 800 of 1,000 files the current count is 200, which is exactly the wrong number. Once paused, a folder stays paused until `approve` or `revert`, even if later local changes bring the pending set back under the thresholds; `status` says so. The batch that would have been sent gets a batch id at the moment of pausing, shown by `status` and `review`, and `approve <id>` sends it.

A paused folder **keeps receiving**: remote batches are applied normally for paths not in the pending batch, and paths that are in the pending batch are left untouched until `approve` or `revert`: every incoming version for such a path is kept, in arrival order, and re-classified when the folder unpauses (dropping one would lose it, since the sender has been acknowledged for it). Further local changes made while paused join the pending batch, the brake is re-evaluated over the whole of it, and `revert` undoes all of it.

**Receiver check.** Runs regardless of what the sender did. A machine running an old or broken delocal, or one whose user approved something hastily, still cannot push a mass change onto a machine that has not agreed.

Counting only. No content analysis, no entropy heuristics, no attempt to recognise ransomware. This catches `rm -rf` in the wrong terminal, a bad script, an unmounted disk, and encryption-style damage (which always looks like mass modification or delete-plus-add) with one simple, testable rule.

### 8.2 Quarantine

When a receiver holds a batch, every incoming entry that produced an apply-set item is written to the `quarantine` table **as received**, not as classified: a conflict's `M` (§7.6) never appears on the wire, and quarantining it instead of the incoming version would let that version through from another peer. Quarantine is by **version**, not by sender:

- The same version offered later by any other member is still held.
- Any version that **dominates** a quarantined version (the source kept editing after the event) is also quarantined, and joins the same review item.
- Unrelated paths from the same source are unaffected; sync continues for them.

`delocal review` shows each held item: source, time, counts, size, sample paths, file-type breakdown. Then:

- **`approve`** re-classifies the quarantined entries against the index as it stands and applies the result normally. Trash still protects every overwritten or deleted file.
- **`deny`** makes this machine's current copies win. For every quarantined path it produces a new version equal to the component-wise maximum of its local version and every quarantined version for that path, with its own counter incremented, so the result dominates all of them; then it drops the quarantine. Where this machine has no record for the path, the new version is a tombstone. Content is unchanged, so on machines that hold the same content this lands as a metadata-only apply (§7.5) and does not trip the brake. The mesh converges on this machine's copies. On the source machine this arrives as a mass modification and may itself trip that machine's brake; that is correct — the user is already in "something went wrong" mode and approving it there restores the source. **[decision]** This is the simplest correct semantics for `deny`; an alternative is for `deny` to only refuse and tell the user to run `revert` on the source.

Approving on one machine does not approve on others in v1 (§3.2).

### 8.3 Revert (on a paused sender)

`delocal revert ~/Sync` on the machine that paused itself means "make this folder look like the rest of the mesh again":

1. Discard the pending batch.
2. For every path in it: move the current local file (if any) to trash, and reset the index record to the last version that was actually announced, restoring that record exactly, `seq` included, so it is not re-announced. A path peers never saw has no announced version; its record is removed.
3. Adds that were part of the pending batch are also moved to trash **[decision]** — in the destructive scenarios (encryption, a script writing junk) they are the debris.
4. The normal want-list logic re-fetches every reverted path from peers, which still have them because nothing was sent. Each restored path follows the in-flight rule of §7.5 with the `revert` exception: an `Absent` observation is ignored in every want state, because the file is in the trash and reporting its absence would announce the very deletion `revert` exists to prevent; a real file appearing at the path is a local change that cancels the want.

`revert` is only meaningful on a paused sender. On other machines it is a no-op with an explanatory message.

### 8.4 Trash

Location: `<folder>/.delocal/trash/YYYY-MM-DD/<relative path>`; name collisions within a day get a `~1`, `~2` suffix. Living inside the folder means every move is a same-filesystem rename: instant, no copy, no extra disk pressure until pruning.

What goes in: every file delocal is about to overwrite or delete because of a **remote** change, plus files moved aside by `revert`. Nothing else — delocal never sees a local edit until it has already happened, and it is not a backup.

Pruning: entries older than **30 days**, and oldest-first when the trash exceeds **10 GiB** per folder **[decision]**. Both per folder via `rules`.

Commands: `delocal trash [folder]` lists; `delocal restore <path> [--from YYYY-MM-DD]` copies the trashed version back to its original path as a normal local change, so it syncs out.

### 8.5 History

Every batch sent or received is a row in `batches` (id, folder, source, time, adds, mods, dels, bytes, decision, decided at) with its entries in `batch_entries`. `delocal history [folder]` lists recent batches; `delocal history <path>` shows everything that ever happened to one path across all machines' batches this machine has seen.

### 8.6 What the safety net does not do

- It is **not a backup.** If you edit a file badly on the machine you are sitting at, delocal faithfully syncs the bad edit. Trash only holds copies displaced by other machines. The README says this in the first screen.
- It does not detect malice or content. A slow, low-volume change that stays under H1 spreads normally.
- `revert` cannot recover changes that were already approved somewhere. For those, `trash` and `restore` are the path.

---

## 9. Folders and membership

### 9.1 Model

```
Folder {
  id: FolderId,
  name: String,                // basename of the path it was first shared from
  created_by: NodeId,
  members: Map<NodeId, Member>,
  rules: Rules,                // brake thresholds, tier limits, trash policy
}
Member { path: LocalPath, mode: Mode, joined_at }
Mode = TwoWay                  // v1. Reserved: SendOnly, ReceiveOnly, Archive
```

Folder metadata is itself synced between members (a small, separately-versioned record), so `share` on one machine is visible everywhere.

### 9.2 Default membership: every machine, same path

`delocal share ~/Sync` creates the folder and offers it to **all** of your machines running delocal. Each machine auto-joins into the **same path relative to `$HOME`** (`~/Sync` → `~/Sync`). Paths outside `$HOME` map to `~/delocal/<name>` on other machines.

A machine auto-joins only if the target path does not exist or is an empty directory. **[decision]** If the path exists and is non-empty, the folder shows as `pending` in `status` and the user runs `delocal add <name> --merge` (merge existing content: identical files merge silently under §7.6, differing ones become conflict copies) or `delocal add <name> --path <other dir>`. Auto-merging would be more Tailscale-like but a wrong guess here is exactly the kind of mess that makes people uninstall.

Explicit membership: `delocal share ~/Sync --to laptop,desktop`. Leaving: `delocal unshare ~/Sync` on the machine leaving; its files stay, its `.delocal/` directory is removed.

### 9.3 Machines that join later

On a new machine, `delocal up` discovers your other machines and lists the folders they share, with sizes. It offers `sync all / pick / none`. That is the one interactive moment in delocal, and it is the onboarding.

Folders shared **after** a machine joined auto-join if under **10 GiB** **[decision]**; larger ones show as `pending` in `status` so a Raspberry Pi is never surprised by a 500 GB folder.

### 9.4 Modes (reserved)

`Mode` is stored per member from day one so v1.1 can add `send-only`, `receive-only` and `archive` without a migration. `archive` (accept adds and modifications, ignore deletes) requires the archive member to suppress its "I still have this" from re-propagating; that logic is out of v1 but the engine's apply step is where it will go.

---

## 10. Command-line interface

`delocal` with no arguments prints help. Every command has `--help`. Output is plain when stdout is not a TTY; `status`, `machines`, `history`, `trash`, `review` accept `--json`. No command prompts when stdin is not a TTY.

| Command | Purpose |
|---|---|
| `delocal up` | Start delocal. First run: check Tailscale, create node ID, install and start the service, run onboarding (§9.3). |
| `delocal down` | Stop the service. |
| `delocal status [folder]` | Machines, tiers, folders, pending, held, paused, warnings. The one command most people ever run. |
| `delocal machines` | Your machines: hostname, tier, delocal version, last seen. `machines trust <host>` for tagged devices (§5). |
| `delocal share <path> [--to a,b] [--name n]` | Create a folder from a directory and offer it to your machines. |
| `delocal add <name> [--path dir] [--merge]` | Join a pending folder on this machine. |
| `delocal unshare <path\|name>` | Leave a folder on this machine. Files stay. |
| `delocal review [folder]` | Held batches (receiver) and paused batches (sender), with detail. |
| `delocal approve <batch\|--all>` | Apply a held batch, or release a paused one. |
| `delocal deny <batch>` | Refuse a held batch; this machine's copies win (§8.2). |
| `delocal revert [folder]` | On a paused sender: discard pending changes and restore from the mesh (§8.3). |
| `delocal history [folder\|path]` | Batch log, or the story of one path. |
| `delocal trash [folder]` | List trashed files. |
| `delocal restore <path> [--from date]` | Restore from trash. |
| `delocal rules [folder] [--set ...]` | Show or change thresholds, tier limits, trash policy, port. |
| `delocal scan [folder]` | Force a full rescan. |
| `delocal doctor` | Check Tailscale, service, socket, port binding, disk space, clock skew. Prints fixes. |
| `delocal update` | Download and install the latest release. |
| `delocal uninstall` | Stop and remove the service, binary and state. Never touches synced files or trash without `--purge`. |
| `delocal daemon` | Hidden. The long-running process the service runs. |

### 10.1 First run

```
$ curl -fsSL https://delocal.sh/install | sh
  installed delocal 0.1.0 to ~/.local/bin/delocal
  run: delocal up

$ delocal up
  ✓ tailscale running (dyllan@github, this machine: laptop)
  ✓ service installed (systemd user unit)
  ✓ found 3 of your machines running delocal: desktop, pi, nas

  they share 2 folders:
    ~/Sync    12.3 GB · 8,412 files
    ~/Notes   38 MB   · 1,204 files

  sync all of them to this machine? [Y/n/pick] y

  ✓ joined ~/Sync and ~/Notes — syncing in the background
  run `delocal status` any time
```

### 10.2 Status

```
$ delocal status
delocal 0.1.0 · dyllan@github · this machine: desktop

MACHINES
  desktop  this machine
  laptop   direct · lan        in sync
  pi       direct · remote     3 files pending · 2 waiting for lan (1.4 GB)
  nas      relay               in sync

FOLDERS
  ~/Sync   12.3 GB · 8,412 files   in sync
  ~/Notes  38 MB   · 1,204 files   ⚠ held: 612 deletes from laptop — delocal review
```

### 10.3 Review

```
$ delocal review
~/Notes · batch 7f3a from laptop · 2 min ago · HELD (count: 612 deletes = 51% of folder)
  deletes  612   modifies  0   adds  0   size  0 B
  sample:  archive/2019/…  archive/2020/…  archive/2021/…  (all under archive/)

  delocal approve 7f3a   apply on this machine
  delocal deny 7f3a      keep this machine's copies; they will win everywhere
```

### 10.4 Style

Colour via `console`/`owo-colors` when TTY, respecting `NO_COLOR`. Human-readable sizes and relative times. No spinners longer than a second; long operations happen in the daemon and `status` reports them. Errors say what happened and what to do next, one line each. Exit codes: 0 ok, 1 error, 2 usage, 3 needs attention (held/paused exists — useful for scripts and prompts).

---

## 11. On-disk layout

**State directory** (`$XDG_STATE_HOME/delocal`, default `~/.local/state/delocal` on Linux; `~/Library/Application Support/delocal` on macOS):

```
node.json        node id, created_at, tailscale binding
delocal.db       SQLite, WAL mode
daemon.sock      CLI ↔ daemon
logs/            rotating, 7 days
```

**Inside every folder:**

```
.delocal/
  folder.json    folder id (the root guard marker, §7.3)
  tmp/           in-progress transfers
  trash/         §8.4
.delocalignore   optional, user-owned, synced like any other file
```

**SQLite schema (sketch):**

```sql
folders        (id, name, created_by, rules_json, meta_version,
                announced_seq, announced_tracked, paused_json)     -- per-folder engine state that is small
pending        (folder, path, announced_record_json)              -- the record peers last saw, per unannounced path (§7.1)
acks           (folder, node, acked_seq)                          -- highest of our seq each peer acknowledged (§7.4)
deferred       (folder, path, entries_json)                       -- incoming versions waiting on a frozen or changed path (§7.5, §8.1)
members        (folder, node, path, mode, joined_at)
entries        (folder, path, kind, size, mtime_ns, exec, hash, version_blob, deleted, modified_by, author_host, seq,
                PRIMARY KEY (folder, path))
peer_seq       (folder, node, last_seq)
batches        (id, folder, source, created_at, adds, mods, dels, bytes, decision, decided_at)
batch_entries  (batch, path, kind, version_blob)
quarantine     (folder, path, version_blob, batch)
want           (folder, path, version_blob, hash, size, deferred_reason)
trash          (folder, trashed_path, original_path, trashed_at, size)
machines       (node, hostname, ts_stable_id, ts_user, trusted, last_seen, delocal_version)
```

`seq` per (folder, this node) is a monotonically increasing integer, incremented on every local write to `entries`.

**Persistence contract.** The engine's per-folder state (`FolderState`: index, pending set, sequence watermarks, peer seqs and acks, quarantine, wants, paused state, deferred entries) is the unit of restart: the daemon rebuilds it from these tables and hands it to `Engine::restore`. Every write the engine reports is made durable before the daemon feeds the engine its next event, so `seq` never rewinds and a crash loses only in-flight host operations and temp files. The engine reports index writes (`IndexChanged`, and `IndexRemoved` for the one case, `revert`, that removes a record) and want changes as actions today; Phase 2 adds the equivalent hooks for the remaining small state, or derives it from the tables above.

---

## 12. Protocol messages

```
Hello         { proto: u32, node: NodeId, hostname, version: String, now: i64 }
Goodbye       { reason }
FolderMeta    { folder, name, members, rules, meta_version, have_up_to: u64 }   // have_up_to: highest seq of the recipient's records the sender holds
Batch         { …§7.4, seq_low: u64, seq_high: u64 }   // contiguous chain per sender per folder
BatchDecision { batch: BatchId, decision: Accepted | Held { reason }, seq_high: u64 }   // doubles as the ack (§7.4)
RequestFile   { folder, path, hash: ContentHash, offset: u64 }   // by content, not version (§7.5)
FileData      { req: RequestId, offset: u64, bytes: Vec<u8> }     // 1 MiB chunks
FileDone      { req: RequestId }
NotAvailable  { req: RequestId }
Ping / Pong
```

Protocol version is a single integer, bumped on any incompatible change. Two machines with different protocol versions do not sync and `status` says which needs updating. Within one protocol version, messages may gain optional fields.

---

## 13. Failure handling

| Situation | Behaviour |
|---|---|
| Tailscale not running / logged out | Daemon idles and retries every 10 s. `status`: "waiting for tailscale". |
| Folder root missing (unmount, wholesale delete) | Root guard (§7.3): pause, warn, announce nothing. |
| Disk full during transfer | Abort that transfer, pause the folder's inbound, `status` warns. Resume when space frees. |
| Permission denied on a path | Skip, count, show in `status`. Never fatal. |
| Crash during transfer | Temp file resumes from offset on restart; verified by hash. |
| Crash after a commit's rename but before its report reached the engine | Next scan sees a "new local change" with content matching a known version → merges by identical-content rule. |
| Peer offers a version it no longer has | `NotAvailable`; try another source; otherwise wait. |
| Protocol mismatch | Refuse politely; `status` shows who needs `delocal update`. |
| Case collision on macOS | Neither applied; `status` names the pair. |
| Two machines with the same node ID (restored backup) | Binding check (§5) refuses the second; `status` warns. |
| Clock skew > 5 min | `status` warning. |

---

## 14. Testing strategy

Sync tools earn trust with tests, not features. Testing is Phase 1, not Phase 5.

### 14.1 The simulator (in `crates/sim`, depending on `delocal-engine`)

The engine is pure, so it can be driven by an in-memory filesystem and an in-memory network with a seeded PRNG. Each run creates N nodes (2–8), one or more folders, and then applies thousands of random steps:

- file create / modify / delete / rename, on random nodes, including the same path on several nodes in the same step
- network partitions and heals, node offline and returning after arbitrary time
- node crash: drop all in-flight host operations and temp files, restart from the persisted folder state (§11); a crash may land after a commit's rename but before its report, and the restarted node must recover through its scan
- message delay and reordering
- clock skew per node
- mass-delete and mass-modify events (to exercise the brake)

Invariants checked at the end of every run and at random quiescent points:

| # | Invariant |
|---|---|
| I1 | **Convergence.** When all nodes are connected and quiescent, every member has the identical set of paths, kinds, hashes and exec bits (excluding `.delocal/`). |
| I2 | **No loss.** Every content hash that was ever **announced** (appeared in a batch some node sent) exists at the end in some node's folder or trash. Content overwritten locally before it was ever announced is not protected, by design (§8.6). |
| I3 | **No resurrection.** A path deleted on a connected node and not concurrently modified is absent on every node after convergence. |
| I4 | **Bounded conflicts.** A losing version is a version that `winner` (§7.6) ranks below a concurrent version with different content at the same path. Every conflict copy present at the end has the deterministic name of some losing version at that path and that version's content, and there is exactly one copy path per losing version, never one per node. Two different losing versions may legitimately have identical content. |
| I5 | **Brake.** No batch that trips H1/H2 is ever applied without an explicit approve step in the simulation. |
| I6 | **Determinism.** Same seed → byte-identical outcome. |
| I7 | **Vector determines content.** On every node, after every index write: two records ever seen at a path with equal vectors have equal kind, hash, exec and deletion state. The one exemption is a machine's own re-issue after `revert`, which reuses a never-announced vector (§8.3). |

Run with `proptest` for shrinking. CI runs 1,000 seeds per push in a dedicated `simulate` job with its own timeout; a scheduled nightly workflow runs 100,000. A `SEEDS` environment variable controls the count. The pinned regression seeds (§14.4) are always required to pass. The random sweep is advisory (`continue-on-error`) until the first time it passes clean at 1,000 seeds, and a required check from then on; a required check that is red for weeks teaches everyone to ignore it.

### 14.2 Integration tests

Real filesystem in temp dirs, real TCP on loopback, `Tailscale` trait replaced by a fake that reports configurable tiers. Cover: two daemons syncing, watcher + scan agreement, atomic commit under concurrent writes, trash and restore, resume after kill -9 mid-transfer, `.delocalignore`.

### 14.3 Acceptance

Three real machines (Linux ×2, macOS ×1) on a tailnet, with a chaos script that edits, deletes, unplugs and reboots. Run for 48 hours before every release. A checklist lives in `TESTING.md`.

### 14.4 Also

Fuzz the frame decoder and `postcard` message parsing. Snapshot-test CLI output with `insta`. Every bug found in the field gets a simulator step that reproduces it before it is fixed.

---

## 15. Build plan

Each phase has a definition of done that is a test, not a feeling.

### Phase 0 — Skeleton (days)
Workspace, CI (fmt, clippy, test, cross-build for the four targets), `DESIGN.md`, `README.md` with the not-a-backup warning.
**Done:** `cargo build` produces static binaries for all four targets in CI.

### Phase 1 — Engine and simulator
Index, version vectors, batches, apply set, conflicts, tombstones, brake, quarantine, revert semantics — all as pure code. The simulator and invariants I1–I6.
**Done:** 100,000 random seeds pass with no invariant violation. This is the milestone that decides whether the design is right.

### Phase 2 — Local machinery
SQLite persistence, scanner, watcher, hashing, ignore rules, atomic commit, trash, root guard. A loopback transport so two daemons on one machine can sync two directories.
**Done:** two local daemons stay in sync under a 1-hour chaos script (random edits, kill -9, disk-full injection); trash holds every displaced file.

### Phase 3 — Network
Tailscale module (LocalAPI + CLI fallback, with recorded fixtures), discovery, transport, dial rule, tiers, size rules, resumable transfers, protocol versioning.
**Done:** three real machines converge from scratch; pulling a cable mid-transfer of a 5 GB file resumes and completes; a relay-only peer defers a 200 MB file and fetches it when the tier improves.

### Phase 4 — CLI and onboarding
All commands in §10, `status` and `review` output, service install (systemd user unit, launchd agent), `doctor`, `--json`.
**Done:** a person who has never seen delocal goes from a fresh machine to syncing in under two minutes using only what the tool prints.

### Phase 5 — Distribution
`install.sh` at `delocal.sh`, GitHub Releases with checksums, `delocal update`, Homebrew tap, docs site (a single page).
**Done:** `curl | sh` then `delocal up` works on Ubuntu, Debian, Fedora, Arch, Raspberry Pi OS (aarch64) and macOS.

### v1.1 candidates, in rough priority
Modes (§9.4) · approve-once via gossiped approvals over authenticated connections · tombstone pruning · delta transfers with block hashes · non-Tailscale transport (plain TCP + own keys) · selective sync · Windows.

---

## 16. Packaging and install

- **Installer:** `curl -fsSL https://delocal.sh/install | sh`. Detects OS and arch, downloads the release from GitHub, verifies the SHA-256 from the release manifest, installs to `~/.local/bin` (prompts for `/usr/local/bin` with `sudo` only if asked), adds a PATH hint if needed, prints `run: delocal up`. Never runs `delocal up` itself.
- **Binaries:** Linux via `musl` targets, fully static. macOS universal binary; codesigning and notarisation are a Phase 5 stretch goal.
- **Service:** on Linux a systemd **user** unit (`~/.config/systemd/user/delocal.service`). On headless boxes user units stop when the user logs out; `delocal up` detects this and offers to run `loginctl enable-linger`. On macOS a launchd agent in `~/Library/LaunchAgents`.
- **Updates:** the daemon checks GitHub Releases once a day (opt-out in `rules`) and `status` shows when a newer version exists. `delocal update` installs it and restarts the service. No silent self-update in v1.
- **Compatibility:** binaries within one protocol version interoperate. A protocol bump is a release note headline.

---

## 17. Open questions

Numbered so they can be referenced. Each has a default so building is never blocked.

1. **Tagged Tailscale devices** (§5): require `machines trust`, or trust all tagged devices on a tailnet that has exactly one user? Default: require trust.
2. **Non-empty target on join** (§9.2): pending + `--merge`, or auto-merge? Default: pending.
3. **`deny` semantics** (§8.2): this machine's copies win everywhere, or refuse-only? Default: win everywhere.
4. **Tombstone retention** (§7.7): forever in v1? Default: forever.
5. **Port** (§6.3): 41831? Default: yes.
6. **Relay size limit** (§6.5): 50 MiB? Default: yes.
7. **Full rescan interval** (§7.3): hourly? Default: yes.
8. **Trash cap** (§8.4): 10 GiB per folder? Default: yes.
9. **Auto-join size limit for late-shared folders** (§9.3): 10 GiB? Default: yes.
10. **Homebrew as the primary macOS path** vs the curl installer: Default: curl first, tap in Phase 5.

---

## 18. Decisions already made (from the planning conversation)

For the record, so they are not reopened by accident:

- Tailscale is required in v1; the network layer is isolated for a later non-Tailscale mode.
- Full mesh, version vectors from day one, no primary.
- Linux and macOS only. No Windows.
- Two-way only in v1; modes are reserved in the data model.
- Changes are announced in batches; receivers approve or hold; senders pre-check.
- Holds attach to versions, not senders (quarantine).
- Adds are exempt from the count rule.
- Tombstones for all deletes, kept.
- Deterministic conflict winner: mtime, then node ID; identical content never conflicts.
- Tailscale LocalAPI preferred over shelling out, in one module.
- Watchers plus periodic full rescans.
- Folders are IDs with per-machine paths; default is all your machines at the same relative path.
- SQLite for the index and history.
- The simulator is built in Phase 1.
- Name: **delocal**, domain **delocal.sh**.

---

## Appendix A — Crates (initial)

| Purpose | Crate |
|---|---|
| async runtime | `tokio` |
| filesystem watching | `notify`, `notify-debouncer-full` |
| scanning & ignore rules | `ignore` (wraps `walkdir`) |
| hashing | `blake3` |
| database | `rusqlite` (bundled feature) |
| serialisation | `serde`, `postcard` (wire), `serde_json` (state files, `--json`) |
| CLI | `clap` (derive), `console` or `owo-colors`, `indicatif` sparingly |
| HTTP to LocalAPI over a unix socket | `hyper` + `hyperlocal`, or a minimal hand-rolled HTTP/1.1 client |
| paths | `directories` |
| errors & logging | `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `tracing-appender` |
| file metadata | `filetime`, `tempfile` |
| NFC normalisation of paths (binary crate only) | `unicode-normalization` |
| ids & randomness | `rand` |
| testing | `proptest`, `insta`, `cargo-fuzz` |

## Appendix B — Repository layout

```
delocal/
  Cargo.toml                workspace
  DESIGN.md                 this file
  README.md                 install, the not-a-backup warning, five commands
  TESTING.md                acceptance checklist
  install.sh                served at delocal.sh/install
  crates/
    engine/                 pure sync logic: index, versions, batches, conflicts, brake
    sim/                    deterministic simulator: in-memory host for N engines, seeded PRNG, invariants I1–I6; dev-only, runnable for the nightly
    delocal/                the binary: daemon, cli, fs, sqlite, net, tailscale, service install
  packaging/
    systemd/delocal.service
    launchd/sh.delocal.plist
```

The engine crate has no dependency on `tokio`, `notify`, `rusqlite`, `rand`, or anything that touches the world, and its source never uses `std::fs`, `std::net`, `std::time::SystemTime` or `Instant`. It may depend on `serde` (with `derive`) so that its value types serialise directly, and on `proptest` for tests. If it needs anything else, the boundary is in the wrong place.
