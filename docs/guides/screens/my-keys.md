---
id: screen-my-keys
label: My API keys
summary: The API keys of a signed-in person
title: My API keys
route: /config/my-keys
order: 282
---

:::lead
My API keys is where a person signed in to this node with their own account makes keys for their own tools. Requests made with these keys carry that person's identity, so they count against their quota and show up in their own logs. Only a person's session opens this screen; the administrator's panel session does not, and makes node keys in API keys instead.
:::

:::steps
### Create a key
**Create key**, with an optional name that tells you later what it is for.
### Copy it once
The key is shown only once, right after creation. Copy it then.
### Revoke
**Revoke** stops a key at once; the list keeps it marked as revoked.
:::

:::deep docs/subsystems/security-model.md
Who may do what, and the keys of a person
:::
