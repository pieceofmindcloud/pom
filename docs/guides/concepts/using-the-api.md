---
id: using-the-api
label: Using the API
summary: API keys, compatible routes and harnesses
title: Using the API
order: 60
---

:::lead
Every node speaks the OpenAI-compatible API and also Anthropic's messages API. Pointing a tool at the cluster is choosing a node's URL and sending an API key.
:::

:::diagram api
The client calls any gateway with an API key; the answer comes back with x_pom.source telling which machines did the work.
:::

:::steps
### Create an API key
In Settings > API keys. Each profile creates its own; the cluster connection key does not work for inference.
### Find the model and the limit
GET /v1/models lists the models the network serves right now, with the available context and the maximum output tokens.
### Make the call
Send the key in Authorization: Bearer or in x-api-key. The request enters the distributed pipeline; there is no other hidden route.
:::

```
curl -s http://<gateway>:8080/v1/chat/completions \
  -H "Authorization: Bearer $POM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Say hi in one sentence."}],"max_tokens":32}'
```

:::note The proof that the answer was distributed
The answer carries x_pom.source. A value starting with pipeline: and with two or more hops proves that more than one machine did the work. A single hop, or none, means coverage did not close.
:::

:::cards
### max_tokens
It is your request's ceiling and it is enforced strictly. Reaching it ends the answer with finish_reason length.
### Prompt too large
If the prompt leaves no room in the context window, the answer is HTTP 413 before any generation.
### Streaming
With stream: true the answer arrives over SSE, and a tool call's arguments come out in fragments as the model generates them.
:::

:::note Claude Code and other clients
For Claude Code, point ANTHROPIC_BASE_URL at the gateway's origin (for example http://<gateway>:8080) and use an active API key in ANTHROPIC_API_KEY. OpenAI-compatible clients use the /v1 base.
:::

:::faq
### What is the difference between max_tokens and the deployment's max_new_tokens?
The deployment's max_new_tokens only limits an internal generation leg: the gateway continues in the same session without the client noticing. What limits your request is max_tokens.
### Can I use the connection key in my client?
No. The inference routes only accept an active API key.
:::

:::deep docs/subsystems/http-api.md
Every route, authentication and output limits
:::

:::deep docs/subsystems/inference.md
Output budgets and the continuation loop
:::
