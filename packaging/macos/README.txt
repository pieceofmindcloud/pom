POM for macOS - Installation
============================

This disk image contains POM.app for Apple Silicon and Intel Macs running
macOS 11 or later.

1. Install the application
--------------------------

Drag POM.app onto the Applications shortcut in this window. Open POM from the
Applications folder, not from the disk image or Downloads.

2. Authorize the application in Gatekeeper
-------------------------------------------

POM is not notarized by Apple yet. The first time you open it, macOS may say
that Apple could not verify that POM is free of malware. Click OK or Done.

Then:

1. Open System Settings > Privacy & Security.
2. Scroll to the security section.
3. Find the message about POM and click Open Anyway.
4. Confirm with your Mac password.
5. Open POM again from Applications and click Open.

On older macOS versions, you can Control-click POM.app in Applications, choose
Open, and confirm Open again.

If a POM process appears in Activity Monitor but no window appears during the
first launch, quit that process before completing the Open Anyway steps above.

3. Allow network access
-----------------------

The first time POM starts, macOS asks whether it can find and connect to devices
on your local network. Click Allow. POM needs this permission to discover other
machines in the cluster.

If you denied it, open System Settings > Privacy & Security > Local Network and
enable POM.

If the firewall asks whether POM can accept incoming network connections, click
Allow. Other nodes need to connect to this Mac.

4. Use POM
----------

POM opens its window and adds its icon to the menu bar. The web interface is
also available through the local gateway, normally at:

https://localhost:8080/admin-ui/

The node's configuration, database, keys and models are stored in the .pom
folder inside your user folder.

Common problems
---------------

POM does not open and nothing appears:
- Confirm that POM.app is in Applications.
- Quit any stuck POM process in Activity Monitor.
- Complete the Gatekeeper authorization in step 2 and open POM again.

POM opens but does not find other nodes:
- Confirm that POM is enabled under Privacy & Security > Local Network.

To stop POM:
- Use Quit from the POM menu bar icon.
- If the interface did not start, quit POM from Activity Monitor.
