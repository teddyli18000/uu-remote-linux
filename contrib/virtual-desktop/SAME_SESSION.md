# Ubuntu 18.04 same-session virtual output

The default X11 path keeps using the existing GNOME Xorg display. When the last physical monitor
is gone, the host enables an Intel `VIRTUAL1` RandR output on that same display. It does not create
another X server, copy windows to a new desktop, or run the network-facing host as root.

This requires an Intel i915 DRM device, the Intel Xorg DDX (`intel_drv.so` from
`xserver-xorg-video-intel`), and a GNOME Xorg session. The Xorg `OutputClass` section matches the
kernel driver and passes options to the matching device; the Intel driver documents `VirtualHeads`
as extra RandR outputs sharing the regular framebuffer. See [`xorg.conf(5)`](https://manpages.ubuntu.com/manpages/jammy/man5/xorg.conf.5.html)
and the [upstream Intel driver manual](https://cgit.freedesktop.org/xorg/driver/xf86-video-intel/tree/man/intel.man?id=7538be3315b8683b05e8f6b22023baadcc0bc4da).

## Configure VirtualHeads

Run `check` as the user who owns the current graphical session. It prints only system status,
driver names, and counts; it does not read or print window titles or application contents.

```sh
./uu-same-session-setup check
```

If `intel_ddx=no`, install the packages manually, then run the check again. The setup command never invokes apt:

```sh
sudo apt install xserver-xorg-video-intel x11-xserver-utils x11-utils
```

These provide the Intel Xorg DDX, `xrandr`/`cvt`, and `xprop` used for local acceptance checks.

Review the exact Xorg configuration before installing it:

```sh
./uu-same-session-setup install --dry-run
```

Install or remove the managed configuration with root:

```sh
sudo ./uu-same-session-setup install
sudo ./uu-same-session-setup remove
```

The installer writes `/etc/X11/xorg.conf.d/30-uu-virtualheads.conf` with `MatchDriver "i915"`,
`Driver "intel"`, SNA acceleration, and one virtual head. It refuses to overwrite an existing
file. Removal succeeds only when the file still exactly matches the configuration this tool
generated. Neither command installs packages, restarts Xorg, changes GDM or automatic login, nor
grants privileges to `uuremote-server`.

The change takes effect after the next logout/login or reboot. That first Xorg restart closes the
currently open applications and windows. Plan for that one-time session loss. After the
configuration has loaded, a physical monitor unplug only changes outputs inside the existing Xorg
session; application processes and window IDs should remain on that display. Confirm this on the
target machine before relying on it.

## Startup and runtime settings

Headless boot requires the original GNOME Xorg login session to exist. `VirtualHeads` does not
create a GNOME session or configure GDM automatic login. If GDM does not start the intended user
session without a connected monitor, this feature cannot provide that session by itself.

The host uses same-session mode by default. Set `UU_VIRTUAL_DISPLAY_MODE=xvfb` only to select the
legacy separate Xvfb session; that session cannot inherit arbitrary applications already open on
the physical desktop. Set `UU_AUTO_VIRTUAL_DISPLAY=0` to disable automatic display handling.
`UU_VIRTUAL_SIZE` selects the virtual output size and defaults to `1920x1080`.

When `VIRTUAL1` is missing or cannot be activated, the host reports an actionable error and does
not silently start a second X server. Desktop encoding remains demand-driven: with no remote desktop
viewer, no desktop stream should run. Once the virtual output has been enabled, replugging a
physical monitor does not remove it.

## Acceptance

The CI same-session fixture exercises RandR activation against a dummy output on an existing X
session and checks that the original window and process survive the simulated transition. This
proves the host-side same-session path. It does not prove Intel DDX/SNA hardware rendering, i915
connector hotplug behavior, cold boot with no monitor, the vehicle's GNOME session, or
official-client acceptance.

On the Ubuntu 18.04 target, verify the remaining hardware behavior in the original user session:

1. Install the configuration, then log out and back in or reboot. Confirm `check` reports an i915
   device, the Intel DDX, and at least one `VIRTUAL` output.
2. Open a harmless application in the original GNOME session. Record its window ID and
   `_NET_WM_PID` locally with `xprop`; do not include window titles or contents in logs.
3. Connect the remote desktop and confirm it shows that application. Unplug the last physical
   display and confirm the same application PID and window ID remain on the same `DISPLAY` and stay
   visible remotely.
4. Close the remote viewer. Confirm no desktop stream or encoder remains active while idle.
   Reconnect and confirm the same desktop is visible.
5. Replug the physical display and confirm the virtual output remains available. Separately cold
   boot with the physical monitor disconnected and verify that GDM creates the intended GNOME Xorg
   user session before expecting a desktop to be served. A reboot creates a new process, so
   PID/window continuity applies to hot unplug/replug, not across reboot.
