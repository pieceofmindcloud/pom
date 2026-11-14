---
id: screen-transfers
label: Transfers
summary: A file an older admin sends to this machine
title: Transfers
route: /transfers
order: 90
---

:::lead
Transfers shows a file an admin still on an older version offers to this machine, and its progress. It is not in the menu: the bell links to it when an offer arrives. Files are no longer sent between machines - each machine serves from its own copy of the model - and the model downloads moved to the Downloads tab of Search Models, the Hugging Face plugin's screen.
:::

:::steps
### Accept or refuse
An offer names the model and the layer range it carries. **Accept** starts it; **Refuse** declines it, which the sender sees as refused, not as a failure.
### Follow
A running transfer shows bytes, speed and the time left; **Cancel** stops it. The finished ones stay listed below.
:::

:::note One more release
These transfers are served for one more release, for admins that have not updated yet, and a file still never crosses a public relay.
:::

:::deep docs/subsystems/file-transfer.md
The retired transfers and what replaced them
:::
