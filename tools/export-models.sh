#!/usr/bin/env bash
# Exports each src/models/*.blend to the .glb beside it, which the app
# embeds: rebuild the app after. Blender is Z up and glTF Y up, and the
# exporter converts, so Blender's +Y is a model's forward, Bevy's -Z.
#
# LD_LIBRARY_PATH is dropped: libraries a shell put there can be newer than
# the glibc Blender was built against, and then Blender does not start.
set -euo pipefail

cd "$(dirname "$0")/.."
for blend in src/models/*.blend; do
  glb="${blend%.blend}.glb"
  env -u LD_LIBRARY_PATH blender --background "$blend" --python-expr \
    "import bpy; bpy.ops.export_scene.gltf(filepath=\"$glb\", export_format=\"GLB\"); bpy.ops.wm.quit_blender()"
done
