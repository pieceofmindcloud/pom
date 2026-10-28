---
id: screen-deploy
label: Deploy to network
summary: Put a model in service across the machines
title: Deploying a model
route: /deploy
order: 100
---

:::lead
Deploy to network is the studio where a model goes into service: you choose what to deploy, place the machines on the map, read the planner's verdict and adjust the settings before pressing Deploy.
:::

![The Deploy studio: the mode bar at the top, the map in the middle, the inspector on the right and the machines of the network along the bottom.](../images/en/deploy.webp)

## The three parts of the screen

:::cards
### The top bar
What to deploy: **Shards** (a set of slices, one range per machine) or **Whole model** (the whole file on a machine). Beside it, the engine (llama.cpp, or an inference service a plugin publishes), the model and the deployment's name.
### The map
The machines in the deployment, each with the layer range it will serve, the memory it has and the projection after loading. The **Gateway** closes the cycle with the cost of one token once it can be measured.
### The inspector
**Global** holds the settings of the whole deployment (context, sessions, sampling, the POM settings); the machine tab holds what is per machine: GPU layers and whether the KV cache goes on the GPU.
:::

## From choice to service

:::steps
### Choose the model
For Shards, the model list shows the slice sets some machine of the network already has. For Whole model, the whole GGUF files.
### Put the machines on the map
Drag machines from the strip at the bottom to the map. The planner places the ranges, and **Model coverage**, at the bottom right, counts the layers covered.
### Read the verdict
When the selection cannot be deployed, a red line at the top says why - for example, which layers are in no chosen slice. The Deploy button stays off until the plan closes.
### Deploy
Each machine extracts or loads its own range. The deployment shows up in the Dashboard as soon as the machines confirm they are serving.
:::

:::diagram lifecycle
The life of a slice: only the state the node itself confirmed lets it come back after a restart.
:::

:::warn Copies need your permission
If a machine does not have the file it needs, the deploy is refused unless you allow transfers - and the screen lists the copies before asking. Weights never move because of slicing alone.
:::

:::note Settings help
Each category of the inspector has a help button that opens its guide: Default settings, POM settings and the Beta settings.
:::

:::deep docs/subsystems/deployment.md
The plan, the merge, the order and the slice lifecycle
:::

:::deep docs/subsystems/parallelism.md
Pipeline or tensor parallelism
:::
