---
id: network-and-internet
label: Local network and internet
summary: How machines find each other and which ports to open
title: Local network and internet
order: 70
---

:::lead
On the same local network the machines find each other on their own. Across the internet they need a starting point and an address the other side can dial.
:::

:::cards
### Local network
Discovery over UDP with broadcast and periodic exchange of announcements. It works with no infrastructure at all and announces even without a loaded model.
### Internet
An iroh endpoint over QUIC (UDP), with relays to cross NAT and hole punching. Addresses are also published in a DHT.
:::

:::diagram network
On the local network discovery is by UDP broadcast. Across the internet, the endpoint uses a relay and hole punching to reach a machine behind NAT.
:::

## The ports

| Use | Default | Protocol |
|---|---|---|
| P2P discovery (--port) | 42777 | UDP |
| P2P endpoint | P2P + 10000 (52777) | UDP |
| Cluster relay | P2P + 10000 (52777) | TCP |
| Interface and API (--gateway-port) | 8080 | TCP |
| Inference server | 8181 | TCP |
| Inference QUIC | P2P + 1000 (43777) | UDP |
| Gateway QUIC | P2P + 2000 (44777) | UDP |

:::steps
### Give it a starting point
Start the node with --bootstrap <machine>:42777 pointing at a machine already in the network.
### Pin the advertised address, if needed
In My machine, Advertised address. With a pinned address, it is the only one announced and dialed by the other nodes. Clearing the field goes back to the detected address.
### Check the traversal
The Network screen shows the NAT state, direct addresses, relays and hole punches - the three steps where two nodes behind NAT usually fail.
:::

:::note Private or public network
A private network requires the connection key and the trust.json to join. A public network shows up in the Public networks directory with its name, country and city, and requires the trust.json.
:::

:::note Encryption between nodes
The principal administrator turns TLS between nodes on or off in Administrative preferences, without a restart. The other nodes adopt the policy within a few seconds. Each node's certificate is derived from its identity.
:::

:::deep docs/subsystems/p2p-network.md
Discovery, addresses, NAT and TLS
:::

:::deep docs/subsystems/operations.md
Ports and field gotchas
:::
