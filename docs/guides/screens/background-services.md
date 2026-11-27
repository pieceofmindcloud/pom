---
id: screen-background-services
label: Pieces
summary: Every app this machine runs, open and stopped, like a dock and a folder
title: Pieces
route: /pieces
order: 315
---

:::lead
Pieces shows every plugin this machine runs as an app: its icon with its name underneath. It looks like a desktop: the POM pieces lock together on a night sky behind it, the apps you can open lie on it as icons you drag wherever you like (the layout stays in this browser), and the open ones sit on a dock at the bottom, like the macOS Dock: first the ones that are always on, then, after a divider, the ones you can close. Click a closed app to open it: it lands on the dock, bounces while it starts, and then you are taken to its screen. An app with no screen of its own, such as an inference engine, does not sit on the dock: while it runs it shows at the top right like a system notification, with buttons to configure and close it. Click an app on the dock to go to its screen. The gear that shows when you point at an icon opens its details, such as the memory it uses, and a right click on any icon opens a menu with what you can do with it: open, go to its screen, configure, keep it always on or run it on demand, close. It is where you start and stop the apps that have no menu entry, such as the inference engines (vLLM, Avalon) and the Goose agent the Chat talks to.
:::

Pieces is in the main menu, right below Plugins. The dock of open apps also floats over every other screen of POM: it hides there and comes up when you move the pointer to the bottom edge. Click an app's icon to see its settings: open or close it, the memory it uses, and how it loads on this machine. **On demand** starts it when you open it or something needs it, and you close it to free its memory; **Always on** starts it with POM and keeps it running. The plugin's own choice is marked as its default, and you can change it for any plugin, so a plugin you rarely use does not sit in memory.

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
