---
id: screen-payments
label: Payments
summary: Charge the visitors that use your network's models
title: Payments
route: /config/payments
order: 285
---

:::lead
Payments is where the network's principal decides whether visitors pay for inference, sets the price of each model, gives credit to the networks its visitors come from, and sees what came in beside what the machines are paid for the work.
:::

:::steps
### Set the prices
Each model gets a price for 1000 input tokens and 1000 output tokens, in credits. A model without a price stays free. A price set on a deployment (Deploy, POM settings) replaces the one here when it goes up.
### Turn charging on
Choose **Charge visitors** and save. Visitors then reach the models only through a member of the network, which charges them. If a visitor runs an older release that cannot do that, the screen lists it first and asks again: it loses access until it updates.
### Give credit
Credit is prepaid. After a visitor's network pays you outside POM, grant its account here, approve its credit request, or type an amount when you approve its admission.
:::

:::note One account per visitor network
All the visitors that come from the same network share one account, named by that network's principal key. The **Visitors** column shows who draws on each one.
:::

:::note When an account runs out
A request is held against the balance before it runs and charged what it actually used when it ends. When the balance cannot cover a request, the visitor gets a refusal saying how much is missing, and its Chat points it to its Credit page.
:::

:::note Income and payouts
**Income and payouts by cycle** lines up what visitors paid in each metering cycle with what the GPU work metering pays the machines for the same cycle, so you can see the network's margin. It needs the metering plugin on this machine.
:::

:::deep docs/subsystems/payments.md
How the charging works, between which machines, and its limits
:::
