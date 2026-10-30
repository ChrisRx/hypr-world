#!/usr/bin/env bash
# Build the hypr-world plugin against the running compositor and hot-swap it
# into the session, no logout.
#
#   dev.sh          incremental build, then swap it in, or with the session
#                   config loading it, `nested.sh up` instead
#   dev.sh -k       the same, ending the running app first, so the world key
#                   launches the app's newest build (every process named
#                   hypr-world: a nested session's app too)
#   dev.sh build    the build alone, loaded nowhere (nested.sh loads it into
#                   a nested test session)
#   dev.sh unload   unload it
#
# Built in the environment of ./default.nix with the nixos-config's
# unstable.hyprland, the same build the session runs (PLUGIN_INIT refuses
# any other). Hyprland keys loaded plugins by path, and dlopen can hand back
# a cached image for a path it has seen, so every build is loaded from a
# fresh copy under $XDG_RUNTIME_DIR, in one hyprctl batch with the unload.
# A plugin loaded with no swap recorded is the one the session config loads,
# which this leaves alone: the config would load it again on its next reload,
# next to the swapped-in build. The build goes to the nested session then.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CONFIG=${NIXOS_CONFIG:-/etc/nixos/nixos-config}
BUILD=$HERE/build
RUN=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/hypr-world-dev
STATE=$RUN/loaded

die() {
  echo "dev.sh: $*" >&2
  exit 1
}

pkg_expr() {
  cat <<EOF
let
  f = builtins.getFlake "$CONFIG";
  unstable = import f.inputs.nixpkgs-unstable { system = builtins.currentSystem; };
in
unstable.callPackage $HERE/default.nix { }
EOF
}

build() {
  if [[ ! -f $BUILD/.devenv.sh ]]; then
    mkdir -p "$BUILD"
    nix print-dev-env --impure --expr "$(pkg_expr)" >"$BUILD/.devenv.sh"
  fi
  (
    set +eu
    # shellcheck disable=SC1091
    source "$BUILD/.devenv.sh"
    cd "$HERE"
    [[ -f build/CMakeCache.txt ]] || cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
    cmake --build build -j"$(nproc)"
  ) || die "build failed"
}

# Plugins named hypr-world that are loaded right now.
loaded_count() {
  hyprctl plugin list -j | jq '[.[] | select(.name == "hypr-world")] | length'
}

# Whether the plugin loaded is the one the session config loads.
config_loaded() {
  [[ ! -s $STATE ]] && (($(loaded_count) > 0))
}

swap() {
  local new=$1 old out
  old=$(cat "$STATE" 2>/dev/null || true)

  if (($(loaded_count) > 0)); then
    [[ -n $old ]] || die "the session config loads hypr-world; try builds in nested.sh instead"
    out=$(hyprctl --batch "plugin unload $old ; plugin load $new" || true)
    [[ ${out%%$'\n'*} == ok ]] || die "could not unload $old (${out%%$'\n'*})"
  else
    hyprctl plugin load "$new" >/dev/null || true
  fi

  if (($(loaded_count) != 1)); then
    echo "dev.sh: $new did not load, see: hyprctl rollinglog | grep -i hypr-world" >&2
    exit 1
  fi

  echo "$new" >"$STATE"
  if [[ $old == "$RUN"/* ]]; then rm -f "$old"; fi
  echo "hypr-world loaded from $new"
}

if [[ ${1:-} == -k ]]; then
  pkill -x hypr-world && echo "dev.sh: ended the running app" || true
  shift
fi

case ${1:-reload} in
  build) build ;;
  reload)
    build
    if config_loaded; then
      echo "dev.sh: the session config loads hypr-world, so this build goes to the nested session"
      exec "$HERE/nested.sh" up
    fi
    mkdir -p "$RUN"
    copy=$RUN/libhypr-world-$(date +%s%N).so
    cp "$BUILD/libhypr-world.so" "$copy"
    swap "$copy"
    ;;
  unload)
    old=$(cat "$STATE" 2>/dev/null || true)
    [[ -n $old ]] || die "nothing loaded by dev.sh to unload"
    hyprctl plugin unload "$old"
    rm -f "$STATE"
    if [[ $old == "$RUN"/* ]]; then rm -f "$old"; fi
    ;;
  *) die "usage: $0 [-k] [reload|build|unload]" ;;
esac
