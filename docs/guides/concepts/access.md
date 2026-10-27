---
id: access
label: Access and security
summary: Password, keys and what each one opens
title: Access and security
order: 100
---

:::lead
There are three credentials, and each one opens a different door. Knowing which is which keeps you from handing the wrong key to the wrong person.
:::

:::cards
### Panel password
A human credential to open the interface. It only lives in the tab's memory; the node keeps just a hash of it. The session covers the administrative routes.
### Connection key
The same across the whole cluster. It identifies the operator and the channel between nodes: it authenticates the administrative and deployment routes. It does not work for inference.
### API key
The only credential accepted on /v1 and /api/generate. Each one has a non-secret identity, so the records show who asked without exposing the secret.
:::

:::note The inference port does not accept client keys
The inference server between machines only accepts the cluster's internal credential. Client API keys are refused there, and only /health stays public to check readiness.
:::

:::warn Think about the network before exposing a port
Without TLS between nodes, the hidden state travels in clear text. Turn TLS on in Administrative preferences when the machines talk over networks you do not control.
:::

:::note Blocking a machine
A node can be blocked by identity, and that identity derives from a key living in the machine's working directory. Deleting that key creates a new machine in the eyes of the others.
:::

:::faq
### Where do I see the connection key?
In Settings > Connection keys. Rotating it cuts off every other node, so the button only shows up for whoever has authority.
### Does a leaked API key compromise the cluster?
It only opens the inference routes. Revoke it in API keys and create another; the connection key stays valid.
:::

:::deep docs/subsystems/http-api.md
Authentication, profiles and every route
:::

:::deep docs/subsystems/p2p-network.md
TLS between peers and limitations for the internet
:::
