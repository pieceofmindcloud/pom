# Three-machine network: the button that would not enable, the gossipsub that did not talk, and the tunnel nobody had proven

Date: 2026-08-16. Branch `fm/rdma-rede-3-maquinas`, on top of `f8eca5f`.

Note: this report predates the product rename; `rdma-p2p` was the binary's
old name and `RDMA_P2P_*` the old environment prefix (today derived from
`brand.json`, for example `POM_HOME`).

Environment: the operator's **real** cluster, live the whole time - a laptop
(LAN), `server-a` (LAN, Proxmox VM) and `server-b-gpu`/`server-b-cpu` (the same
physical machine on the other side of the internet, two node identities).
Twelve nodes in total, counting the extra identities (`server-a-2..5`,
`server-b-gpu2`, `server-b-cpu2`). Machine and node names in this report are
generic placeholders.

Nothing was changed on the operator's machines. Every measurement was made with
**our own probe nodes** (`probe-fm`, `probe-fm-2`), started with `RDMA_P2P_HOME`
in this session's temporary directory, with no model and without touching the
cluster's slices - which made it possible to compare binary `f8eca5f` and the
fixed binary on the same cluster, with the same node identity, in the same
minute.

---

## Defect 1 - the deploy button does not enable and the screen does not say why

### Reproduction, on the operator's machine

On the operator's node screen (`http://localhost:8080/implantar`, binary and UI
`f8eca5f`), with the set `lmstudio-community-Qwen3.6-35B-A3B-GGUF`:

1. turn on "Merge neighboring shards on the same machine";
2. give the same machine (`server-b-cpu`) to L0-7 and L16-23.

Result measured in the DOM: `Deploy this way` with `disabled: true`, and **no
explanation text anywhere in `<main>`** - the only texts matching "cannot /
blocked / não / vizinhas" are hints of other controls.

The same state on the node with the fix (`http://localhost:8099/implantar`):

```
This selection cannot be deployed
as fatias de server-a-2 não são vizinhas (L0-7 e L16-23) - só dá para juntar fatias adjacentes
[next to the button] see the reason above
```

