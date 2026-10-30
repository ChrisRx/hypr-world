#!/usr/bin/env bash
# A Hyprland of its own, in a window on the desktop, for trying hypr-world
# plugin builds without loading them into the session's compositor: a crash
# takes down that window and nothing else. It is started through the
# session's compositor, so it runs where the app does, with the GPU and the
# session's Wayland socket, on a special workspace of its own.
#
#   nested.sh backend        the nested Hyprland as the app's backend: headless,
#                            no window in the session, the plugin's build
#                            loaded in it, then the app run in the session
#                            against it (`cargo nested`), its output here
#   nested.sh headless       the same without running the app
#   nested.sh up             start, or load if it is running, then it and the
#                            world in view (what dev.sh runs when the session
#                            config loads the plugin)
#   nested.sh start          the nested Hyprland, this plugin's build loaded in
#                            it, and a terminal for the walls
#   nested.sh load           build the plugin and load it into the nested one
#                            again, in place of the build loaded there
#   nested.sh toggle         the world key, in the nested session
#   nested.sh drive [out-dir [steps-file]]
#                            tools/drive.sh against the nested session's app
#   nested.sh run <command>  a command started in the nested Hyprland, on its
#                            active workspace: an app to try on the walls.
#                            A shell command line; quote it whole when an
#                            argument has a space in it
#   nested.sh rofi [mode]    rofi in the session, over the app, starting what
#                            it runs in the nested Hyprland (drun by default):
#                            Super+Ctrl+Space from inside the world
#   nested.sh client [args]  the test client (examples/surfaces.rs), built
#                            and started floating on its active workspace,
#                            its output in $DIR/surfaces.log
#   nested.sh show           its special workspace, into view or out of it
#   nested.sh status         its instance, and the plugin's state in it
#   nested.sh stop           the nested Hyprland, and everything in it
#
# The app it launches keeps its control pipe and log in $DIR rather than
# $XDG_RUNTIME_DIR, so the session's app and the nested one can run at
# once. Nothing here loads, unloads or configures anything in the session's
# own Hyprland: it only asks it to start the nested one, and to show it.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
RUNTIME=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
DIR=$RUNTIME/hypr-world-nested
INSTANCE=$DIR/instance
LOADED=$DIR/loaded
WORKSPACE=hypr-world-nested
# The session's instance, which the nested one must never be taken for.
SESSION=${HYPRLAND_INSTANCE_SIGNATURE:-}
# The system's Hyprland, the one the plugin is built against: it
# refuses to load into any other.
HYPRLAND=$(readlink -f /run/current-system/sw/bin/Hyprland)

die() {
  echo "nested.sh: $*" >&2
  exit 1
}

# The nested instance's signature, if it is running.
running() {
  local sig
  sig=$(cat "$INSTANCE" 2>/dev/null) || return 1
  [[ -n $sig && $sig != "$SESSION" ]] || return 1
  hyprctl -i "$sig" version >/dev/null 2>&1 || return 1
  echo "$sig"
}

# Whether its window in the session is still open. Closed (Super+Q on it is
# the session's bind, not its own), it runs on with only a headless FALLBACK
# output, which nothing shows or types into.
windowed() {
  hyprctl -i "$1" monitors -j | jq -e 'any(.[]; .name | startswith("WAYLAND-"))' >/dev/null
}

# Whether it draws on its headless output (`headless`), the size of the
# session's monitor, and so needs no window in the session.
headless() {
  hyprctl -i "$1" monitors -j | jq -e 'any(.[]; .name == "hypr-world")' >/dev/null
}

