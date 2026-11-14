---
id: getting-started
label: Getting started
summary: From the first machine to a network of two
title: Getting started
order: 20
---

:::lead
A network starts with one administrator machine. The others join by invitation. No model is passed on the command line: everything after the first start happens through this interface.
:::

:::diagram setup
The administrator creates the network and the invitation. The new machine finds the administrator through local discovery, or gets the pasted invitation, and goes from joining to connected.
:::

:::steps
### Start the binary
Run the pom executable with no arguments. Without a configuration file, the node comes up in setup mode and waits for the browser.
### Open the interface
Go to http://<machine>:8080/. The interface and the API always live on the gateway port, which is 8080 by default.
### Pick the role
The first machine picks Administrator. The wizard only asks for the machine name, the network name and whether it is private or public.
### Keep the invitation
On the last step the administrator gets the invitation: the connection key (on a private network) and the trust.json file. The other machines join with them.
### Bring in the second machine
On the other machine, run the same binary, open the interface and pick Node. On the same local network the wizard finds the administrator on its own; otherwise, paste the trust.json and the key.
:::

:::note While the wizard runs, the administrator sees who arrives
On the invitation step, each computer looking for the network shows up with its name, system, IP, CPU, RAM and GPU, and goes from joining to connected when it receives the root. Finishing with nobody is normal too.
:::

:::warn Change the default password
After setup, the panel password is admin and the interface shows a fixed banner until you change it in Preferences. Folders, memory reserves and the thermal limit come from the machine's defaults and can also be adjusted later.
:::

With both machines in the network, the next step is a model: bring a GGUF into the models folder or use Search Models (the Hugging Face plugin, installed from Plugins), then follow the guide From model to serving cluster.

:::note Machines on different networks
Outside the same local network, automatic discovery does not reach the administrator. Start the node with --bootstrap <administrator>:42777 and see the guide Local network and internet.
:::

:::checklist Ready for the next step when
- Both machines show up on the Dashboard.
- The default panel password was changed in Preferences.
- There is a GGUF model in the models folder, or a finished download in Downloads.
:::

:::faq
### Do I need to open router ports?
Not on the same local network: UDP discovery is enough. Across different networks, see the guide Local network and internet, which lists the ports and the advertised address.
### Can I run two nodes on one machine?
Yes, as long as each has its own working directory (POM_HOME) and its own ports, for example --port 42778 --gateway-port 8091 on the second one.
### I lost the invitation. Now what?
On the administrator, Settings > Sharing shows the trust root and the connection key again.
:::

:::deep docs/subsystems/operations.md
Bringing up a two-machine cluster and checking it
:::

:::deep docs/subsystems/web-ui.md
The first access wizard in the interface
:::