(The refusal text came from the node in Portuguese: "the slices of server-a-2 are
not neighbors (L0-7 and L16-23) - only adjacent slices can be merged".)

### Root cause - two defects, one on each side

**(a) The node returned `steps` out of layer order.** `deploy::plan` grouped
the chosen ranges **by target machine** and only then emitted the steps. A
machine with two ranges emitted the second right after the first, ahead of
everything in between. Measured against the real cluster, with the five-range
selection and `server-a` getting L0-7 and L16-23:

```
$ curl -s -H "Authorization: Bearer $KEY" -d @sel2.json localhost:8080/api/ui/deploy/plan
blocked: server-a ficou com duas faixas (L0-7 e L16-23) - junte as fatias vizinhas...
STEPS (order returned):
  1  0 - 7  -> server-a
  2 16 - 23 -> server-a        <-- out of order
  3  8 - 15 -> server-b-cpu2
  4 24 - 31 -> laptop.local
  5 32 - 39 -> server-a-3
```

There was a second, worse case, because it hit the automatic plan: an empty
`target_node_id` grouped by string equality like any other, so **every range
without a machine became "the same machine"**. With two ranges without a
target the order broke, and the refusal named a machine with no name
(`" ficou com duas faixas (L8-15 e L24-31)"`, with the space where the name
should be).

**(b) The screen went silent exactly because of that.** In
`apps/frontend/src/routes/DeployModel.tsx`:

```ts
const rows = (selection ?? rowsOf(p)).slice().sort(byLayer);   // sorted by layer
const validated = !!p && sameRows(rows, rowsOf(p));            // compared with the RAW plan
const canDeploy = validated && !p?.blocked && rows.length > 0;
```

`sameRows` compared position by position. The screen rows were sorted by
layer; `rowsOf(p)` came out in plan order. They diverged, `validated` = false -
and the four explanation panels (refusal, incomplete coverage, deducted total,
memory warnings) **also rendered under `validated`**. So in the only state in
which the operator needs the reason, the screen hid it and left just the grey
button.

Note that `validated` was false even **without the operator editing
anything**: with `selection === null`, `rows = rowsOf(p)` **sorted** and the
comparison was against the raw list. An out-of-order automatic plan - which is
what happens when the planner runs out of machines and more than one range is
left without a target - silenced the screen on its own, and the suppressed
reason was precisely "there are N slices for M machines - a node serves one
range at a time".

### Fix

Node side (`crates/node/src/backend/deploy.rs`):

- `steps` is sorted by `(start_layer, end_layer)` once at the end, and
  `position` is numbered **after** that. This is not cosmetic: `position`
  means "place in the pipeline", pipeline order is layer order, and `run`
  reads the launch order straight from this list (`LaunchTarget` with
  `peer_index: None` derives the index from the position in the vector).
- a range with an empty `target_node_id` never groups with another - each one
  is its own step with its own `problem` ("slice L{}-{} has no machine").

Screen side (`DeployModel.tsx`):

- `sameRows` compares both sides **sorted by layer**;
- the explanation panels now render under `answered` (the node answered about
  *these* rows: same group, no `isPlaceholderData`, no edit in flight), never
  under `validated`;
- `whyNot` puts the reason in one line next to the button, also covering the
  two states that have no panel at all: nothing selected and answer in flight.

### Regression tests

Rust (`deploy.rs`), all three failed before:

- `steps_stay_in_layer_order_when_a_machine_takes_two_ranges`
- `ranges_left_without_a_machine_are_not_one_machine`
- `two_hand_picked_ranges_without_a_machine_keep_their_own_rows`

Frontend (`DeployModel.test.tsx`), all three failed before:

- explains the refusal even when the node returns steps out of layer order
- enables the button when the plan describes the same rows in another order
- names each range without a machine instead of going silent

Checked that the three frontend tests fail without the fix (`git checkout` of
`DeployModel.tsx`, `npm test`: 3 failed) and pass with it.

### What this means for the operator

With the fix, deploying the three machines is possible again **and** the path
is visible: this cluster's automatic plan already closes 0-39 on its own (five
ranges on five nodes, `blocked: None`, `complete: true`); when the operator's choice does
not close, the screen now says which rule was broken and on which machine.

---

## Defect 2 - gossipsub stopped carrying announcements (and it was not a repeated payload)

### Reproduction

Probe node with binary `f8eca5f`, joining the real cluster:

```
2026-08-16T13:16:25 WARN Not publishing a message that has already been published message_id=31355230
2026-08-16T13:16:30 WARN Not publishing a message that has already been published message_id=31355230
... every 5 s, always the same id
```

59 warnings in a 296 s window - that is, **every publish after the first** was
refused, and waiting for the cache's 60 s TTL never helped.

### Root cause

It is not the payload. `Announce` carries `timestamp: Utc::now()`
(`Network::self_announce`), so two consecutive announcements **are** different
in bytes. What is constant is the **id**.

`Discovery::new` builds gossipsub with `MessageAuthenticity::Anonymous` and
does not set `message_id_fn`. The libp2p default is `source + sequence_number`
(`libp2p-gossipsub-0.49.4/src/config.rs:518`) and both are `None` under
anonymous authenticity, so the id is the constant `"15R0"` - `31355230` in
hex, exactly the id in the operator's log.

The damage is on both sides of the same cache (`duplicate_cache`, 60 s):

- **publish**: `Behaviour::publish` refuses with `PublishError::Duplicate`;
- **receive**: `handle_received_message` drops it as a duplicate - and since
  the id does not depend on content, *any* peer's announcement is a
  "duplicate" of any other.

And since every node in the cluster keeps reinserting the same constant, the
entry never actually expires.

### Real, measured impact

Probe node, real cluster, mesh formed (5 grafted peers, subscribed topic),
`RUST_LOG=libp2p_gossipsub=debug`:

| binary | window | messages accepted | dropped as duplicate | publishes refused |
|---|---|---|---|---|
| `f8eca5f` | 377 s | **0** | 0 | 75 |
| fixed | 2223 s | **444** | 39 | **0** |

444 messages in 2223 s = 444 ticks of 5 s: exactly one announcement per tick
from the other fixed node. The 39 dropped ones show that deduplication still
does its real job (the same message arriving over two mesh paths).

