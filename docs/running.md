# Starting the app

## The short version

```bash
cd ~/Projects/mikrotik-dashboard
./scripts/run.sh              # against the router in ~/.config/mikdash/api.env
./scripts/run.sh --demo       # simulated router, no hardware needed
./scripts/run.sh --x11        # force XWayland instead of Wayland
```

`scripts/run.sh` is a thin wrapper: it resolves the session environment, reads the API
password out of the config file into the environment, and `exec`s the binary. It also
installs as a desktop entry (see below), which is how the window is normally started.

## Wayland by default

The app is built with slint's winit backend, which is **Wayland-first**: when
`WAYLAND_DISPLAY` (or `WAYLAND_SOCKET`) is set and the socket exists, it uses Wayland;
otherwise it falls back to X11, which on a Wayland desktop means XWayland.

That choice is made by the *environment*, not by the binary — winit 0.30 removed the
`WINIT_UNIX_BACKEND` override, and slint sets no platform preference of its own
(`i-slint-backend-winit/src/lib.rs`, `EventLoopBuilder`). The consequence worth knowing:
a process started from a context that has no session variables — a systemd unit, a cron
job, an `ssh` session, an exec from another service — lands on XWayland even on a
Wayland desktop. `scripts/run.sh` exists to remove that trap: it fills in
`XDG_RUNTIME_DIR` and finds the `wayland-N` socket before launching, and it prints which
backend it chose:

```text
backend: Wayland (/run/user/1000/wayland-0)
```

Two checks, if you ever doubt which one you are on:

```bash
ss -xap | grep wayland          # an established unix socket to wayland-0 = Wayland
xlsclients 2>/dev/null | grep -i dashboard   # listed = XWayland client
```

Fixing a launch that landed on XWayland is therefore an environment fix, not a rebuild:

```bash
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR
# or, per unit:
Environment=WAYLAND_DISPLAY=wayland-0
Environment=XDG_RUNTIME_DIR=/run/user/1000
```

## Desktop entry

`packaging/mikrotik-dashboard.desktop` starts the app from the application menu:

```bash
install -Dm644 packaging/mikrotik-dashboard.desktop \
    ~/.local/share/applications/mikrotik-dashboard.desktop
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

Its `Exec=` line points at `scripts/run.sh`, so the menu entry gets the same backend
resolution and credential handling as the shell.

## Credentials

One mode-600 file, no secrets in argv and none in the repo:

```bash
install -m 600 /dev/null ~/.config/mikdash/api.env
cat >> ~/.config/mikdash/api.env <<'EOF'
MIKDASH_PASSWORD=<the API password>
MIKDASH_TARGET=192.0.2.1:8728
MIKDASH_USER=mikdash
EOF
```

`scripts/run.sh` sources that file, so `--target`/`--user`/`--password-env` do not have
to be repeated. A secret passed as `--password` lands in the process list and in the
shell history; the file exists so that never has to happen.

## Choosing the interface

`--interface ether5` makes both traffic cards follow that interface; without it they
follow the detected uplink. A name the router does not have is reported and the uplink is
charted instead.

## The window itself

The surface is a fixed 1920x1080. Under a compositor that will not honour that size the
window is simply scaled or tiled like any other client; there is no fullscreen mode, by
design — the layout is a grid sized for exactly one screen.
