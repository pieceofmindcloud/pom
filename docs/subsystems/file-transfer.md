# Subsystem: file transfer between machines

This subsystem moves GGUF slices between machines in the cluster, entirely in
`crates/node/src/backend/transfer.rs`. There are two directions that end in
the same download: **pull** (this node asks a peer for a slice) and **offer**
(the machine's owner picks a file, the peer accepts and then pulls). The bytes
travel over the peers' existing HTTP gateway, with `Range` for resume - not
over libp2p bitswap, which left the swarm because it was an orphan protocol
(see `p2p-network.md`).

Where the file lands depends on what it is. A **part** of a slicing goes to
`<shard_dir>/<model>-<short hash of the original>/<sha256>.gguf`, with the
manifest `<model>-<short hash of the original>.pom` written in the same folder
before the bytes, and is checked against the hash that was requested. A file
that is not part of any manifest - a whole model - lands in
`<shard_dir>/recebidos/` ("received") and only takes its final name when the
MD5 matches.

When receiving a v1 manifest, the peer merges by `pom_id`: local parts are
kept and missing remote parts are added. This applies to both `parts` and
`tp_parts`; the local manifest is never replaced by the sender's partial view.

**The `.pom` is mandatory for a part, in both directions.** The sender refuses
to offer a slice without a manifest (`send_to_peer`), the server refuses to
deliver it (`servable_path`), and the receiver refuses to accept it
(`alvo_da_oferta`, `spawn_download`). There is no alternative path: an orphan
slice is a file nobody knows how to name, and the hash of the original model -
which decides the folder it lives in - only exists in the manifest.

## 1. The two directions, and the rule of who chose the file

What may leave a machine depends on **who chose the file**:

- **Pull** (`POST /api/transfer/pull`): this node asks a peer for a path nobody
  on the other side chose. So the path is confined to `shard_dir` (`models/` by
  default) through `safe_model_path`, which canonicalizes and requires
  `starts_with` on the root directory. The request carries the path, because it
  is a catalog file.
- **Offer** (`POST /api/transfer/send`): the screen only shows `.gguf` files
  present in the local catalog and sends the basename in `filename`. The
  backend rescans the configured folders, requires a single match and only then
  resolves the real path. Absolute paths, `..`, duplicate names, missing files
  and extensions other than `.gguf` are refused. The peer never sees that path:
  it only gets an opaque id (see grant below) and fetches with `?grant=<id>`.

Both directions go through `ensure_gguf`, which requires the `GGUF` signature in
the first 4 bytes **and** the `.gguf` extension. Extension-less blobs from the
Ollama store cannot be transferred through this screen.

## 2. The grant

`TransferState::grants` is an `id -> PathBuf` map created **only** by
`send_to_peer`, which runs from this machine's screen. The offer body carries
the `grant` and the receiver returns it to `/api/transfer/manifest` and
`/api/transfer/download` through `?grant=<id>`. That is what lets a file from
outside `models/` leave without opening the disk to whoever holds the key: the
path is always someone's local choice, never a remote request. The
`servable_path` guard also runs `ensure_gguf` on the grant path - it was the
only door without a type check.

## 3. The download: `.part`, `Range`, MD5

Accepting an offer (`POST /api/transfer/offers/:id/accept`) **returns before
the other side answers**. `spawn_download`:

1. Registers the job right away (`bytes_total` 0, indeterminate bar, detail
   "checking the file at the source"). The destination directory - the model
   folder for a part; `<shard_dir>/recebidos/` for a loose file - is the only
   thing that has to succeed immediately.
2. In a background task, `fetch_manifest` asks for size + MD5 at
   `/api/transfer/manifest` (`MANIFEST_TIMEOUT` = 30 min - a digest is a read
   of the whole file on a slow volume, and the deadline runs in the background)
   and runs the **disk space check** right there: `size_bytes` +
   `DISK_HEADROOM_MB` (512) against free space; it refuses before filling the
   disk and failing halfway.
3. `run_download` writes `<name>.part`, resumes from an existing `.part` with
   `Range: bytes=<already-downloaded>-`, reads/writes in `CHUNK` blocks (1 MB),
   updates progress every 500 ms (speed uses what **moved in this attempt**,
   not the total - a resume with bytes already on disk would inflate the
   number), switches the state to `verifying`, checks the MD5 and only then
   renames to the final name. A wrong MD5 deletes the `.part` and fails the
   job; a truncated file never passes as good. Cancelling midway deletes the
   `.part`.

The **sending** side also shows up in the transfer list
(`direction: "sending"`), because it is the one transmitting the bytes and
already knows the progress (`served_stream` counts what went out; ending the
body before the promised `Content-Length` is how it honors a cancellation). The
same id exists on both machines with opposite directions - that is how the two
sides talk about the same transfer. Job states: `running`, `verifying`,
`done`, `failed`, `cancelled`; the `TransferJob` fields are `id`, `name`,
`direction`, `peer_host` (replaces `from_host`, which lied in the send
direction), `path`, `state`, `bytes_done`, `bytes_total`, `bytes_per_sec`,
`eta_secs`, `detail`, `finished_at`, `model_label`, `start_layer` and
`end_layer`.

**A slice presents itself by model and layers, never by hash.** A part's
`name` is its sha256, and a hash says neither which model it belongs to nor
which piece it carries - the notification used to show
`df48d094c3d1023c0dab9e...` and the operator could decide nothing from that.
The `.pom` knows both, and it **arrives before any weight byte**: it travels
inside the offer (`TransferOffer::pom`) in the push direction, and is requested
from `/api/transfer/pom` before the download in the pull direction. In both
cases it is already written in the model folder when `spawn_download`
registers the job, and `slice_label_for_path` reads the label from there. On
the sending side, `download` does the same read next to the served file. The
hash stays on screen as a secondary identifier - it names the file on disk and
identifies the part between machines.

**Sending closes at the last byte, not at the end of the stream.** When the
body satisfies the promised `Content-Length`, hyper stops pulling and drops the
stream without ever asking for the `None` that would mark the end - counting
bytes is the only signal that always arrives. Without that close, the sender's
job stayed `running` forever, with `bytes_done` frozen at the last 500 ms
report (on a fast LAN, zero): the machine that sent the file showed in the
bell an endless transfer that had already finished and been verified on the
other side.

Every outcome goes through `TransferState::finish`, which records the state,
the reason and `finished_at`, and **refuses to rewrite a terminal state
already set**. That is what makes "cancelled by <peer>" - someone's decision -
survive the anonymous "failed" the loop reports right after seeing the stream
die. The timestamp is what lets the bell show what **just** finished: before
it, the screen only listed what was in progress, and completed/failed/refused
transfers vanished the moment they happened.

## 4. The offer carries no digest

`send_to_peer` posts **only the `stat`**: name + real `size_bytes`, and `md5`
always empty. The field still travels because an older peer requires it in the
request body, but nobody reads it. Hashing the whole file **before** the offer
was what held the notification on the other side for minutes: measured on the
operator's SD card, the `md5` of a 4.2 GB slice costs 3 min 23 s (~20 MB/s).
The digest cache by `(path, mtime, size)` only masked it, making the second
send of the same file instant - hence the "sometimes 1 or 2 minutes" in the
report. The receiver checks the download, with the MD5 it asks for at
`/api/transfer/manifest` after accepting.

## 5. Manifest and disk check inside the task

For the same reason, the manifest and the disk check run **inside** the
download task, not before the accept click. Asking the other side costs a full
read of the file on the source machine; doing it on accept would make the
click wait for minutes. In exchange, the operator gets the job in the list
right away, ending in `done` or `failed` with the reason where they would look
for it.

## 5.0 Which peer address is used

The offer, the pull request and the cancel notice do not use the peer's first
advertised address, which may be an IPv6 nobody reaches: they use
`Network::gateway_bases`, which puts first the host that already answered this
node (`RouteTable::last_success`, the same one the participants screen shows
as the direct route), then the ones that have not failed recently, and last
the discredited ones. The offer is a signed control call
(`GatewayState::control_post_json`): it tries each address in that order and,
last, the libp2p tunnel (`/api/transfer/offer` is on the tunnel's list). The
return address (`from_gateway`, through `local_ip_for`) comes from the first
of that list, so it points at the interface through which the peer is really
reached. The download is still direct HTTP in both directions: weights do not
go through the relay.

## 5.1 The dialer's scheme is the local one, not the offer's

The offer carries `from_gateway`, built by the **sender**. It used to write a
fixed `http://`: with the cluster on TLS, the acceptor dialed in clear text and
the other side's gateway answered **403 in plain text** - literally "this
cluster requires TLS between nodes; redo the call over https". `.json()` fell
on that sentence and the operator saw `manifesto inválido de <peer>: error
decoding response body` ("invalid manifest from <peer>"), which points at
nothing (08/18).

Two defenses, because both are needed:

- the sender builds the URL with `peer_client::peer_base_url(.., tls_ativo())`,
  not a literal;
- the acceptor passes the URL through `normaliza_esquema`, which enforces the
  **local policy** - an older node still offers `http://`, and the download
  works anyway.

And `fetch_manifest` checks the status **before** trying to read JSON,
repeating the other side's sentence. Gateway errors usually come as plain
text; reading JSON over them replaces the explanation of whoever knows with a
message that does not help.

## 5.2 The verdict goes back to the offerer

Knocking on the door and never knowing whether it opened was half of the field
report: whoever sent a slice got no notice that the other end had accepted or
refused. The offer already traveled with a `grant`, and it is the only id the
two ends share before a job exists - `send_to_peer` keeps what it offered in
`TransferState::offered` (`grant` -> destination, name, path, size) and the
receiver returns the verdict at `POST /api/transfer/offers/answer`
(`notify_offerer` -> `receive_answer`).

- **Accept** registers the `sending` job right away, with the same `job_id` the
  receiver minted, and the detail "accepted; waiting for the send to start".
  This matters because the gap between accept and the first byte is the time
  the receiver takes to ask for the digest - minutes on a slow volume - and it
  was pure silence on the offerer's screen.
- **Refuse** becomes a `cancelled` job with the name of whoever refused.
  Refusing is not failing: it was someone's decision, and the screen says
  whose.
- **Accept that could not start** (slice without `.pom`, full disk) also
  notifies, instead of leaving the sender waiting for a download that never
  comes.

The notice is best-effort and goes out in a task: the decider's click does not
wait for the other machine to answer, and a sender that is offline does not
make the refusal fail on the refuser's screen. A verdict whose `from_node_id`
is not the registered destination is refused with 403 - it describes another
conversation. An offer from a peer older than `grant` produces no notice:
there is no shared id to name it.

## 6. Symmetric cancellation

`POST /api/transfer/jobs/:id/cancel` is a single verb with the same semantics
on both ends: the local screen and the notice the other side sends use the same
route. The cancellation is marked in a `cancelled` set that survives even for
jobs that do not exist here yet (the other side's notice can arrive before the
download starts). Whoever cancels from their own screen notifies the peer
(`notify_peer_cancel`, with `from_peer: true` to avoid the echo); if the notice
is lost, the remote side falls into the dead-connection path and reports the
real reason. A job cancelled by someone is never overwritten by an anonymous
"failed".

## 7. Automatic accept - development only

`POM_DEV_AUTO_ACCEPT_TRANSFERS` makes the **receiver** accept every offer
without a click. It is a variable of the receiving process, read **once** at
startup (`OnceLock`), and off by default: absent, empty, `0` and `false` all
still require confirmation. The sender has no way to ask for it - the gate is
opened by the machine whose disk will be written. If an automatic accept
cannot start the download, the offer goes back to the shelf instead of
vanishing with an error only in the log. Do not turn it on for a machine whose
disk matters.

## 8. The three entry points on screen

The transport is the same; what changes is who chooses what:

- **Slices on this machine** (Models tab): lists **every** local slice, always,
  and the destination is a `select` with only the nodes that do not have it yet
  ("all already have it" when no one is missing it). Destination and
  availability label carry the start of the `node_id` when peers on the same
  machine repeat a hostname.
- **Slices across machines** (Nodes tab): a **difference** list - only what is
  missing on the other side. That explains "there is no option to send a slice"
  when the machines have the same slices: the panel is a difference list and
  no row is left to click.
- **Send a file from this machine** (Nodes tab): a combobox with name and full
  path for visual checking, sending only the safe basename.

Progress lives in the bell and comes from the nodes, so closing the tab
interrupts nothing.

## 9. Routes

All behind `ConnectionAuth` (connection key, see `http-api.md`):

| Route | Direction | Role |
|---|---|---|
| `GET /api/transfer/manifest` | serve | size + MD5 of the requested file (by `?path=` or `?grant=`) |
| `GET /api/transfer/download` | serve | stream with `Range`, resume and progress |
| `POST /api/transfer/pull` | pull | this node asks a peer for a slice (path in `models/`) |
| `POST /api/transfer/offer` | offer | the peer registers an offer of ours; nothing downloads until accepted |
| `GET /api/transfer/offers` | receive | lists the offers + `disk_free_mb` |
| `POST /api/transfer/offers/:id/accept` | receive | starts the download (returns before the other side answers) |
| `POST /api/transfer/offers/:id/reject` | receive | drops the offer and notifies the offerer |
| `POST /api/transfer/offers/answer` | offer | the other side's verdict on an offer of ours (`grant`) |
| `GET /api/transfer/jobs` | both | jobs in progress, sorted by name |
| `POST /api/transfer/jobs/:id/cancel` | both | cancels (idempotent, notifies the peer) |
| `POST /api/transfer/send` | offer | the local screen sends `{to_node_id, filename}` to a peer |

The `?job=` and `?peer=` locators are optional on purpose: an older peer
ignores them and the transfer works the same, just without the sending side's
view.

## 10. Tests

They use the `gateway` testkit (see `http-api.md`): `state_with` with the peer
table seeded by hand and an axum `fake_peer` on loopback that captures the
offer body. The offer latency tests prove that the offer goes out **without** a
digest (empty `md5`) and that `auto_accept` stays closed unless the variable is
explicitly set. There is also a security test: a `?path=` outside `models/` and
an invented `grant` are refused.

## 11. Defects noted

- **DEFECT NOTED: `cancelled` is never pruned** (`TransferState::cancelled`,
  `transfer.rs`) - cancelled ids enter the `HashSet` and never leave; the set
  grows without bound for the life of the process. Harmless in practice (a few
  bytes per id), but it is a memory leak.
- **DEFECT NOTED: `grants` never expire** (`TransferState::grants`,
  `transfer.rs`) - a `grant` created by `send_to_peer` lives until the process
  dies, even after the download finishes or the offer is refused. The error
  message talks about an "expired grant", but there is no deadline.
