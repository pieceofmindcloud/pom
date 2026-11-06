---
id: profiles
label: The profiles
summary: Admin, secondary admin, worker and visitor
title: The profiles
order: 30
---

:::lead
Every network has one principal admin, as many secondary admins as it promotes, and members, who are workers or visitors. A member's role is decided at admission; an admin's comes from the trust chain.
:::

:::diagram profiles
The principal admin governs the network; the secondary admin deploys and admits; the worker serves layers; the visitor talks to the models.
:::

:::cards
### Admin
The network's principal, proven by the trust chain. Does everything: settings, keys, promoting and demoting admins, admissions, removals and deploys.
### Secondary admin (promoted)
An admin the principal promoted, with the grant signed in the trust chain. Deploys and undeploys models, sees logs and telemetry, approves or declines admissions and removes workers and visitors. Settings, keys and admin changes stay with the principal.
### Worker
A machine admitted to process: it receives slices and serves its layer range, shows the local model and load, and can leave the network whenever it wants.
### Visitor
A reduced view for talking to the models through its own node: Chat, documentation, its own API key and the Network Chat when the network opens it. It never holds or serves a model.
:::

| Action | Admin | Secondary admin | Worker | Visitor |
|---|---|---|---|---|
| Talk in the Chat | yes | yes | yes | yes |
| Deploy and undeploy models | yes | yes | no | no |
| Approve or decline admissions | yes | yes | no | no |
| Remove workers and visitors | yes | yes | no | no |
| Change the network's settings and keys | yes | no | no | no |
| Promote and demote admins | yes | no | no | no |
| Serve slices of a model | yes | yes | yes | no |
| Activate and install plugins on its own machine | yes | yes | yes | yes |
| Create its own API key | yes | yes | yes | yes |
| Leave the network | yes | yes | yes | yes |

:::note The screen hides, the server decides
Hiding a button is only presentation. The real authorization happens on the nodes, which refuse a route outside the role even when someone calls it by hand.
:::

The role is chosen in the admission request and the admin decides when approving. A machine admitted as a visitor stays one, and the principal refuses to promote it; only a worker can be promoted to secondary admin, in My Network > Administrators.

:::deep docs/subsystems/security-model.md
Roles, trust chain and membership tokens
:::

:::deep docs/subsystems/web-ui.md
What each profile sees in the interface
:::
