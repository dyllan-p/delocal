# delocal — v1 Design

> Draft 1 · 22 September 2026 · Status: **for review**
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

### 3.3 Requirements on the user's machines

- Tailscale installed, logged in, and running
- All machines belong to the same Tailscale user (see §5)
- A user session that can run a background service (see §16 on `loginctl enable-linger` for headless boxes)

---

## 4. Vocabulary

| Term | Meaning |
|---|---|
| **Machine / node** | One installation of delocal. Identified by a **node ID**. |
| **Node ID** | Random 128-bit identifier generated on first `up`, stored in the state dir. Independent of Tailscale so it survives a Tailscale reinstall. |
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

This section is the core of the system and the part the simulator (§14) exists to prove. The engine that implements it is pure: it takes events (scan results, incoming messages, timer ticks) and returns actions (write file, send message, record history). It does no I/O itself.

### 7.1 The index

Per folder, one record per entry the machine knows about, including deleted ones:

| Field | Notes |
|---|---|
| `path` | Relative, forward slashes, NFC-normalised, no leading `./` |
| `kind` | `file` / `dir` / `symlink` |
| `size` | bytes (0 for dir; target length for symlink) |
| `mtime_ns` | as observed or received |
| `exec` | bool; the only permission bit synced |
| `hash` | BLAKE3 of content (files), of the target string (symlinks), empty (dirs) |
| `version` | version vector (§7.2) |
| `deleted` | bool — this record is a tombstone |
| `modified_by` | node ID that produced this version |
| `seq` | this machine's per-folder sequence number when this record was last written locally |

Plus, per folder per remote member: the highest `seq` of theirs we have received, used for catch-up (§7.4).

### 7.2 Versions

```
Version = BTreeMap<NodeId, u64>
```

- **Local change to an entry:** take the entry's current version, set `v[self] = max(v[self], self_counter) + 1`. Deletion is a local change like any other; it produces a tombstone with a new version.
- **Comparison** of `a` and `b`, treating missing keys as 0:
  - `a == b` → **equal**
  - every `a[k] ≥ b[k]` and not equal → **a dominates**
  - every `b[k] ≥ a[k]` and not equal → **b dominates**
  - otherwise → **concurrent** (a conflict, unless the content hashes are equal — see §7.6)
- **Merge** (used when concurrent versions have identical content): component-wise maximum, no increment. Two machines merging the same pair independently produce the same result, so they converge without talking.

Wall-clock time never participates in ordering. See §7.8.

### 7.3 Local change detection

