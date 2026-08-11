# Routing provider boundary

## Current workspace boundary

The open-source workspace owns the public `routing-contract` crate and the
deterministic `routing-base` provider. The contract defines
`ROUTING_API_VERSION`, `RoutingRequest`, `RoutingPlan`, peer capacity data, and
validation errors. Both request and plan validation reject an unsupported API
version before a plan can be used.

`crates/node` currently converts its transport-owned peer records into a
`RoutingRequest` as a compatibility seam. Production inference continues to use
the existing node pipeline planner while equivalence coverage is expanded; the
node does not yet invoke `routing-base` or load an external provider at runtime.
This distinction keeps the workspace migration behavior-preserving.

## Future private provider

An enterprise routing provider is not a Cargo feature, workspace member,
dependency, asset, or source directory in this repository. A future private
distribution may run as a signed local process and negotiate the same contract
over a per-user local endpoint. On Unix that endpoint will be a Unix domain
socket; on Windows it will be a named pipe protected by the current user's ACL.

When runtime provider selection is introduced, the node must record a missing,
unhealthy, unlicensed, or incompatible provider and fall back to
`routing-base`. The provider must receive only a `RoutingRequest` and return a
`RoutingPlan`; it must not receive direct access to node sockets, model files,
HTTP credentials, or the inference engine.

## Compatibility rules

- Providers and nodes must use the same `ROUTING_API_VERSION`.
- A valid plan covers every layer exactly once, in order, from layer 0 through
  the model's final layer.
- The public contract contains JSON-safe data only. Transport clients, runtime
  handles, filesystem paths, and private provider implementation details stay
  outside it.