Zero messages in 377 s with a formed mesh proves that gossipsub announcement
traffic **was completely stopped across the whole cluster**. The cluster only
worked because the UDP path is independent.

### Fix

`discovery::announce_message_id`: id = hash of topic + payload. It is a
**local** setting (`message_id_fn` does not travel in the protocol), so an
updated node interoperates with one still running the default - which is
exactly what made it possible to measure before and after on the same
cluster.

Tests: `the_libp2p_default_id_collapses_every_announce_into_one` (reproduces
the id `31355230` at the exact spot),
`two_different_announces_get_two_different_ids`,
`the_same_announce_keeps_one_id`.

---

## Defect 3 - verifying what `f8eca5f` declared unverified

### 3.1 Advertised address

**Precedence checked live, without restarting anything**, by reading what the
*other* machines announce about the probe node (`PeerRequest` over UDP
against the operator's laptop and against server-a):

| step | what the node's screen says | what the laptop and server-a see |
|---|---|---|
| nothing pinned | `[<observed-ipv6>]:42799` (observed) | the same, on both |
| pinned in the Network screen | `[<pinned-ipv6>]:42799` | the same within ~10 s, on both |
| field cleared | back to observed | back to observed, on both |

Precedence #3 (local interfaces) shows up in `addr_candidates`
(`<lan-ip>:42799`, `100.x.y.z:42799`, ...), which is where it
lives.

**A real defect found here.** The first `identify` answer the node got came
from a peer reached over the **tailnet**, and the node adopted it as its
*public* address:

```
INFO 📡 Endereço público observado por um peer: 100.x.y.z:42799
```

`100.64.0.0/10` is the RFC 6598 range (CGNAT) - the one Tailscale uses - and
`Ipv4Addr::is_private` only answers RFC 1918, so `addrs::is_private_or_local`
let it through. A tailnet address proves the peer is on the same tailnet, just
as `192.168.` proves it is on the same LAN: nobody outside can dial either.
Fixed together with the other ranges the check treated as single addresses
instead of ranges (all of `127/8`, `169.254/16`, `0.0.0.0/8`, multicast,
broadcast, all of `fe80::/10`, `::`). After the fix, on the same cluster, only
global IPv6 addresses show up as observed.

A second finding, noted and **not** fixed because it is a product decision:
`set_observed_addr` keeps what the **last** `identify` said, with no
tie-breaking rule. On a Mac with several privacy IPv6 addresses, different
peers observe different addresses and the advertised one flips - seen on the
screen itself, showing `<ipv6-a>` and `<ipv6-b>` on
consecutive reads. `filter_candidates` prefers stable over temporary in local
enumeration; the observed path has no such preference. Whoever needs
stability pins the address, and that works.

Third finding, fixed because it is cheap and the datagram has an MTU budget:
the pinned address went out **duplicated** in `addr_candidates`, because
deduplication compared the bracketed form (`effective_host`) with the raw form
of the enumeration.

### 3.2 Tunnel fallback

**It fires and works against `server-b-gpu`, over the internet.** Two `#[ignore]`
probes, driven by `RDMA_P2P_TUNNEL_PROBE`, stay in the repository as a
reproducible procedure:

`infer_tunnel::tests::tunnel_reaches_a_live_peer` - raw protocol:

```
ConnectionEstablished peer=12D3KooWRuq6…(server-b-gpu) established_in: 68.358ms
túnel respondeu: status=400 body="quadro inválido: quadro curto demais: 0 bytes"
```

(The tunnel answered 400 "invalid frame: frame too short: 0 bytes".) The 400
is produced **by server-b's inference server** (it is not the synthetic 502 of
`serve_loopback`), so the frame crossed the internet, entered the tunnel, was
forwarded to `127.0.0.1:8181` over there and the answer came back.

Repeated against `server-b-cpu` (same physical machine, same IPv6, different
port): `established_in: 73.673ms`, same answer. **The two identities are not
confused** - each `PeerId` reaches the inference server on its own port.

`relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer` - the whole
transport switch, through production code, with direct HTTP pointed at
`192.0.2.1` (TEST-NET-1, RFC 5737, never routable):

