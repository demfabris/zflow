# Spike E: pre-login injection (stub)

**Question:** can a root daemon's uinput keyboard, created before the display manager starts, type a password at the GDM/SDDM greeter, and do the virtual devices classify correctly?

**Kill criteria:** failure would gut the kernel-backbone rationale. Expected to pass; too much architecture rides on it to leave untested.

**Plan:**

1. Reuse spike B's receiver, extended with a virtual keyboard (EV_KEY with the full keyboard range), or a standalone script driving uinput.
2. Wrap it in a systemd unit: `WantedBy=multi-user.target`, `Before=gdm.service`, `Type=notify` behavior faked with a readiness sleep for the spike.
3. Reboot. At the greeter, from a second machine (ssh), inject a known key sequence; confirm the greeter's password field receives it.
4. Check classification: `udevadm info` shows ID_INPUT_KEYBOARD / ID_INPUT_MOUSE, `libinput list-devices` lists both, and the devices land on seat0.
5. Repeat at the lock screen and on a VT. RESULT.md records distro, display manager, and any udev property that had to be forced.
