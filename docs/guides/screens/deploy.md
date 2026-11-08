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
What to deploy: **Pipeline**, the default, starts from a whole model: one machine serves it all, or several machines that hold it each serve a range. **Slices (legacy)** shows only for slice sets made before, or for an inference service that runs slices in stages. Beside it, the engine (llama.cpp, or an inference service a plugin publishes), the model and the deployment's name.
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

In **Pipeline**, the card of a machine that does not have the model says so. **Download here** has it download the same Hugging Face repository and file another machine downloaded, every part of a split file included; the card then shows the progress. For a file copied by hand, put it in one of that machine's model folders and press **Rescan**. A model nobody downloaded through POM offers only Rescan. A machine without the model is never sent a copy over the network: the Deploy button stays grey and names it until the model is there. A machine given more layers than its GPUs hold runs the rest on the CPU; Deploy then asks for a confirmation that names each such machine with its GPU and CPU layer counts.
### Split a whole model over machines that have it
In **Pipeline**, when two or more machines on the map hold the whole file, the Machine tab offers **Split the layers over N machines**. It divides the layers evenly, in map order, at cuts the model allows. Each machine then shows its range with **-** and **+**: a click moves the cut it shares with its neighbour by one valid step, and a button that cannot move says why (no valid cut that way, or a machine would be left with no layers). Each machine serves its range from its own copy, so nothing is copied over the network. A machine that cannot point at its range writes it to disk as a slice first; its tab says so, with the size.

The lock beside a range keeps it exactly as it is. **Auto-balance by free memory** then divides the layers of the unlocked machines in proportion to each one's free memory (GPU memory, or RAM on a machine without a GPU), at cuts the model allows, around the locked ranges. A locked range's - and + are off until you unlock it, and so are a neighbour's buttons when they would move the cut it shares with the locked range. It counts the weights and the context's memory (the KV cache, for the context size and sessions you set), but not the compute buffers.

When a machine's free VRAM cannot hold its whole range, its tab shows how the layers split, for example **18 on GPU, 6 on CPU**, with the RAM the CPU part needs. The layers that do not fit run on the CPU, which makes generation much slower; the deploy still goes ahead after the usual confirmation, unless the CPU part does not fit in the machine's free RAM either: then the plan is refused and says how much RAM it needs. Auto-balance never chooses this on its own, so give that machine fewer layers or add a machine when you can.
### Read the verdict
When the selection cannot be deployed, a red line at the top says why - for example, which layers are in no chosen slice. The Deploy button stays off until the plan closes.
### Deploy
Each machine extracts or loads its own range. A window follows the deploy step by step until it is ready or fails, with the reason; **Switch to dashboard** takes you to the Dashboard, and **Close** leaves the deploy running, its result staying in the bell. The deployment shows up in the Dashboard as soon as the machines confirm they are serving. A deployment is deployed once at a time: pressing Deploy again while it comes up only reopens that window.
:::

:::diagram lifecycle
The life of a slice: only the state the node itself confirmed lets it come back after a restart.
:::

:::warn Copies need your permission
If a machine does not have the file it needs, the deploy is refused unless you allow transfers - and the screen lists the copies before asking. Weights never move because of slicing alone.
:::

:::note Settings help
Each category of the inspector has a help button that opens its guide: Default settings and POM settings.
:::

:::deep docs/subsystems/deployment.md
The plan, the merge, the order and the slice lifecycle
:::

:::deep docs/subsystems/parallelism.md
Pipeline or tensor parallelism
:::
