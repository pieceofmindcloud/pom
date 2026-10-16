# POM for Windows - Installation

This package runs POM on 64-bit Windows 10 or Windows 11.

There are two packages:

| Package | For |
|---|---|
| `pom-<version>-x86_64-pc-windows-msvc.zip` | Any PC (CPU only) |
| `pom-<version>-x86_64-pc-windows-msvc-cuda-vulkan.zip` | PCs with a GPU: NVIDIA (CUDA) or AMD and Intel (Vulkan) |

If you are not sure, use the CPU package. It works everywhere.

## What you need

- Windows 10 (64-bit) or Windows 11.
- **Microsoft Edge WebView2 Runtime.** Windows 11 and most Windows 10 installations already have it. If POM opens with a blank window or does not open, install it from <https://developer.microsoft.com/microsoft-edge/webview2/>.
- For the GPU package: an NVIDIA driver (for CUDA) or a recent video driver with Vulkan support (for AMD and Intel).

## 1. Download and extract

Download the package for your PC and extract it:

1. Right-click the `.zip` file and choose **Extract All...**, or open it with 7-Zip.
2. Choose a folder that you can write to, such as `C:\Users\<you>\POM`. Avoid `Program Files`.

Keep the whole folder together. It holds `pom.exe`, the files POM needs to run, and your data (see step 5). Do not run POM from inside the zip file.

## 2. Start POM

Double-click `pom.exe`.

The first time, Windows SmartScreen may show *"Windows protected your PC"*, because the program is not signed by a publisher Windows recognizes. Click **More info**, then **Run anyway**.

POM opens its window and puts an icon in the notification area (next to the clock).

## 3. Allow the firewall

POM needs to receive connections from the other nodes of the cluster.

- If Windows shows a **User Account Control** prompt asking whether POM can change the firewall rules, click **Yes**. POM then adds the rules for the ports in the table below.
- If Windows Defender Firewall shows *"Windows Defender Firewall has blocked some features of this app"*, allow it on **Private networks**. Allow **Public networks** only if you trust the network you are on.

If you click **No** on the UAC prompt, POM still runs, but other nodes may not be able to connect to this PC.

| Port | Protocol | Used for |
|---|---|---|
| 42777 | UDP | Discovery of nodes on the local network |
| 43777 | UDP | Inference traffic |
| 44777 | UDP | Gateway traffic |
| 52777 | TCP | Peer connections (libp2p) |

The ports follow the discovery port, 42777. If you change it, the rules change with it.

## 4. Open the interface

The POM window shows the web interface. You can also open it in any browser:

<http://localhost:8080/admin-ui/>

Closing the window hides it in the notification area, and POM keeps running. Click the icon there to show the window again. To stop POM, right-click the icon and choose **Quit**.

## 5. Where your data lives

POM keeps its data in the folder that contains `pom.exe`:

- `pom.json`: the configuration
- `pom.db`: the local database
- the node keys
- `models/`: the models you deploy

Back up this folder to keep your node. To start over, quit POM and delete the files above.

## Problems

- **The window stays blank or does not open:** install the WebView2 Runtime (see *What you need*).
- **POM runs but does not find other nodes:** check the firewall (step 3) and that the network is set to **Private**.
- **The GPU is not used:** update the driver. For NVIDIA use the CUDA package; for AMD or Intel use the same package, which includes Vulkan.

## Uninstall

Quit POM from the notification area, then delete the folder you extracted it to.
