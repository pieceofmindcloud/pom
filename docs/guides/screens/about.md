---
id: screen-about
label: About
summary: The license, the privacy notice, the libraries and PoM updates
title: About
route: /config/about
order: 300
---

:::lead
About says what PoM is made of and under which terms: its license, the privacy and security notice, the libraries it depends on, and the check for a newer PoM.
:::

![About: the MIT license, the privacy and security notice and the PoM update check.](../images/en/about.webp)

:::cards
### License
PoM is licensed under the MIT License.
### Privacy and security (LGPD)
PoM moves prompts and model data between machines and cannot guarantee privacy on public networks or with unknown computers. You decide what data to send and which computers to trust.
### Libraries
Everything a PoM build ships - the interface packages, the Rust crates (transitive ones included) and the native libraries compiled in - grouped by license.
### PoM update
Checks the PoM releases on GitHub (the website as fallback) and applies one only after you authorize it; the node restarts on the new version. It needs no license.
:::

:::deep docs/subsystems/enterprise-licensing.md
How PoM updates itself
:::