```
first leg (direct HTTP): [Timeout] in 75 s
the peer refused over the tunnel (which already proves the path): 415 Expected request with `Content-Type: application/json`
```

**It does NOT fire on a healthy network.** Real distributed inference across
the three machines, with the fixed probe node acting as gateway, and zero
occurrences of the "trying via libp2p tunnel" / "libp2p tunnel also failed"
log lines.

Two honest caveats:

1. The machines reach each other over **global IPv6**. What was exercised is
   "over the internet", not "behind a blocking NAT" - on this cluster's IPv6
   there is no NAT to traverse. Breaking direct HTTP against server-b would mean
   touching it or the routing table of the operator's Mac, and the brief
   forbids the former; so the failure was induced with a black-hole address,
   which is the same failure mode (`Timeout`) through the same trigger.
2. **The detour is expensive to start**: 75 s measured on macOS, because
   `relay::send_frame` sets 600 s per request and overrides the client
   deadline, leaving the system `connect` in charge. The tunnel itself is
   fast. This is a product decision about the direct HTTP deadline, so it was
   noted and not changed.

### 3.3 Non-regression

With the fixed binary acting as gateway, through the two pairs the operator
has live:

| pair | `source` | tokens | tok/s |
|---|---|---|---|
| Ornith 0-19 (server-b-gpu) → 20-39 (server-a) | `pipeline:02eb8c40→5b035803` | 46 | **19.6** |
| qwen 0-15 (laptop) → 16-31 (server-b-cpu) | `pipeline:7d00220e→ff952586` | 44 | 6.6 |

The operator's control pair measured 17.9 tok/s; 19.6 tok/s is in the same
range. Both chains close with two hops, which is the proof of joint execution.

---

## Defect 4 (found during the work) - a short id on the wire kills `receive_loop`

