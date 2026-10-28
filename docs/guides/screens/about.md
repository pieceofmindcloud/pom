---
id: screen-about
label: About
summary: The license, the privacy notice, the libraries and POM updates
title: About
route: /config/about
order: 300
---

:::lead
About says what POM is made of and under which terms: its license, the privacy and security notice, the libraries it depends on, and the check for a newer POM.
:::

![About: the POM software license, the privacy and security notice and the links to the website and the repository.](../images/en/about.webp)

:::cards
### License
POM is licensed under the GNU Affero General Public License, version 3 (AGPL-3.0).
### Privacy and security (LGPD)
POM moves prompts and model data between machines and cannot guarantee privacy on public networks or with unknown computers. You decide what data to send and which computers to trust.
### Libraries
The direct dependencies of the interface and of the Rust workspace, grouped by license.
### POM update
Checks the POM releases on GitHub (the website as fallback) and applies one only after you authorize it; the node restarts on the new version. It needs no license.
:::

:::deep docs/subsystems/enterprise-licensing.md
How POM updates itself
:::
