---
id: human-plugin-host
label: Plugin host
summary: Why a plugin runs in its own process, and how it talks to POM
title: Inside the plugin host
reference: crates/plugin-host/AGENTS.md
order: 60
---

:::lead
A plugin never runs inside the `pom` process. `pom-plugin-host` loads it on the side, one instance per active plugin, and the two talk through a small versioned protocol. A plugin that crashes takes only its own process down.
:::

:::diagram plugins
Each plugin runs in a process of its own beside the node. The license or the GitHub repository counts for this machine only.
:::

:::steps
### Activate
A plugin is activated per `plugin_code`: by a license key the license server granted, or from a public GitHub repository. Plugins share no state with each other.
### Load
`pom` starts `pom-plugin-host` as a child with the plugin's library. The library is loaded through a C ABI, and the two processes talk over stdin and stdout - no socket, no shared volume.
### Show up
The plugin declares its menus, screens, routes and texts in a manifest (`pom-plugin-ui/v1`), and POM draws them in this interface.
### Stay current
Each plugin updates on its own. POM's own update is not a plugin: it lives apart and swaps the binary through the same host.
:::

:::note A license per plugin
There is no single product-wide license: each plugin keeps its own activation, lease and heartbeat. A screen of a paid plugin stays in the menu without a license, but the node refuses its data.
:::

:::deep crates/plugin-host/AGENTS.md
The host process
:::

:::deep docs/subsystems/enterprise-licensing.md
Licenses, the process boundary and updates
:::
