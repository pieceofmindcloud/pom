---
id: screen-transfers
label: Downloads
summary: Follow the models each machine is downloading
title: Downloads
route: /transfers
order: 90
---

:::lead
Downloads shows the models each machine is downloading from Hugging Face: this machine's, and those you started on other machines from the deploy screen or the Hugging Face screen. Files are no longer sent between machines: each machine serves from its own copy of the model.
:::

:::steps
### Follow
Each download shows bytes, speed and the time left. **Cancel** stops an unfinished one, **Try again** restarts a failed one and **Dismiss** takes it off the list.
### Other machines
A panel per machine, **Downloads on** that machine, lists what it is downloading; its buttons act on that machine.
### A file placed by hand
Copy it into one of the machine's model folders and press **Rescan** (on the Storage screen for this machine, on the machine's card in the deploy screen for another one).
:::

:::note Transfers from an older admin
An admin still on an older version may offer a file to this machine. The offer and its progress still show here for one more release, and the file still never crosses a public relay.
:::

:::deep docs/subsystems/file-transfer.md
The retired transfers and what replaced them
:::