# Run by the session's compositor: the nested Hyprland itself. The app its
# plugin launches keeps its pipe and log in $DIR.
compositor() {
  echo $$ >"$DIR/running"
  export HYPR_WORLD_DIR=$DIR
  export HYPRLAND_NO_SD_NOTIFY=1 HYPRLAND_NO_CRASHREPORTER=1
  # For its rofi bind (nested/hyprland.lua): this script, and the session's
  # display, where rofi shows.
  export HYPR_WORLD_NESTED_SH=$HERE/nested.sh HYPR_WORLD_SESSION_DISPLAY=${WAYLAND_DISPLAY:-}
  # The session's monitor, for its headless output's rule (nested/hyprland.lua).
  local mode scale
  read -r mode scale < <(hyprctl monitors -j | jq -r 'first(.[] | select(.focused)) | "\(.width)x\(.height)@60 \(.scale)"')
  export HYPR_WORLD_MODE=$mode HYPR_WORLD_SCALE=$scale

  # On start Hyprland points the session's systemd and D-Bus activation
  # environment at itself (`systemctl --user import-environment`, then
  # `dbus-update-activation-environment`), and on exit takes those
  # variables out of it again (`systemctl --user unset-environment`), and
  # 0.56 does both whatever HYPRLAND_NO_SD_VARS says. Nested, the first
  # sends whatever the session starts through systemd or D-Bus to the
  # nested window, and the second leaves the session without a display.
  # These two, first on its PATH, keep the nested one away from the
  # session's systemd altogether, and it runs on a D-Bus of its own
  # (`dbus-run-session`, below) in case anything reaches for the session's.
  # That bus does get the nested display, without --systemd (the session's):
  # apps it starts, as rofi's drun and single-instance ones such as Ghostty
  # do, open in the nested Hyprland.
  mkdir -p "$DIR/bin"
  cat >"$DIR/bin/systemctl" <<'EOF'
#!/bin/sh
case " $* " in *" --user "*) exit 0 ;; esac
exec /run/current-system/sw/bin/systemctl "$@"
EOF
  cat >"$DIR/bin/dbus-update-activation-environment" <<'EOF'
#!/bin/sh
for arg do
  shift
  [ "$arg" = --systemd ] || set -- "$@" "$arg"
done
exec /run/current-system/sw/bin/dbus-update-activation-environment "$@"
EOF
  chmod +x "$DIR/bin/systemctl" "$DIR/bin/dbus-update-activation-environment"
  export PATH=$DIR/bin:$PATH

  dbus-run-session -- "$HYPRLAND" --config "$HERE/nested/hyprland.lua" >"$DIR/hyprland.out" 2>&1 || true

  # A child it forked to run something can outlive it, stuck before it ran
  # anything on a lock held when it forked: none may stay behind.
  pkill -KILL -f -- "--config $HERE/nested/hyprland[.]lua" || true
  rm -f "$DIR/running"
}

# Run by the nested compositor: the test client, which finds its libraries
# through its RUNPATH, as the app does (flake.nix).
client() {
  exec "$ROOT/target/release/examples/surfaces" "$@" >"$DIR/surfaces.log" 2>&1
}

