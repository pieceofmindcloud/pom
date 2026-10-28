---
id: plugins
label: Plugins
summary: Licenses and repositories, machine by machine
title: Plugins
order: 80
---

:::lead
Plugins add screens and features to the panel. They are local: each machine activates and installs its own, and one plugin's license never turns another one on or off.
:::

:::diagram plugins
Each plugin runs in its own process next to the node. The license or GitHub repository only applies to this machine.
:::

:::steps
### Open Plugins
Settings > Plugins shows the catalog, with what is active on this machine.
### Activate by license
Paste the license key in the form. The answer says which plugins that key granted and installed.
### Or install from GitHub
Add a public repository URL in Add from GitHub and pick a release compatible with this machine. Installing and updating ask for confirmation.
:::

:::cards
### Any profile
Administrator, node and inference can activate, install, update and use plugins on their own machine.
### Discoverable screens
Request logs and Statistics show up in the menu even without a license; the data only arrives once the matching plugin is active.
### Separate process
Each plugin is loaded by its own process, pom-plugin-host, and not inside the main binary. Plugins are independent from one another.
:::

:::note Updating pom is not a plugin
Checking for a new version of the binary itself lives in Settings > About, separate from the Plugins screen.
:::

:::deep docs/subsystems/enterprise-licensing.md
Licenses, GitHub repositories and the process boundary
:::

:::deep docs/subsystems/plugins-ui.md
Menus, screens and routes provided by plugins
:::
