---
id: screen-deploy
label: Deploy to network
summary: Put a model in service across the machines
title: Deploying a model
route: /deploy
order: 100
---

:::lead
Deploy to network is the studio where a model goes into service: you choose the model, place the machines on the map, give each machine the layers it serves and press Deploy, which checks the map and says why it cannot run, if it cannot.
:::

![The Deploy studio: the mode bar at the top, the map in the middle, the inspector on the right and the machines of the network along the bottom.](../images/en/deploy.webp)

## The three parts of the screen

:::cards
### The top bar
What to deploy: **Pipeline** (a language model, from its whole GGUF) or **Image model**. Beside it, the engine (llama.cpp, or an inference service a plugin publishes), the model and the deployment's name, and the Deploy button.
### The map
The machines in the deployment, each with the layer ranges it will serve and its live GPU, RAM and load. A machine follows the pipeline's order as soon as it has a range; **Organize** puts the cards you dragged back in place. The **Gateway** closes the cycle.
### The inspector
**Global** holds the settings of the whole deployment (context, sessions, sampling, the POM settings). **Machines** lists every machine on the map, in pipeline order, with only what you edit per machine: its layer ranges, GPU layers and whether the KV cache goes on the GPU.
:::

## From choice to service

:::steps
### Choose the model
The whole GGUF files known on any machine.
### Put the machines on the map
Drag machines from the strip at the bottom to the map. The first one takes the whole model; the next ones join without a range, and you give them layers in the Machines tab. Nothing is checked while you edit, so the map never redraws on its own: **Model coverage**, at the bottom right, counts the layers covered.

The card of a machine that does not have the model says so. **Download here** has it download the same Hugging Face repository and file another machine downloaded, every part of a split file included; the card then shows the progress. For a file copied by hand, put it in one of that machine's model folders and press **Rescan**. A model nobody downloaded through POM offers only Rescan. The same goes for a vision projector or an MTP draft the deployment needs on a machine that lacks it. A machine without the model is never sent a copy over the network.
### Give each machine its layers
In the **Machines** tab, each machine that holds the whole file can serve it whole (**Whole model**), one range or several (**+ Range**). Each range has a slider with two handles that stop only at cuts the model allows, so any range it gives can be served: each machine reads its layers from its own copy of the whole file, nothing is written or copied. **Split the layers over N machines** divides the layers evenly, in map order, as a starting point. Two machines on the same range are replicas.
### Deploy
Pressing Deploy checks the map once against the network as it is now. When the map cannot run - layers no machine serves, two ranges that overlap, a machine without the model - a window lists why and nothing is deployed. A map that leaves layers without a machine asks before going ahead; machines already serving a model are named before they are taken over. Then each machine loads its own range. A window follows the deploy step by step until it is ready or fails, with the reason; **Switch to dashboard** takes you to the Dashboard, and **Close** leaves the deploy running, its result staying in the bell. A deployment is deployed once at a time: pressing Deploy again while it comes up only reopens that window.
:::

:::diagram lifecycle
The life of a slice: only the state the node itself confirmed lets it come back after a restart.
:::

:::note Settings help
Each category of the inspector has a help button that opens its guide: Default settings and POM settings.
:::

## Image models

**Image model** in the mode bar turns the studio into an image pipeline. An image model (Z-Image, Qwen Image, Flux...) is served by stage, in the order the image is made: the **text encoder** reads the prompt, the **diffusion model** draws the image step by step, and the **VAE** turns the result into pixels. Each stage runs on machines that run ComfyUI and already hold that stage's file, so a small GPU can hold the diffusion model while the others live elsewhere.

:::steps
### Choose the files
Choose the diffusion model's file. The text encoder and the VAE are filled in when the same folder holds exactly one of each; change them if you want others.
### Drag machines onto the stages
Drag machines from the strip at the bottom onto each stage. More than one machine on a stage makes replicas: each call goes to the nearest one that answers, and the image keeps going when one of them stops. One machine can serve several stages; the small VAE usually rides along with another one. A machine that cannot take a stage says why on its card.
### Read the verdict and deploy
The red line at the top says what is still missing. Deploy follows the same progress window as a language model, and the deployment shows on the Dashboard and in Deployments.
:::

ComfyUI on any machine of the network lists the stages as `POM ▸ <name> ▸ <file>` in its ordinary loaders, and the Chat can generate images with it. Nothing is copied: a machine without the file is not offered.

:::deep docs/subsystems/deployment.md
The plan, the merge, the order and the slice lifecycle
:::

:::deep docs/subsystems/parallelism.md
Pipeline or tensor parallelism
:::
