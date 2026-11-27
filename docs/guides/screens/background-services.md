---
id: screen-background-services
label: Services
summary: Every app this machine runs, open and stopped, like a dock and a folder
title: Services
route: /plugins
order: 315
---

:::lead
Services shows every plugin this machine runs as an app: its icon with its name underneath. The apps you can open sit at the top, like a folder of apps; the open ones sit on a dock at the bottom of the screen, like the macOS Dock: first the ones that are always on, then, after a divider, the ones you can close. Each open app shows how much memory (RAM) it uses. It is where you start and stop the apps that have no menu entry, such as the inference engines (vLLM, Avalon) and the Goose agent the Chat talks to.
:::

Services is the first tab of Plugins once a plugin is installed. Click an app's icon to see its settings: open or close it, the memory it uses, and how it loads on this machine. **On demand** starts it when you open it or something needs it, and you close it to free its memory; **Always on** starts it with POM and keeps it running. The plugin's own choice is marked as its default, and you can change it for any plugin, so a plugin you rarely use does not sit in memory.

:::cards
### Open
The shelf at the top: every app whose process is up, with a light under it. Below the name: what it is (an inference engine and the model it serves), or the memory it uses where this machine can measure it (Linux). A plugin that always runs says "always on".
### Close
The X that shows on an open app when you point at it stops it, after a confirmation. It keeps its license, installation, settings and data, and uses no memory, processor or GPU until opened again. Closing the engine that serves this machine's model releases that model first.
### Stopped
The folder below: the apps installed but closed, dimmed. Click an icon to open it. A deployment starts an inference engine by itself, and the Chat opens an agent plugin when you pick its agent.
:::

:::deep docs/subsystems/plugins-ui.md
On-demand plugins, inference engines and the ACP agents they serve
:::
