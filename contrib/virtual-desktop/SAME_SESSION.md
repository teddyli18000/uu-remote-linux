# Same-session display on Ubuntu 18.04

The same-session route keeps the existing GNOME Xorg server, desktop, windows, and application
processes. It does not migrate applications to Xvfb. The current development path is a runtime
force-on request for one already-existing i915 HDMI connector. The temporary route has passed on the Ubuntu 18.04 / Linux 5.4.0-150 / i915 target: with HDMI unplugged, the official UU client showed the complete original GNOME desktop and accepted window dragging and keyboard input at 1024x768. New-version automatic unplug takeover, persistence across host restart, and monitorless cold boot still require acceptance.

## Scope and limits

No driver is installed, replaced, or switched, and no Xorg driver configuration or GDM setting is
changed. Installing the helper does not require a reboot or graphical-session restart. For a
monitorless cold boot, the optional [systemd oneshot](uuremote-display-helper.service) can run
`ensure` before `display-manager.service`; it forces only the connector and does not create a GNOME
user session. GDM must already be set up to start the intended user's Xorg session automatically.
This feature does not enable automatic login or change GDM. Without that existing session, the
remote host cannot preserve or serve the original desktop.

On first activation, the helper requires exactly one i915 DRM card and every physical connector to
have a known `disconnected` status. It selects an existing HDMI-A connector on that card; if the
card exposes several HDMI-A ports, it deterministically selects the lowest numeric port. A
connected connector, an unknown status, multiple candidate cards, or an unavailable helper must
not be treated as permission to force an output. Later `ensure` calls use the helper's recorded
connector instead of selecting another port. DisplayPort is never selected: forcing a DP
connector does not provide the AUX/link-training path needed to establish a usable link without a
sink.
Machines without an eligible HDMI connector have no same-session automatic output through this
path; use the explicitly separate Xvfb mode only when losing the original GUI applications is
acceptable.

