#!/usr/bin/env bash
# Drives the app with no hands, for tests: opens it as the world key does,
# takes it round a tour (or the steps in a file), and quits. Screenshots of
# what the app draws, a copy of its log and the GPU's busy percentage, four
# times a second, go into the output folder.
#
#   drive.sh [out-dir [steps-file]]
#
# A steps file has one step a line:
#   hold <key> <seconds>     w, a, s, d, r, space, shift, or an evdev key code
#   click                    the left mouse button, pressed and let go
#   turn <dx> <dy> <moves>   that many mouse moves of dx, dy px, a frame apart
#   wait <seconds>
#   set <key> <value>        a world setting, for this run
#   toggle                   the world key: the flight out, or back in
#   shot <name>              a screenshot, <out-dir>/<name>.png
#
# The keys and moves go in through the app's control pipe, as if the plugin
# had sent them (src/glue.rs `control`). While it runs the app has the
# screen, the keyboard and the mouse; Super+Escape, or `hyprctl hypr-world
# off`, takes them back.
set -euo pipefail

# The app's pipe and log, and the screenshots by default, are in
# $HYPR_WORLD_DIR when that is set, and hyprctl talks to the Hyprland
# instance $HYPRLAND_INSTANCE_SIGNATURE names: plugin/nested.sh sets both to
# drive the app in a nested test session.
RUN=${HYPR_WORLD_DIR:-${XDG_RUNTIME_DIR:-/run/user/$(id -u)}}
PIPE=$RUN/hypr-world.ctl
LOG=$RUN/hypr-world.log
OUT=${1:-$RUN/hypr-world-drive}
STEPS=${2:-}
GPU=$(ls /sys/class/drm/card*/device/gpu_busy_percent 2>/dev/null | head -n 1 || true)

die() {
  echo "drive.sh: $*" >&2
  exit 1
}

mkdir -p "$OUT"
rm -f "$OUT"/*.png "$OUT/gpu.log" "$OUT/hypr-world.log"

state() { hyprctl hypr-world 2>/dev/null || true; }

# Waits up to $2 seconds for the plugin's state to say $1.
wait_for() {
  local deadline=$((SECONDS + $2))
  until state | grep -q -- "$1"; do
    ((SECONDS < deadline)) || die "the plugin never said \"$1\": $(state)"
    sleep 0.1
  done
}

# In, unless it already is: launched if it is not running.
state | grep -q 'app connected, in view' || hyprctl eval 'hl.plugin.hypr_world.toggle()' >/dev/null
wait_for 'app connected, in view' 30
wait_for 'state look' 5
# The flight in and the fade.
sleep 1.5

# Something must read the pipe: with no reader the open below would wait
# forever.
timeout 3 sh -c ': > "$1"' _ "$PIPE" || die "nothing reads $PIPE"
exec 3>"$PIPE"
send() { printf '%s\n' "$1" >&3; }

code() {
  case $1 in
    w) echo 17 ;;
    a) echo 30 ;;
    s) echo 31 ;;
    d) echo 32 ;;
    r) echo 19 ;;
    space) echo 57 ;;
    shift) echo 42 ;;
    *) echo "$1" ;;
  esac
}

hold() {
  local key
  key=$(code "$1")
  send "key $key 1"
  sleep "$2"
  send "key $key 0"
}

turn() {
  for _ in $(seq "$3"); do
    send "motion $1 $2 $1 $2"
    sleep 0.016
  done
}

# Once the look has eased in, of the next frame drawn.
shot() {
  local file=$OUT/$1.png
  sleep 0.4
  send "screenshot $file"
  for _ in $(seq 100); do
    [ -s "$file" ] && return
    sleep 0.1
  done
  echo "drive.sh: no screenshot $file" >&2
}

step() {
  # shellcheck disable=SC2086 # a step is words
  set -- $1
  case ${1:-} in
    hold) hold "$2" "$3" ;;
    # As the plugin sends a click it keeps from the windows (BTN_LEFT).
    click) send "button 272 1 0" && sleep 0.05 && send "button 272 0 0" ;;
    turn) turn "$2" "$3" "$4" ;;
    wait) sleep "$2" ;;
    set) send "set $2 $3" ;;
    toggle) hyprctl eval 'hl.plugin.hypr_world.toggle()' >/dev/null ;;
    shot) shot "$2" ;;
    '' | '#'*) ;;
    *) die "no such step: $*" ;;
  esac
}

if [ -n "$GPU" ]; then
  while :; do
    printf '%s %s\n' "$(date +%s.%N)" "$(cat "$GPU")"
    sleep 0.25
  done >"$OUT/gpu.log" &
  SAMPLER=$!
fi

if [ -n "$STEPS" ]; then
  while IFS= read -r line; do step "$line"; done <"$STEPS"
else
  # The four ways round and up, a screenshot each, then a walk. A quarter
  # turn is 628 px of motion (0.0025 rad a px).
  for line in \
    "shot ahead" "turn 16 0 40" "shot right" "turn 16 0 40" "shot behind" \
    "turn 16 0 40" "shot left" "turn 16 0 40" "turn 0 -12 40" "shot up" "turn 0 12 40" \
    "hold w 2" "hold a 1" "hold d 1" "hold s 2" "wait 3"; do
    step "$line"
  done
fi

send quit
exec 3>&-
if [ -n "${SAMPLER:-}" ]; then
  kill "$SAMPLER" 2>/dev/null || true
fi
wait_for 'app not connected' 10
cp "$LOG" "$OUT/hypr-world.log"
echo "drive.sh: done, in $OUT"
