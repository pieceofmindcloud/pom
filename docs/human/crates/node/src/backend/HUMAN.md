---
id: human-backend
label: Backend and API
summary: The gateway every node serves, and who may call what
title: Inside the backend
reference: crates/node/src/backend/AGENTS.md
order: 50
---

:::lead
Every node is a gateway: on port 8080 it serves this interface, the OpenAI- and Anthropic-compatible API and the administration routes. This folder is that HTTP side. It owns the only way to generate text - the distributed pipeline - and the rules of who may call each route.
:::

:::diagram api
The client calls any gateway with an API key; the answer comes back with x_pom.source telling which machines worked.
:::

:::cards
### One generation path
`/api/generate` and the `/v1` routes only run the distributed pipeline. With no plan covering every layer, the request fails with a message - there is no fallback.
### Two separate surfaces
The pipeline routes and the administration routes never share an authorization rule. Inference takes an API key minted by this node; administration takes the browser session or a signed request from another member.
### Each route has a scope
Some routes only the principal may call, some any admin, some a serving member, some only this machine. The role is enforced by the nodes, never by the screens.
### The client's budget is final
The `max_tokens` the client asks for is a ceiling nothing crosses. The deployment's `max_new_tokens` only sizes one leg of the gateway's continuation loop, invisible to the client.
:::

:::diagram profiles
The principal admin governs the network; the secondary admin deploys and admits; the worker serves layers; the visitor talks to the models.
:::

:::deep crates/node/src/backend/AGENTS.md
Every router and handler of the backend
:::

:::deep docs/subsystems/http-api.md
Every route, authentication and scope
:::
