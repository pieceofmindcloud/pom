---
id: screen-preferences-admin
label: Administrative preferences
summary: Network-wide switches reserved for the network authority
title: Administrative preferences
route: /config/preferences/admin
order: 270
---

:::lead
Administrative preferences are the switches that change the whole network, not just this machine, and that only the network's authority may turn.
:::

![Administrative preferences: QUIC transport, process logs between nodes, network chat and local discovery.](../images/en/preferences-admin.webp)

:::cards
### QUIC transport
When on, new inference hops prefer HTTP/3 over QUIC. QUIC always carries TLS 1.3 and falls back to HTTP or HTTPS when a peer cannot use it.
### Process logs between nodes
When on, every node sends a copy of its process log to each administrator over a separate channel; the Deployment logs screen reads it. The queue drops lines under pressure, so inference is never held up.
### Network chat
The low-priority conversation channel between the participants. Messages and files travel over the network and are validated by the principal machine.
### Relays
Which relays the machines may use to reach each other: all of them (the default), only this network's own (its dedicated relays and machines of the network that relay, no public relay run by a third party), or none. Traffic through a relay is already encrypted end to end, so a relay cannot read it, but it sees which machines talk, when and how much. With no relay, only direct connections work: a machine behind NAT without an open port cannot reach the others. Only the principal changes it, and every machine applies it within seconds.
### Automatic local discovery
Lets machines on the same local network find each other by broadcast. It is off by default.
:::

:::deep docs/subsystems/p2p-network.md
Transport, logs and chat between machines
:::
