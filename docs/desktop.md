# The desktop

A graphical session holds VRAM on the card it runs on: the compositor, Xwayland, the shell.
On a machine that is mostly used remotely, that VRAM is better spent on a model or a job.
With `[desktop]` gpuq frees it while something needs it and gives the desktop back after.

It is off by default. Three policies:

| `policy` | What a release does | What a restore does |
|---|---|---|
| `off` | nothing (default) | nothing |
| `stop` | stops `user_units`, then `system_units` (the display manager) | starts `system_units` again |
| `offload` | restarts the desktop pointed at another card | moves it back, if asked to |

Configuration keys are in [config.md](config.md#desktop); an example is
[examples/desktop.toml](../examples/desktop.toml).

## When gpuq releases the desktop

Any of these, if it lasts `release_delay_s` (30 s):

- an LLM model matching `models` is loaded on the card;
- an LLM model is loaded and less than `min_free_mib` is free. This reason lasts as long as
  that model stays loaded, so the VRAM freed by the release does not end it at once;
- a job waits for the card and declares more VRAM than is free (`leases = true`). When the
  arbiter finds that a job does not fit on an otherwise idle card, it asks for a release at
  once, without the delay.

Never while someone is logged in at the seat (`seat`, a logind session of class `user`, not a
greeter), and never when logind cannot be asked. With `wake_on_input`, also not while a key
was pressed in the last `input_idle_s`. After a restore asked for by hand or by a key press,
not for `hold_s` (30 min).

`gpuq desktop release` releases at once. It still refuses with someone at the seat;
`--force` releases anyway.

## When gpuq gives it back

When no reason to release has been seen for `restore_delay_s` (10 min) and no lease holds the
card, gpuq starts the display manager again. Also when a key is pressed at the seat
(`wake_on_input`), or on `gpuq desktop restore`, which holds off releases for `hold_s` (or
`--hold SECS`).

gpuq only starts what it stopped itself. A desktop that was already stopped when gpuq started,
or that someone stopped by hand, stays stopped until `gpuq desktop restore`. A desktop that
someone starts by hand is adopted as running. gpuq remembers across restarts that it stopped
the desktop (`state_file`).

With a user logged in on a text console of the seat, the display manager is not started:
starting it could take that console.

## The states

```text
  active ──(release)──> releasing ──(ok)──> released ──(restore)──> restoring ──(ok)──> active
    ^                       │                                            │
    └────(failed, retry_s)──┘                    released <──(failed, retry_s)
```

gpuq polls every `poll_s` (5 s): the units (`systemctl show`, no privileges needed), the
sessions on the seat (`loginctl`), and what gpuq itself runs on the card. It acts only on a
transition, and logs only when what it waits for changes. A released desktop is never stopped
again, and a change made outside gpuq is adopted only after it lasted two polls.

## Who holds the card

The card's DRM nodes come from its PCI address (`/sys/bus/pci/devices/<addr>/drm`): the
display node (`/dev/dri/cardN`) and the render node (`/dev/dri/renderDN`). gpuq looks through
`/proc/*/fd` for processes that have one of them open and reads their resident VRAM from
`/proc/*/fdinfo`. A process counts as the desktop when it is not in `ignore` and either runs in
the cgroup of one of the configured units or has the display node open (it drives a screen).

Everything else is left alone. A headless compositor used by browser automation, for example
`kwin_wayland --virtual`, only opens a render node and runs in its own unit, so it is never
the desktop; it can also be listed in `ignore` to be sure. gpuq never kills a process: it only
stops and starts the units it is given.

Without root, gpuq sees only its own user's processes, and not those started with file
capabilities (`kwin_wayland` has one). With `elevate` set, it then runs the scan as
`<elevate> gpuq scan-drm --pci <addr>` and uses that; otherwise `gpuq desktop` says how many
processes it could not read. The units are checked with systemd either way.

## Stop

1. Note the desktop's processes on the card and the VRAM in use.
2. Stop the `user_units` that run, then the `system_units` that run. User units go first:
   they outlive the display manager when the user manager stays up (linger, SSH sessions).
3. Wait up to `stop_timeout_s` for every unit to be down and every desktop process to be gone
   from the card.
4. Set `session_env` in the user manager and in the D-Bus activation environment, then clear
   failed user units. The session left `DISPLAY` and `WAYLAND_DISPLAY` behind; without this,
   every D-Bus activated service (portals, secret service, notifications) starts against a
   display that is gone and crashes. The default removes both variables; pointing
   `WAYLAND_DISPLAY` at a headless compositor that is always up works too. A `WAYLAND_DISPLAY`
   whose socket does not exist is removed instead.
5. Measure the VRAM again and report the difference.

If a stop command fails or something is still there after `stop_timeout_s`, gpuq starts the
system units that were running again, waits for them, and tries again after `retry_s`.

## Offload

For machines whose monitors are on another card (often the integrated GPU), the desktop can
keep running there.

1. If monitors are connected to this card, or to no other card (`/sys/class/drm/*/status`),
   offload cannot work. gpuq says so once and uses `fallback` from then on.
2. Set `offload.user_env` in the user manager and `offload.system_env` in the system manager,
   and run `offload.commands`. For KWin, `KWIN_DRM_DEVICES` names the card to use; for other
   compositors use their setting, or a command that writes a drop-in.
3. Stop the `user_units`, restart the `system_units`, wait `offload.settle_s` for the session.
4. Check that no desktop process holds this card any more. If one still does, the environment
   is removed, gpuq logs which process and node, and uses `fallback` (`stop` stops the desktop
   right away; `off` restarts it as it was).

A moved desktop stays where it is until `gpuq desktop restore`, or on its own once nothing
needs the VRAM if `offload.move_back = true`. Moving back removes the environment, runs
`offload.undo_commands` and restarts the units.

Some setups cannot be moved by an environment variable. An X11 display manager greeter, for
example, runs Xorg, which opens the display node of every card. There `offload` finds Xorg
still on the card and falls back; switching the greeter to Wayland, or an Xorg config that
names one card, makes it work.

## Checking the journal

After each change, `check_delay_s` (20 s) later, gpuq reads the system and user journals since
the change began for messages at priority `err` or worse, lists core dumps (`coredumpctl`)
and units that failed since. The result is logged and shown in `gpuq desktop` and
`gpuq status`. The system journal and core dumps are read with `elevate` when gpuq is not root;
what could not be read is listed as unchecked.

## Permissions

Stopping and starting a system unit needs root, a polkit rule that allows
`org.freedesktop.systemd1.manage-units`, or `elevate = ["sudo", "-n"]` with a sudoers entry.
User units are handled in the user manager of `user`: gpuq running as that user (with
`XDG_RUNTIME_DIR` set) talks to it directly, gpuq running as root uses
`systemctl --user --machine=<user>@.host`. `wake_on_input` needs read access to the input
devices (the `input` group); only the time of the last key press is kept.

## Commands

```
gpuq desktop                   state, reasons, units, processes on the card, last check
gpuq desktop status --json     the same as JSON (GET /desktop)
gpuq desktop release [--force] release now (POST /desktop/release)
gpuq desktop restore [--hold SECS]
                               give it back now (POST /desktop/restore)
```

`gpuq status` and `GET /status` show the state in one line (`desktop` object).
