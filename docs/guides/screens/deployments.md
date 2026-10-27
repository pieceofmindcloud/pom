---
id: screen-deployments
label: Deployments (History)
summary: Every deployment of the network, its versions and its state
title: Deployments
route: /deployments
order: 110
---

:::lead
Deployments is the history: every deployment the network registered, newest activation first, with the machines it uses, whether it is serving and which one answers when a request names only the model.
:::

![Deployments: one row per deployment, with its shape, machines, last activation, state and actions.](../images/en/deployments.webp)

:::cards
### Serving and Default
**Serving** means live machines are announcing it. **Default** marks the deployment that answers a request naming only the model, with no `@` suffix. The same model can be deployed more than once, with other machines or other settings.
### Versions
A deployment keeps a version per change of settings or machines. Expanding the row lists them, newest first, with the active one marked and **Re-deploy this version** on each.
### Re-deploy with new settings
Opens the same inspector as the Deploy screen, seeded with the active version; **Reload with these settings** sends only what changed, on the same machines and the same layers.
### View process logs
Opens the Deployment logs filtered to this deployment.
:::

:::note The panel on the right
Clicking a version shows its settings read-only. The KV cache is not there on purpose: it belongs to the machine and to the moment, not to the version.
:::

:::deep docs/subsystems/deployment.md
Versions, the default deployment and redeploying
:::
