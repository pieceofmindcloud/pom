---
id: human-network
label: Network
summary: How machines find each other, prove who they are and get through a NAT
title: Inside the network
reference: crates/node/src/network/AGENTS.md
order: 20
---

:::lead
This folder is how a node finds the others and is found: a signed UDP announce on the local network, and an iroh endpoint with relays and hole punching across the internet. Its one rule: a machine is its key, never its IP.
:::

:::diagram network
On the local network discovery is a UDP broadcast. Across the internet, the endpoint uses a relay and hole punching to reach a machine behind a NAT.
:::

:::cards
### Identity is a key
`.pom-node.key` is the only key of a node. Its id, its TLS certificate, its iroh endpoint and its pkarr record all derive from it. Deleting the file makes a new machine.
### Announce every five seconds
Each node announces, signed, who it is, how much memory it has and which layers it serves. Peers that stop answering drop out.
### Addresses are hints
An advertised address only goes in front of the candidates. Whoever dials decides which address to use, and remembers which ones failed.
### The ladder
Direct first, then a hole-punched connection, then a relay: n0's public ones, a dedicated one, or a member of your own network. A file never crosses a public relay.
:::

:::note Mutual TLS, always
Between machines every call is mutual TLS with certificates derived from the node keys. The connection key alone opens nothing: the caller must also be an admin of the trust chain or a member with a valid token.
:::

:::deep crates/node/src/network/AGENTS.md
Every file of the network layer
:::

:::deep docs/subsystems/p2p-network.md
Discovery, identity, relays and NAT traversal in full
:::
