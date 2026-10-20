# Same-session monitorless desktop - paused

Paused at the user's request on 2026-10-21. Preserve this branch for possible future work;
do not continue experiments, install a candidate on the target, or publish aic.4 without a new request.

- Published baseline: v0.2.12-aic.3. Its independent Xvfb desktop can show remote terminal windows,
  but does not inherit the original Ubuntu GNOME desktop or its existing application windows.
- Archived implementation: restricted i915 HDMI runtime-force helper, original Xorg session retention,
  persistent output ownership, scoped video refresh/recovery, optional pre-display-manager oneshot,
  packaging and installation support. No driver installation/switching or Xorg/GDM configuration edits.
- Target manual experiment succeeded: with the monitor unplugged, forcing the existing HDMI output
  produced the original complete GNOME desktop through the official UU client, including mouse and
  keyboard. Independent rollback completed; original Xorg/GNOME processes survived. This used the old
  released host with a temporary runtime override, not the new automatic implementation.
- No new binary or privileged helper was installed on the target. Existing automatic login was
  confirmed by configuration and the active gdm-autologin X11 session.
- Last implementation commit: 6f33ed1. CI run 39442772617 passed five jobs; Ubuntu 18.04 helper tests,
  Rust tests and release compilation passed, but its GNOME acceptance job failed. The cold fixture
  incorrectly reports DUMMY0 connected, so GNOME automatically activates it. Hotplug screenshots
  also show severe horizontal corruption after replug despite satisfying the magenta-pixel check.
- Future work must resolve synthetic connector semantics and compare root geometry/native capture
  against an independent reference. Do not count the current screenshot check as visual acceptance.
- Automatic target hotplug, persistence across host restart, and monitorless cold boot remain untested.
  v0.2.12-aic.4 has no tag or Release. No release should be inferred from this archived branch.

CI evidence: https://github.com/teddyli18000/uu-remote-linux/actions/runs/39442772617
Earlier screenshot evidence remains in ignored local target/ci-39440821486 and
 target/ci-39442772617; these directories are not part of the archived Git snapshot.