- `notify` watches every folder root recursively. Events are debounced (2 s of quiet per path).
- A **full scan** runs at daemon start, every hour **[decision]**, and on `delocal scan`. Watchers drop events under load; the scan is the ground truth.
- Fast path: an entry whose `size` and `mtime_ns` match the index is unchanged. Anything else is hashed.
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
  entries: [ { path, kind, size, mtime_ns, exec, hash, version, deleted } ],
  summary: { adds, mods, dels, bytes }
}
```

Before sending, the sender runs the **brake pre-check** (§8.1) against its own batch. If it trips, the folder is **paused**: the batch is kept locally, nothing is sent, `status` says so, and the user resolves it with `approve` or `revert` (§8.3).

Otherwise the batch is sent to every connected member of the folder and recorded in history.

**Receiving a batch.** For each entry, compare its version with the local record (absent = empty version, dominated by everything):

- incoming **dominates** → candidate to apply
- incoming **dominated** or **equal** → ignore (we already have it or newer)
- **concurrent** → conflict handling (§7.6) produces zero or more candidate actions

The set of candidates is the batch's **apply set**. The brake (§8.1) is evaluated over the apply set. Result: `accepted` (apply set goes into the want-list) or `held` (versions quarantined, §8.2). The receiver replies `BatchDecision { id, decision }`, and records the batch in history either way.

**Catch-up.** When two members connect, each sends every index record with `seq` greater than the last `seq` the other has acknowledged, packaged as one or more synthetic batches (max 10,000 entries each). These go through exactly the same brake. A brand-new member receiving the whole folder sees a batch of pure adds, which the count rule ignores (§8.1), so first sync is never held by count; it can still be held by size.

**Why batches and not a live index stream:** every batch is a natural unit for approval, for `history`, for `review` and for `revert`. Syncthing streams index updates continuously and has no such unit, which is why its safety story is weaker. The cost is up to 10 s of latency on a change, which is acceptable.

### 7.5 Applying changes

**Want-list.** Accepted entries go into a per-folder want-list: `(path, version, hash, size, sources)`. `sources` is every connected member whose index says it has that exact version; any of them may be asked. A want-list item is deferred if it exceeds the tier rule (§6.5) for every currently available source.

**Fetching a file.**

1. Pick a source (prefer `lan`, then `direct`, then `relay`).
2. Send `RequestFile { folder, path, version, offset }`. The `offset` is the size of any partial temp file from an earlier attempt, so transfers resume.
3. Data arrives as `FileData` chunks (1 MiB) into `.delocal/tmp/<hash-prefix>-<random>`. The source refuses (`NotAvailable`) if it no longer has that exact version; the receiver removes that source and tries another.
4. On completion, verify BLAKE3 against the expected hash. Mismatch → discard, retry once from a different source, then give up on that item until the index changes.
5. Set mtime and exec bit on the temp file.

**Committing.**

6. Check the target path is still what the index said it was when the decision was made (same `size` and `mtime_ns`, or absent). If not, the local file changed underneath us: abort the commit and treat the situation as a conflict on the next scan (§7.6).
7. If a file exists at the target, **move it to trash** (§8.4). Same filesystem, so this is a rename.
8. `rename(tmp, target)`. Ensure parent directories exist (creating them as index entries if they arrived in the same batch).
9. Update the index record and `fsync` the parent directory.

**Deletes.** Move the current file to trash, write the tombstone to the index. Directories are removed only when empty and only after all children in the batch have been processed. Order: creates process parents before children; deletes process children before parents.

**Symlinks.** Written with `symlink(target, tmp)` then rename. Never followed.

**Crash safety.** The index is updated only after the rename. Temp files are content-addressed enough to resume; anything in `tmp/` that no want-list item claims is removed at daemon start.

### 7.6 Conflicts

Two versions of the same path are in conflict when they are **concurrent** (§7.2) **and** their hashes differ (kind, hash, and for files exec bit; mtime is not compared). Concurrent versions with identical content are not a conflict: both sides merge vectors and move on. This rule matters because it is how independently-made identical changes, and the conflict-copy mechanism itself, converge without producing duplicates.

**Deterministic winner.** Every machine must pick the same winner without communicating:

1. The version with the larger `mtime_ns` wins.
2. Tie: the version whose `modified_by` node ID is lexically larger wins.

**Actions.** Let `W` be the winning version and `L` the losing one.

- A machine that currently **holds L** locally renames its file to the conflict name (below) as a normal local change (producing a fresh version for the new path), then accepts `W` for the original path.
- A machine that currently **holds W** locally produces `W' = merge(W, L)` with its own counter incremented. `W'` dominates both `W` and `L`, so every machine converges on `W`'s content once `W'` propagates. If several machines hold `W` and each does this, their `W'` versions are concurrent with identical content and merge under the identical-content rule.
- A machine that holds **neither** (it has an older version or nothing) does nothing special: it will receive `W'` and the conflict copy as ordinary changes.

**Conflict copy name** is deterministic so that several machines renaming `L` independently produce the same path and the same content, which then merge:

```
<stem>.conflict-<L mtime as UTC YYYYMMDD-HHMMSS>-<hostname of L.modified_by><ext>
report.conflict-20260922-143005-laptop.xlsx
```

**Special cases.**

