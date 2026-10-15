#!/usr/bin/env bash
# Start the dashboard.
#
# Wayland by default: the app is built with slint's winit backend, which prefers
# Wayland whenever WAYLAND_DISPLAY is set and falls back to X11 (XWayland) only when
# it is not. A missing session variable is the usual reason a Wayland desktop app ends
# up on XWayland, so this fills in what a desktop launch would have set.
#
#   scripts/run.sh                          # the router from ~/.config/mikdash/api.env
#   scripts/run.sh --demo                   # simulated router, no hardware
#   scripts/run.sh --x11                    # force XWayland instead
#   scripts/run.sh --screenshot out.ppm --demo --seconds 30
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin=${MIKDASH_BIN:-$here/target/release/mikrotik-dashboard}
config=${MIKDASH_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/mikdash/api.env}
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}

force_x11=0
args=()
for a in "$@"; do
    case $a in
        --x11) force_x11=1 ;;
        *) args+=("$a") ;;
    esac
done

[ -x "$bin" ] || {
    echo "no binary at $bin — build it first: cargo build --release" >&2
    exit 1
}

# ── display backend ─────────────────────────────────────────────────────────
if [ "$force_x11" = 1 ]; then
    unset WAYLAND_DISPLAY WAYLAND_SOCKET
    echo "backend: X11 (XWayland), forced by --x11" >&2
else
    if [ -z "${WAYLAND_DISPLAY:-}" ]; then
        for sock in "$runtime"/wayland-[0-9]*; do
            case $sock in *.lock) continue ;; esac
            [ -S "$sock" ] || continue
            WAYLAND_DISPLAY=${sock##*/}
            break
        done
    fi
    if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -S "$runtime/$WAYLAND_DISPLAY" ]; then
        export WAYLAND_DISPLAY XDG_RUNTIME_DIR="$runtime"
        echo "backend: Wayland ($runtime/$WAYLAND_DISPLAY)" >&2
    else
        echo "backend: X11 — no Wayland socket in $runtime" >&2
    fi
fi

# ── credentials ─────────────────────────────────────────────────────────────
# The password is read here and passed on as an environment variable, never as an
# argument: a password in argv is visible to every process on the box.
if [ -r "$config" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$config"
    set +a
fi

# `--demo` is the only offline mode (`--probe` and `--screenshot` are routinely used
# against a real router), so it is the only one that skips the credentials.
case " ${args[*]-} " in
    *" --demo "*) ;;
    *)
        if [ -z "${MIKDASH_PASSWORD:-}" ]; then
            echo "no MIKDASH_PASSWORD: write it to $config (mode 600), or run with --demo" >&2
            exit 1
        fi
        args+=(--user "${MIKDASH_USER:-mikdash}" --password-env MIKDASH_PASSWORD)
        [ -n "${MIKDASH_TARGET:-}" ] && args+=(--target "$MIKDASH_TARGET")
        ;;
esac

exec "$bin" "${args[@]}"
