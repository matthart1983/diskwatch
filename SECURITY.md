# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use
[GitHub private vulnerability reporting](https://github.com/matthart1983/diskwatch/security/advisories/new).

You should get an acknowledgement within 72 hours and a triage verdict within 7
days. Please allow time for a fix before public disclosure. Reports are credited
in the advisory and release notes unless you prefer otherwise.

## Supported versions

diskwatch is pre-1.0. Fixes target the latest release only, so please reproduce
on it before reporting.

## What matters most

diskwatch is read-only, but it's often run as root to read SMART data, and it
displays names that other users on the machine control.

1. Terminal escape injection from file, mount, device, volume or model names,
   in the TUI or in `--diag` output.
2. Anything that lets an unprivileged local user use diskwatch's privileges,
   including through the programs it runs (`smartctl`, `zpool`, the LVM tools).
3. Writes to anything other than its own config file.
4. Crashes, hangs or unbounded memory use caused by a crafted name, mount or
   device.

diskwatch does not elevate itself. A user who already has root on the machine is
outside this policy.