Linux 5.4 exposes connector `status` as a writable DRM sysfs attribute. Its `status_store` accepts
`on`, `on-digital`, `off`, and `detect`; `on` requests a forced-connected state for an existing
connector, while `detect` returns it to normal detection. See the [Linux v5.4 DRM sysfs source](https://github.com/torvalds/linux/blob/v5.4/drivers/gpu/drm/drm_sysfs.c#L101-L139)
and the [status attribute declaration](https://github.com/torvalds/linux/blob/v5.4/drivers/gpu/drm/drm_sysfs.c#L224).
This changes kernel connector state at runtime; it does not create a DRM connector or guarantee
that a sinkless HDMI port will scan out a usable mode. Actual rendering, GNOME repaint, and capture
must be verified on the target hardware.

RandR screen geometry (`xrandr --fb`) and RandR 1.5 logical monitors (`xrandr --setmonitor`, where
the client and server expose that version) cannot create a DRM connector or a hardware output. The
[Ubuntu 18.04 xrandr manual](https://manpages.ubuntu.com/manpages/bionic/man1/xrandr.1.html)
describes `--fb` as screen-size configuration and `--newmode`/`--addmode` as attaching a mode to
an output. The [RandR 1.5 xrandr manual](https://manpages.ubuntu.com/manpages/jammy/man1/xrandr.1.html)
describes `--setmonitor` as logical monitor geometry and output associations; it is not a
substitute for an active connector. The older Bionic manual's omission of `--setmonitor` does not
by itself establish whether the installed xrandr client implements it.

## Privileged helper

Use the helper binary bundled in the archive for your Ubuntu version. From the extracted archive:

```sh
sudo bash contrib/virtual-desktop/uu-display-setup install "$USER" --boot
```

Omit `--boot` if only runtime unplug takeover is needed. The installer verifies that the selected
service user's primary group is not shared with another account, then atomically installs
`/usr/local/libexec/uuremote-display-helper` as root:group, mode `4750`. `--boot` also installs and
enables the optional system oneshot for the next boot. Installation does not restart UU, the desktop,
or the machine, and does not force an output immediately. No driver or display-manager configuration
is changed. The network-facing host remains an ordinary user process.

Keep the UU user service on the original GNOME `DISPLAY` and `XAUTHORITY`. Remove an obsolete explicit
`UU_VIRTUAL_DISPLAY_MODE=xvfb` selection or `DISPLAY=:99` override when migrating from aic.3; verify
the actual original-session values first. The installer does not edit the UU service or credentials.

The helper keeps its ownership record under `/run/uuremote-display-helper`. A force remains in effect
until explicitly released or rebooted; the optional boot oneshot rechecks it on each boot.

The helper interface is:

```sh
/usr/local/libexec/uuremote-display-helper ensure
/usr/local/libexec/uuremote-display-helper status
/usr/local/libexec/uuremote-display-helper release
```

`ensure` may force the eligible connector on and prints its DRM connector name (for example,
`card0-HDMI-A-1`) when a connector is recorded. Repeated calls report the recorded connector
without choosing another port. `status` reports that record to its owner. A safe no-op has no
output. A stale recorded connector or one no longer confirmed as belonging to i915 is an error;
the state is preserved and no replacement port is selected. A normal caller cannot inspect or
adopt a record created for another non-root user. The optional boot oneshot creates a root-owned
record; authorized helper-group callers may use `ensure` and `status` with that record, but only
root may release it. Root may inspect or release any recorded connector for recovery. `release`
is an explicit rollback: it returns only the connector recorded by the helper to kernel `detect`
mode, then clears that record. The helper must not select DP or guess when connector state is
unknown.

The boot oneshot runs before `display-manager.service` and has no stop action. It prepares a
connector, then GDM uses its existing setup to start the original GNOME Xorg user session; this
feature does not enable automatic login.

Explicit rollback, when returning to normal connector detection is appropriate:

```sh
sudo systemctl disable uuremote-display-helper.service  # if installed
sudo /usr/local/libexec/uuremote-display-helper release
```

To remove the installed helper and optional unit entirely, stop the UU service first and run
`sudo bash contrib/virtual-desktop/uu-display-setup uninstall "$USER" --boot` from its package.
The script refuses to remove a changed unit or a helper belonging to another service group.

The forced state is intentionally persistent. Closing a viewer, reconnecting, replugging a
physical display, or stopping the host does not automatically release the HDMI connector. This
keeps the Xorg output available across those events. Run `status` before any manual rollback, and
use `release` only when it is appropriate to return the connector to automatic kernel detection.
The behavior is scoped to one existing connector, but no zero-impact guarantee is made: forcing
its reported state can affect the original Xorg desktop layout or hardware-specific display
behavior.

## Acceptance

The target experiment above used a temporary runtime override with the existing released host, not the new automatic implementation. A system-level recovery task restored the experiment independently of the UU process. It proves the existing-kernel route is usable on this target; it does not cover automatic takeover or cold boot.

The Xorg dummy CI fixture checks host-side state handling, capture, and original-session identity
against a synthetic output. It does not prove i915 HDMI scanout, HDMI link behavior with no sink,
GNOME repaint after physical unplug, monitorless GDM startup, or vehicle acceptance.

On a disposable or otherwise approved Ubuntu 18.04 target, verify in the original GNOME Xorg
session before relying on the feature:

1. Check the installed helper's owner, group, mode, selected DRM card, and connector inventory.
   Record only connector names and status values. Confirm all physical connectors report known
   disconnected status before first activation. Confirm the helper picks the lowest-numbered
   HDMI-A port on the unique i915 card, and repeated `ensure` calls keep using its record. Confirm
   that DP, multiple-card, and unknown-status cases do not activate an output.
2. Open a harmless GUI application. Record its Xorg PID, `DISPLAY`, root-window ID, application PID,
   and window ID locally with `xprop`; do not capture titles or application contents in logs.
3. Invoke `ensure`, verify the recorded connector and RandR output, then confirm remote capture
   shows the same application in the same Xorg session after the last physical display is unplugged.
   Check that the application PID and window ID survive the hot-unplug transition.
4. Keep the remote viewer connected through physical unplug and confirm the session continues
   showing the same application without a user reconnect. Close the viewer and confirm no desktop
   encoder/stream remains active while idle; then reconnect as an intentional new viewing session.
   Replug a monitor and confirm the forced HDMI output remains available.
5. Stop the host and verify the output remains forced on. Check `status`, then exercise the explicit
   `release` rollback and confirm the connector returns to automatic detection.
6. Separately cold-boot with physical displays disconnected and the optional oneshot enabled.
   Verify the helper ran before the display manager, then verify GDM starts the intended GNOME Xorg
   user session using the existing automatic-login setup. The helper does not configure that
   session. Process IDs change across reboot, so process/window continuity is only meaningful for
   the hot-unplug test.

The aic.4 release remains unpublished until the helper implementation and target behavior have
been reviewed and validated. Dummy CI results and source-level feasibility are not hardware
acceptance.
