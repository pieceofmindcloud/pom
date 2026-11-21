---
id: screen-background-services
label: Background services
summary: The plugins that run only while opened, the memory each uses, and closing them
title: Background services
route: /config/background-services
order: 315
---

:::lead
Some plugins run only while you use them. Background services lists them, says how much memory each running one takes, and closes the ones you are done with. It is the only place to close a plugin that has no menu entry of its own, such as the Goose agent the Chat talks to.
:::

:::cards
### Running
Each on-demand plugin whose process is up, with its version and, where this machine can measure it (Linux), the resident memory of the plugin and everything it started.
### Close
Stops the plugin at once, after a confirmation. It keeps its license, installation, settings and data, and uses no memory, processor or GPU until it is opened again.
### Stopped
The on-demand plugins that are installed but closed. Open starts one again; the Chat also opens an agent plugin by itself when you pick its agent.
:::

:::deep docs/subsystems/plugins-ui.md
On-demand plugins and the ACP agents they serve
:::