start() {
  local before sig= dir name deadline
  ! running >/dev/null || die "already running: $(running)"
  [[ ! -e $DIR/running ]] || die "a nested Hyprland is still clearing up ($DIR/running)"
  mkdir -p "$DIR"
  before=$(ls "$RUNTIME/hypr" 2>/dev/null || true)

  hyprctl eval "hl.exec_cmd(\"[workspace special:$WORKSPACE silent] $HERE/nested.sh compositor\")" >/dev/null

  # Its instance is the one that was not there before, once it answers.
  deadline=$((SECONDS + 20))
  while [[ -z $sig ]]; do
    ((SECONDS < deadline)) || die "no nested Hyprland came up, see $DIR/hyprland.out"
    sleep 0.2
    for dir in "$RUNTIME"/hypr/*/; do
      name=$(basename "$dir")
      grep -qxF "$name" <<<"$before" && continue
      [[ $name != "$SESSION" ]] && hyprctl -i "$name" version >/dev/null 2>&1 && sig=$name && break
    done
  done
  echo "$sig" >"$INSTANCE"
  echo "nested Hyprland $sig, on special:$WORKSPACE (nested.sh show)"

  load
  # A window drawn on the GPU, so through dmabufs, that changes all the time.
  hyprctl -i "$sig" eval \
    "hl.exec_cmd(\"alacritty --title hypr-world-test -e sh -c 'while :; do date +%T.%N; sleep 0.2; done'\")" >/dev/null
}

# Plugins named hypr-world loaded in the nested instance.
loaded_count() {
  hyprctl -i "$1" plugin list -j | jq '[.[] | select(.name == "hypr-world")] | length'
}

load() {
  local sig old copy out
  sig=$(running) || die "not running"
  "$HERE/dev.sh" build
  # A fresh copy each time, as dev.sh loads the session's: Hyprland keys
  # plugins by path, and dlopen may hand back an image it has seen.
  copy=$DIR/libhypr-world-$(date +%s%N).so
  cp "$HERE/build/libhypr-world.so" "$copy"
  old=$(cat "$LOADED" 2>/dev/null || true)

  if (($(loaded_count "$sig") > 0)); then
    [[ -n $old ]] || die "hypr-world is loaded in $sig but $LOADED does not say from where"
    out=$(hyprctl -i "$sig" --batch "plugin unload $old ; plugin load $copy" || true)
    [[ ${out%%$'\n'*} == ok ]] || die "could not unload $old (${out%%$'\n'*})"
  else
    hyprctl -i "$sig" plugin load "$copy" >/dev/null || true
  fi
  (($(loaded_count "$sig") == 1)) || die "$copy did not load, see $RUNTIME/hypr/$sig/hyprland.log"

  echo "$copy" >"$LOADED"
  [[ -n $old && $old == "$DIR"/* ]] && rm -f "$old"
  echo "hypr-world loaded from $copy"
}

# Whether its special workspace is in view in the session.
shown() {
  hyprctl monitors -j | jq -e --arg ws "special:$WORKSPACE" 'any(.[]; .specialWorkspace.name == $ws)' >/dev/null
}

show() {
  hyprctl dispatch "hl.dsp.workspace.toggle_special(\"$WORKSPACE\")" >/dev/null
}

# The nested Hyprland with no window in the session: drawing on a headless
# output the size of the session's monitor, the plugin's newest build loaded,
# and a shell to type into beside the test terminal.
to_headless() {
  local sig name deadline
  if sig=$(running); then load; else start; fi
  sig=$(running)
  if ! headless "$sig"; then
    hyprctl -i "$sig" output create headless hypr-world >/dev/null
    deadline=$((SECONDS + 5))
    until headless "$sig"; do
      ((SECONDS < deadline)) || die "no headless output in $sig, see $RUNTIME/hypr/$sig/hyprland.log"
      sleep 0.2
    done
    for name in $(hyprctl -i "$sig" monitors -j | jq -r '.[] | select(.name | startswith("WAYLAND-")) | .name'); do
      hyprctl -i "$sig" output remove "$name" >/dev/null
    done
  fi
  if ! hyprctl -i "$sig" clients -j | jq -e 'any(.[]; .title == "hypr-world-shell")' >/dev/null; then
    hyprctl -i "$sig" eval 'hl.exec_cmd("alacritty --title hypr-world-shell")' >/dev/null
  fi
}

# The app in the session, against the nested Hyprland, until it quits.
backend() {
  to_headless
  cd "$ROOT"
  HYPR_WORLD_DIR=$DIR exec direnv exec . cargo run --release -- --backend "$(running)"
}

# rofi on the session's display, launching into the nested Hyprland. From
# the nested one's own bind as well, so its instance is read from $INSTANCE
# rather than checked against the session's (`running`). drun starts apps
# itself, past -run-command, so rofi runs with the nested Hyprland's
# environment for them to inherit, and is handed its own connection to the
# session's display as an open socket (WAYLAND_SOCKET), which libwayland
# takes out of the environment once it has connected.
rofi_into() {
  local sig display session pid bus
  sig=$(cat "$INSTANCE" 2>/dev/null) || die "not running"
  display=$(sed -n 2p "$RUNTIME/hypr/$sig/hyprland.lock" 2>/dev/null)
  [[ -n $display ]] || die "no Wayland socket for $sig"
  # Its D-Bus, for apps rofi starts through it and single-instance ones,
  # which would find the session's copy on the session's bus.
  pid=$(sed -n 1p "$RUNTIME/hypr/$sig/hyprland.lock")
  bus=$(tr '\0' '\n' <"/proc/$pid/environ" | sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p')
  session=${HYPR_WORLD_SESSION_DISPLAY:-${WAYLAND_DISPLAY:-}}
  [[ -n $session ]] || die "no session display"
  [[ $session == /* ]] || session=$RUNTIME/$session
  WAYLAND_DISPLAY=$display HYPRLAND_INSTANCE_SIGNATURE=$sig DBUS_SESSION_BUS_ADDRESS=${bus:-${DBUS_SESSION_BUS_ADDRESS:-}} \
    exec python3 -I -c '
import os, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
fd = s.detach()
os.set_inheritable(fd, True)
os.environ["WAYLAND_SOCKET"] = str(fd)
os.execvp(sys.argv[2], sys.argv[2:])
' "$session" rofi -show "${1:-drun}" "${@:2}"
}

# This tree's build in front of you: the nested Hyprland started, or the
# build loaded into the one running, then it and the world in view.
up() {
  local sig
  if sig=$(running) && ! windowed "$sig" && ! headless "$sig"; then
    echo "nested.sh: its window was closed, starting it again"
    stop
  fi
  if running >/dev/null; then load; else start; fi
  sig=$(running)
  shown || show
  [[ $(hyprctl -i "$sig" hypr-world 2>/dev/null) == *", in view,"* ]] ||
    hyprctl -i "$sig" eval 'hl.plugin.hypr_world.toggle()' >/dev/null
}

stop() {
  local sig deadline
  sig=$(running) || die "not running"
  # 0.56's dispatch takes a dispatcher in Lua.
  hyprctl -i "$sig" dispatch 'hl.dsp.exit()' >/dev/null
  # Gone once `compositor` has cleared up after it.
  deadline=$((SECONDS + 15))
  while [[ -e $DIR/running ]]; do
    ((SECONDS < deadline)) || die "$sig did not exit"
    sleep 0.2
  done
  rm -f "$INSTANCE" "$LOADED" "$DIR"/libhypr-world-*.so
  echo "nested Hyprland $sig stopped"
}

case ${1:-status} in
  compositor) compositor ;;
  backend) backend ;;
  headless) to_headless ;;
  up) up ;;
  start) start ;;
  load) load ;;
  rofi)
    shift
    rofi_into "$@"
    ;;
  run)
    sig=$(running) || die "not running"
    shift
    (($#)) || die "usage: $0 run <command>"
    # As a quoted string, which JSON and Lua write alike.
    hyprctl -i "$sig" eval "hl.exec_cmd($(jq -rn --arg c "$*" '$c | @json'))" >/dev/null
    ;;
  toggle)
    sig=$(running) || die "not running"
    hyprctl -i "$sig" eval 'hl.plugin.hypr_world.toggle()' >/dev/null
    ;;
  drive)
    sig=$(running) || die "not running"
    shift
    # The nested Hyprland draws only while its window is in view (a hidden
    # window gets no frame callbacks), so the drive brings it into view and
    # puts it back as it found it.
    hidden=0
    shown || { hidden=1 && show; }
    status=0
    HYPRLAND_INSTANCE_SIGNATURE=$sig HYPR_WORLD_DIR=$DIR "$ROOT/tools/drive.sh" "$@" || status=$?
    if ((hidden)); then show; fi
    exit "$status"
    ;;
  client)
    sig=$(running) || die "not running"
    shift
    (cd "$ROOT" && direnv exec . cargo build --release --example surfaces) >&2
    hyprctl -i "$sig" eval "hl.exec_cmd(\"[float] $HERE/nested.sh run-client $*\")" >/dev/null
    ;;
  run-client)
    shift
    client "$@"
    ;;
  show) show ;;
  stop) stop ;;
  status)
    if sig=$(running); then
      echo "nested Hyprland $sig, on special:$WORKSPACE"
      if headless "$sig"; then
        echo "headless, the app's backend (nested.sh backend)"
      elif ! windowed "$sig"; then
        echo "its window is closed: nested.sh up starts it again"
      fi
      echo "plugin: $(hyprctl -i "$sig" hypr-world 2>/dev/null || echo 'not loaded')"
      echo "app's pipe and log: $DIR"
    else
      echo "not running"
    fi
    ;;
  *) die "usage: $0 [backend|headless|run|rofi|up|start|load|toggle|drive|client|show|status|stop]" ;;
esac