Not in the brief: it showed up because it **took down the operator's cluster
during this session**, and the cause is in the operator's node log
(the laptop node's log file):

```
thread 'tokio-rt-worker' panicked at crates/node/src/network/network.rs:1024:33:
byte index 12 is out of bounds of `eu`
2026-08-16T14:05:18 INFO 🧹 Removed 10 timed-out peers
```

`&node_id[..12]` in the `PeerList` handling slices by byte index an id that
came **from the network**. A two-character id panics, the `receive_loop` task
dies, and the node **stops receiving UDP forever**. What is left is the worst
possible state to diagnose: HTTP answers, gossip keeps announcing, the TUI
shows the node ready with the model loaded, and the only clue is the socket
with a full receive queue (measured: `Recv-Q` 786,835 bytes on the laptop,
212,752 on server-a, frozen at the ceiling).

Where the id `"eu"` came from: it is a test fixture. `cargo test -p node` runs
tests that call `Network::announce` on a node that is **not** detached, and
`broadcast_local` sends to `255.255.255.255:42777` **whatever the node's port
is** - a fact that `a_detached_node_neither_announces_nor_looks_for_bootstrap`
itself documents. Running the suite on a machine in the same LAN as a live
cluster therefore triggers the panic on **every node on port 42777 of that
LAN**.

The correlation matches exactly: the laptop node (42777) and the main server-a
node (42777) went silent - the only two that receive LAN broadcast.
`server-a-2..5` (ports 42778-42781) and the two server-b nodes (remote) stayed
healthy, with `/peers` and UDP answering normally.

**Fixed**: `network::short()` cuts with `len().min(12)` respecting character
boundaries, and is used at the four spots that sliced the id. Test:
`a_short_node_id_from_the_wire_never_panics`.

**Not fixed** (see Pending): the `receive_loop` task has no panic isolation at
all, so the *next* panic on any message repeats the same silent outcome; and
the test suite can still broadcast on the real LAN.

## Test status

- `cargo test -p node`: **436 passed, 0 failed, 5 ignored** (the two new
  probes are among the ignored, along with the three that already existed).
- `npm test` in `apps/frontend`: **88 passed, 20 files**.
- `cargo clippy -p node --all-targets`: 29 warnings, against 30 on `f8eca5f` -
  the changes add none.

## Pending

0. **ACTION NEEDED NOW: the laptop node and the main server-a node have a dead
   `receive_loop`** from the panic above and must be **restarted** - only that
   restores UDP reception, and without them the cluster has no pipeline
   (`/peers` on each shows only itself). I did not restart them because they
   are the operator's processes, started in the operator's terminals:
   - laptop: `./target/release/rdma-p2p --refresh 3600 --advertise-addr '[<pinned-ipv6>]:42777'`, run from the repository root
     - the slice comes back on its own: `rdma-p2p.json.last-slice.json` is in
       `serving`.
   - server-a: **there is no `last-slice.json`** in its project
     root, so its Ornith 20-39 slice does
     **not** come back on its own and the pair must be redeployed from the
     screen.
   The other nodes (server-b-gpu, server-b-cpu, server-a-2..5) are intact.
1. **The test suite broadcasts on the real LAN.** `broadcast_local` goes to
   `255.255.255.255:42777` whatever the port, and some tests call `announce`
   on a node that is not detached. Running `cargo test` next to a live cluster
   injects test messages into it. The panic fix removes the catastrophic
   effect, not the broadcast. It deserves a guard in the harness (test node
   always detached, or an injectable broadcast address).
2. **`receive_loop` has no panic isolation.** A panic in `handle_message`
   kills UDP reception for the whole process, silently, and nothing in the
   TUI, `/health` or `/api/ui/node` reports it. A per-message `catch_unwind`,
   or a supervisor task that revives the loop, would turn a silent, permanent
   defect into a log error.
3. **`Pipeline plan failed: Gap at layer 16 for model 'qwen'` every 5 s** in
   the laptop log: a consequence of losing the peers (with the 16-31 peer out
   of the table, the planner cannot close). Noted for completeness; it should
   disappear with the restart in item 0. Not investigated.
4. **The observed address has no tie-breaking rule** (3.1). Needs a decision:
   prefer stable over temporary as local enumeration does, or require
   agreement from more than one peer before switching. Noted in
   `crates/node/src/network/AGENTS.md` and in
   `docs/subsystems/p2p-network.md`.
5. **The 75 s before the tunnel detour** (3.2). `relay::send_frame` sets 600 s
   per request; a hop behind NAT pays the OS `connect` timeout before the
   tunnel is tried. A product decision about that deadline.
6. **A NAT that really blocks** was never exercised, because the cluster
   reaches itself over global IPv6. Only possible with a machine without
   routable IPv6.
7. **`server-a-2` to `server-a-5` do not announce `libp2p_peer_id` nor
   `transport/libp2p-tunnel-v1`** (measured in `PeerRequest`): exactly the
   nodes that could never use the tunnel. Not investigated - out of scope, but
   the kind of thing that only shows up when the tunnel is needed.
8. **The node's refusal message arrives in pt-BR in an English UI**, and the
   screen shows "The node returned details in an unsupported language" next to
   the original text. Seen on every plan refusal; cosmetic and not touched.

## How to reproduce the measurements

```sh
# Probe node, without touching the operator's cluster
mkdir -p /tmp/probe/models && ln -s <worktree>/apps /tmp/probe/apps
# rdma-p2p.json with gateway_port 8099, p2p_port 42799, bootstrap 127.0.0.1:42777
RDMA_P2P_HOME=/tmp/probe RUST_LOG=info,libp2p_gossipsub=debug \
  ./target/release/rdma-p2p --refresh 3600 > /tmp/probe/log 2>&1 &

grep -c "Not publishing" /tmp/probe/log              # refused publishes
grep -c "Put message in duplicate_cache" /tmp/probe/log   # accepted announcements
grep -c "Adding peer to the mesh" /tmp/probe/log     # the mesh does exist

# Tunnel against a live peer (the multiaddr comes from the announced
# libp2p_peer_id and the peer address with libp2p port = UDP port + 10000)
RDMA_P2P_TUNNEL_PROBE=/ip6/<ip>/tcp/<port+10000>/p2p/<peer-id> \
  cargo test -p node --bin rdma-p2p tunnel_reaches_a_live_peer -- --ignored --nocapture
RDMA_P2P_TUNNEL_PROBE=… \
  cargo test -p node --bin rdma-p2p the_tunnel_carries_a_real_frame -- --ignored --nocapture
```