- **Delete vs modify:** the modification wins regardless of mtime. The deletion is dropped. A file that someone is still editing should not vanish.
- **Modify vs modify on a directory** cannot happen; directories carry no content.
- **File vs directory at the same path:** the newer (by the winner rule) wins; the loser is renamed with the conflict suffix. Expected to be vanishingly rare.
- **Case-insensitive filesystems (macOS default):** two index paths that differ only by case cannot both exist. Neither is applied; `status` reports the pair and the user resolves it on a case-sensitive machine. **[decision]** This is the Syncthing approach and is good enough for v1.

### 7.7 Tombstones

A deleted entry keeps its index record with `deleted = true` and the deletion's version. Tombstones are announced, stored, and compared like any other version. A machine that was offline and still has the file will, on catch-up, receive a tombstone that dominates its version and delete (to trash) locally; it will not resurrect the file. A machine that **modified** the file while offline has a concurrent version → delete-vs-modify → the file survives everywhere.

**[decision] Retention:** tombstones are kept indefinitely in v1. Each is a few hundred bytes; a folder with a million historical deletions would carry a few hundred megabytes of index, which is tolerable for v1. Pruning (for example, once every member has acknowledged a tombstone's `seq`) is a v1.1 item and must never be done unilaterally.

### 7.8 Clocks

Wall-clock time is used for exactly three things: local change detection (mtime vs index), the conflict tie-break, and preserving mtimes on received files. It never orders events; version vectors do. If a peer's `Hello` timestamp differs from local time by more than 5 minutes, `status` shows a warning, because the tie-break is less trustworthy.

---

## 8. Safety net

### 8.1 The brake

A batch is **held** if either rule trips, evaluated over the batch's apply set (receiver) or the batch itself (sender pre-check):

| Rule | Default |
|---|---|
| **H1 · count** | `dels + mods ≥ 50` **and** `dels + mods ≥ 25%` of the folder's tracked (non-deleted) entries before the batch |
| **H2 · size** | total bytes of adds + mods `> 20 GiB` **[decision]** |

- Adds do not count toward H1. Adds are almost never destructive, and exempting them keeps first sync and bulk imports quiet.
- H1 cannot trip on a folder with zero tracked entries.
- Thresholds are per folder: `delocal rules ~/Sync --hold-count 50 --hold-pct 25 --hold-size 20G`. Setting `--hold-count 0` disables H1 for that folder.

**Sender pre-check.** The same rules run on the machine where the changes happened, before anything is sent. If they trip, the folder is **paused** on that machine: nothing leaves, and `status` everywhere shows `paused on <machine>: 812 deletes pending — delocal review on <machine>`. This is cheap and it is what makes `revert` trivially safe: no other machine has seen the damage.

**Receiver check.** Runs regardless of what the sender did. A machine running an old or broken delocal, or one whose user approved something hastily, still cannot push a mass change onto a machine that has not agreed.

Counting only. No content analysis, no entropy heuristics, no attempt to recognise ransomware. This catches `rm -rf` in the wrong terminal, a bad script, an unmounted disk, and encryption-style damage (which always looks like mass modification or delete-plus-add) with one simple, testable rule.

### 8.2 Quarantine

When a receiver holds a batch, every version in its apply set is written to the `quarantine` table. Quarantine is by **version**, not by sender:

- The same version offered later by any other member is still held.
- Any version that **dominates** a quarantined version (the source kept editing after the event) is also quarantined, and joins the same review item.
- Unrelated paths from the same source are unaffected; sync continues for them.

`delocal review` shows each held item: source, time, counts, size, sample paths, file-type breakdown. Then:

- **`approve`** applies the apply set normally. Trash still protects every overwritten or deleted file.
- **`deny`** makes this machine's current copies win: for every quarantined path, it bumps its own version (a local change with unchanged content), which dominates the quarantined version, and drops the quarantine. The mesh converges on this machine's copies. On the source machine this arrives as a mass modification and may itself trip that machine's brake; that is correct — the user is already in "something went wrong" mode and approving it there restores the source. **[decision]** This is the simplest correct semantics for `deny`; an alternative is for `deny` to only refuse and tell the user to run `revert` on the source.

Approving on one machine does not approve on others in v1 (§3.2).

### 8.3 Revert (on a paused sender)

`delocal revert ~/Sync` on the machine that paused itself means "make this folder look like the rest of the mesh again":

1. Discard the pending batch.
2. For every path in it: move the current local file (if any) to trash, and reset the index record to the last version that was actually announced.
3. Adds that were part of the pending batch are also moved to trash **[decision]** — in the destructive scenarios (encryption, a script writing junk) they are the debris.
4. The normal want-list logic re-fetches every reverted path from peers, which still have them because nothing was sent.

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
folders        (id, name, created_by, rules_json, meta_version)
members        (folder, node, path, mode, joined_at)
entries        (folder, path, kind, size, mtime_ns, exec, hash, version_blob, deleted, modified_by, seq,
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

---

## 12. Protocol messages

```
Hello         { proto: u32, node: NodeId, hostname, version: String, now: i64 }
Goodbye       { reason }
FolderMeta    { folder, name, members, rules, meta_version }
Batch         { …§7.4 }
BatchDecision { batch: BatchId, decision: Accepted | Held { reason } }
RequestFile   { folder, path, version, offset: u64 }
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
| Crash between rename and index write | Next scan sees a "new local change" with content matching a known version → merges by identical-content rule. |
| Peer offers a version it no longer has | `NotAvailable`; try another source; otherwise wait. |
| Protocol mismatch | Refuse politely; `status` shows who needs `delocal update`. |
| Case collision on macOS | Neither applied; `status` names the pair. |
| Two machines with the same node ID (restored backup) | Binding check (§5) refuses the second; `status` warns. |
| Clock skew > 5 min | `status` warning. |

---

## 14. Testing strategy

Sync tools earn trust with tests, not features. Testing is Phase 1, not Phase 5.

### 14.1 The simulator (in `crates/engine`)

The engine is pure, so it can be driven by an in-memory filesystem and an in-memory network with a seeded PRNG. Each run creates N nodes (2–8), one or more folders, and then applies thousands of random steps:

- file create / modify / delete / rename, on random nodes, including the same path on several nodes in the same step
- network partitions and heals, node offline and returning after arbitrary time
- node crash: drop all in-flight transfers and un-fsynced state, restart
- message delay and reordering
- clock skew per node
- mass-delete and mass-modify events (to exercise the brake)

Invariants checked at the end of every run and at random quiescent points:

| # | Invariant |
|---|---|
| I1 | **Convergence.** When all nodes are connected and quiescent, every member has the identical set of paths, kinds, hashes and exec bits (excluding `.delocal/`). |
| I2 | **No loss.** Every distinct content hash that ever existed in any node's folder exists at the end in some node's folder or trash. |
| I3 | **No resurrection.** A path deleted on a connected node and not concurrently modified is absent on every node after convergence. |
| I4 | **Bounded conflicts.** Each concurrent-edit event produces at most one conflict copy per losing version, never per node. |
| I5 | **Brake.** No batch that trips H1/H2 is ever applied without an explicit approve step in the simulation. |
| I6 | **Determinism.** Same seed → byte-identical outcome. |

Run with `proptest` for shrinking. CI runs 1,000 seeds per push; nightly runs 100,000.

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
    engine/                 pure sync logic: index, versions, batches, conflicts, brake; the simulator
    delocal/                the binary: daemon, cli, fs, sqlite, net, tailscale, service install
  packaging/
    systemd/delocal.service
    launchd/sh.delocal.plist
```

The engine crate has no dependency on `tokio`, `notify`, `rusqlite` or anything that touches the world. If it needs to, the boundary is in the wrong place.
