# POM for macOS - Installation

This package contains `POM.app` for Macs with Apple Silicon (M1 or newer) running macOS 11 or later.

The app is not notarized by Apple, so macOS asks you to authorize it the first time you open it. The steps below explain how.

## 1. Extract the package

Double-click the `.zip` file. It creates a folder containing `POM.app`.

## 2. Move the app to the Applications folder

Drag `POM.app` into the **Applications** folder.

Open it from there, not from the Downloads folder. When the app runs from Downloads, macOS can hang on the first launch without showing any window.

## 3. Authorize the app in Gatekeeper

The first time, macOS shows the message *"Apple could not verify that POM is free of malware"*. Click **OK** or **Done**.

Then:

1. Open **System Settings** > **Privacy & Security**.
2. Scroll to the security section. A message about POM appears with an **Open Anyway** button.
3. Click **Open Anyway** and confirm with your Mac password.
4. Open POM again, from Launchpad or from the Applications folder, and click **Open**.

On older versions of macOS, you can also Control-click `POM.app` and choose **Open** twice.

## 4. Allow local network and internet access

The first time POM runs, macOS asks whether it can **find and connect to devices on your local network**. Click **Allow**.

POM uses the local network to find the other nodes of the cluster, and the internet to connect to remote nodes and relays. Internet access needs no separate permission. Local network access does, and without it POM cannot find the other computers.

If you denied it by mistake, enable it later in **System Settings** > **Privacy & Security** > **Local Network** and turn on **POM**.

If macOS asks whether POM can **accept incoming network connections** (firewall), click **Allow**. Other nodes need to be able to connect to this Mac.

## 5. Use POM

While POM is running, the web interface is at:

<http://localhost:8080/admin-ui/>

The POM icon also appears in the menu bar.

The node's data (the `pom.json` configuration, the database, keys and models) is stored in the `~/.pom` folder inside your user folder.

## Common problems

- **The app does not open and nothing appears:** check that it is in the Applications folder and that the authorization in step 3 was done. If it is still stuck, restart the Mac and try again.
- **POM opens but does not find other nodes:** check the Local Network permission (step 4).
- **To stop POM:** click the menu bar icon and choose quit, or run `pkill -f "POM.app/Contents/MacOS/pom"` in Terminal.
