---
id: screen-sharing
label: Sharing
summary: The trust root and what another machine needs to join
title: Sharing network access
route: /config/sharing
order: 250
---

:::lead
Sharing gathers what another machine needs to trust this network: the fingerprint of the trust root, the trust document to download, and, for a private network, the connection key.
:::

![Sharing: this machine's trust root with its fingerprint, and below it the trust document to copy or download.](../images/en/sharing.webp)

:::steps
### The trust root
The root is this machine's own key. **Anchored** means this node already has one. Its fingerprint is public and grants nothing on its own.
### Carry the fingerprint
Take the fingerprint to the other machine by a channel you trust - read it out, type it there by hand. That check is what anchors the trust.
### Give the trust document
**Download** the trust document and add it on the other machine, in Networks > Add network. A private network also needs its connection key, which only the principal hands over.
:::

:::note The connection key
The same string on every machine of the network: it authenticates the administration routes and the synchronization between nodes, but alone it opens nothing, since every call between machines is also mutual TLS from an admin or a member with a valid token. This screen shows it with the bootstrap address and the API base URL; **Generate another** rotates it on this machine only, and the others stop accepting this one until they get the new key. Only whoever has authority sees the button.
:::

:::note Trust root, the older address
The old Trust root address (`/config/trust`) still opens the trust root part of this screen, for old links; it is no longer in the menu.
:::

:::deep docs/subsystems/security-model.md
The trust chain and how a machine anchors it
:::
