#!/usr/bin/env bash
# A Hyprland of its own, in a window on the desktop, for trying hypr-world
# plugin builds without loading them into the session's compositor: a crash
# takes down that window and nothing else. It is started through the
# session's compositor, so it runs where the app does, with the GPU and the
# session's Wayland socket, on a special workspace of its own.
#
#   nested.sh start          the nested Hyprland, this plugin's build loaded in
#                            it, and a terminal for the walls
#   nested.sh load           build the plugin and load it into the nested one
#                            again, in place of the build loaded there
#   nested.sh toggle         the world key, in the nested session
#   nested.sh drive [out-dir [steps-file]]
#                            tools/drive.sh against the nested session's app
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

# Run by the session's compositor: the nested Hyprland itself. The app its
# plugin launches keeps its pipe and log in $DIR.
compositor() {
  echo $$ >"$DIR/running"
  export HYPR_WORLD_DIR=$DIR
  export HYPRLAND_NO_SD_NOTIFY=1 HYPRLAND_NO_CRASHREPORTER=1

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
  mkdir -p "$DIR/bin"
  cat >"$DIR/bin/systemctl" <<'EOF'
#!/bin/sh
case " $* " in *" --user "*) exit 0 ;; esac
exec /run/current-system/sw/bin/systemctl "$@"
EOF
  printf '#!/bin/sh\nexit 0\n' >"$DIR/bin/dbus-update-activation-environment"
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
  start) start ;;
  load) load ;;
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
      echo "plugin: $(hyprctl -i "$sig" hypr-world 2>/dev/null || echo 'not loaded')"
      echo "app's pipe and log: $DIR"
    else
      echo "not running"
    fi
    ;;
  *) die "usage: $0 [start|load|toggle|drive|client|show|status|stop]" ;;
esac
