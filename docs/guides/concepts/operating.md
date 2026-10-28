---
id: operating
label: Operate and diagnose
summary: Bring it up, check it, find what broke
title: Operate and diagnose
order: 90
---

:::lead
A distributed cluster fails in ways a single machine never does: half the network sees the other half, a slice did not load, an address changed. This page is the checklist.
:::

:::diagram lifecycle
A slice's lifecycle: only the state the node itself confirmed may be resumed after a restart.
:::

## The proof that it works

Every answer carries the chain that produced it in x_pom.source. A source starting with pipeline: and listing two or more hops is the only proof that inference was joint. A single hop, or none, means the planner did not close layer coverage.

```
curl -s localhost:8080/peers | python3 -m json.tool
curl -sf localhost:8181/health
```

:::checklist Quick check
- /peers lists every expected machine.
- The inference server's /health answers on each machine with a slice.
- A test call comes back with x_pom.source starting with pipeline: and two or more hops.
:::

## Symptoms and where to look

| What you see | Likely cause | Where to confirm |
|---|---|---|
| The machines do not see each other | discovery never left the node, or the advertised address is wrong | the Network screen and the dashboard node list |
| Source “none” in the answer | layer coverage has a gap | the Deploy to network screen shows the missing range |
| A node shows up with no slice | the model did not load, or the inference port is taken | the machine's card on the dashboard states the reason |
| The answer takes long to start | the machine holding layer 0 is chewing the prompt | move layer 0 to a machine with more memory |
| A slice does not come back after a restart | it never finished loading before it died | the card shows why the resume was refused |
| The node shows as idle | the model was unloaded for inactivity | nothing to do: the next request reloads it |

:::warn Two machines, two homes
Running two nodes from the same directory makes both load the same identity, and the cluster treats them as one machine. Different ports are not enough: the second node needs its own working directory (POM_HOME).
:::

:::note Deployment logs
Deployments > Logs gathers the process logs of every machine in arrival order, and can be filtered by deployment.
:::

:::deep docs/subsystems/operations.md
Bring-up, verification and field gotchas
:::

:::deep docs/subsystems/build-and-run.md
Build, GPU features and environment variables
:::
