# GPU instances: several deployments on one machine

Status: design, not implemented. Part 1 (the per-deployment GPU choice) is
implemented; this document covers what remains.

## Goal

One POM process per machine, several deployments on it at once, each on its
own GPUs (for example a two-GPU machine serving model A on GPU 0 and model B
on GPU 1), for every engine. Today that needs one POM node (one container) per
GPU.

## What already exists (part 1)

- `RuntimeParams.gpus` / `NodeConfig.gpus` (`config::GpuSet`) per machine,
  chosen in the Deploy Studio's Machines tab, all GPUs by default.
- llama.cpp loads the model on the chosen devices only
  (`EngineConfig.devices`); a plugin's engine receives only the chosen GPUs in
  `engine.load` (`plugin_engine::chosen_gpus`) and pins its process to them.

So one deployment can already use a subset of a machine's GPUs. What is
missing is a second deployment on the same machine at the same time.

## Why it is not a small change

A node serves one model: `NodeRuntime` holds one `NodeConfig`, the `Announce`
carries one `model` / `deploy_id` / layer range and one `inference_port`, and
the planner, the routing, `/health` and the deployment records index
everything by `node_id`. Making one node announce a list of served models
would change every contract between machines (gossip, planner, health,
records, telemetry) and break the coexistence with released nodes
(compatibility policy in the root `AGENTS.md`).

## Design: an instance is a member of its own

An **instance** is a second POM process on the same machine, spawned and
supervised by the machine's node (the **host**):

- its own home under the host's (`<home>/instances/<id>`), so its own
  `.pom-node.key` and `node_id`;
- its own P2P, inference and loopback-only gateway ports, allocated by the
  host; no public interface (the host's interface and API stay the only URL);
- the host's network settings, trust chain and connection key copied into its
  `pom.json`; every GPU visible (the deployment's `gpus` restricts the engine,
  part 1), never a process-wide `pom.json` `gpus`, which would renumber the
  cards for the engines;
- `node_name` = the host's name and the GPUs (`ksia-gpus · GPU 1`), and an
  optional `host_node_id` in its `Announce` so the screens group it under the
  host.

For the network, an instance is a peer like the containers used today:
planner, routing, pipeline, sessions and the prefix cache do not change.

### Membership (the hard part)

A new node is a member only once an admin's roster approves it
(`LobbyRequest::Admission`, a decision, a roster vouch). Today only the
operator sends that request, from the Networks screen. An instance needs:

1. **A host certificate**: the host signs `{instance_node_id, host_node_id,
   gpus, issued_at}` with its node key.
2. **A headless join**: the instance sends `LobbyRequest::Admission` itself at
   start, with an optional `host_certificate` field (new, optional: older
   principals ignore it), and takes the grant like the Networks screen does.
3. **Auto-approval on the principal**: a valid certificate from a host the
   roster admits approves the instance at once with the host's role (an
   admin host's instance is a worker). A ban of the host bans its instances
   (the revocation lists the host; `apply_decision` cuts the instances whose
   certificate names it).

After that the instance holds an ordinary roster vouch, so every role gate
works unchanged.

### Deploying onto an instance

The Machines tab keeps showing the host with its GPU boxes. At launch, for a
target `(host, gpus)`:

- the host serves it itself when it serves nothing else, or serves this same
  deployment;
- otherwise a free instance of the host serves it, or the host spawns one
  (`POST /api/ui/instances/ensure {gpus}` to the host, answered with the
  instance's `node_id` once it is a member) and the launch targets the
  instance with the same `gpus`;
- two deployments on overlapping GPUs of one machine are refused by the plan.

Undeploying an instance's last deployment stops it after a grace period; the
host keeps the list of its instances in `pom.db` and starts them again at its
own start, with their last slice (`last-slice.json` lives in each instance's
home).

## Phases

1. Instance supervisor on the host (spawn, ports, homes, restart, stop),
   `host_node_id` in the announce, names.
2. Host certificate, headless join, auto-approval and ban cascade.
3. Deploy redirection and the overlap refusal; screens grouping instances.
4. Docs: `deployment.md`, `p2p-network.md`, `security-model.md`,
   `operations.md` (the extra ports a container must publish).
