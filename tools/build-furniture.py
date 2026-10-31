# Builds the furniture's models from scratch: src/models/chandelier.blend,
# pedestal.blend, table.blend and plant.blend, each exported to the .glb
# beside it, which the app embeds. Rebuild the app after. This replaces the
# .blend files, so a change made to one in Blender is lost: make it here
# instead, or stop using this for that model.
#
#   env -u LD_LIBRARY_PATH blender --background --factory-startup \
#     --python tools/build-furniture.py
#
# LD_LIBRARY_PATH is dropped as in tools/export-models.sh. Blender is Z up,
# and the furniture faces -Y, Blender's front view, which the glTF exporter
# turns into Bevy's +Z.

import math
import os
import random

import bmesh
import bpy
from mathutils import Matrix, Vector

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "models")
TAU = 2 * math.pi


def lin(c):
    return tuple(x / 12.92 if x <= 0.04045 else ((x + 0.055) / 1.055) ** 2.4 for x in c)


def material(name, srgb, rough=0.8, metal=0.0, glow=0.0, glow_color=None, alpha=1.0, two_sided=False):
    m = bpy.data.materials.new(name)
    # Always on from Blender 5, where setting it warns.
    if bpy.app.version < (5, 0, 0):
        m.use_nodes = True
    p = m.node_tree.nodes.get("Principled BSDF")
    p.inputs["Base Color"].default_value = (*lin(srgb), 1.0)
    p.inputs["Roughness"].default_value = rough
    p.inputs["Metallic"].default_value = metal
    if glow > 0:
        p.inputs["Emission Color"].default_value = (*lin(glow_color or srgb), 1.0)
        p.inputs["Emission Strength"].default_value = glow
    if alpha < 1:
        p.inputs["Alpha"].default_value = alpha
        m.surface_render_method = "BLENDED"
    m.use_backface_culling = not two_sided
    return m


# Primitives, into a bmesh, centered on the origin.
def box(bm):
    bmesh.ops.create_cube(bm, size=1.0)


def cyl(segments=24, r1=0.5, r2=0.5):
    def make(bm):
        bmesh.ops.create_cone(
            bm, cap_ends=True, cap_tris=False, segments=segments, radius1=r1, radius2=r2, depth=1.0
        )

    return make


def sphere(u=16, v=10):
    def make(bm):
        bmesh.ops.create_uvsphere(bm, u_segments=u, v_segments=v, radius=0.5)

    return make


def diamond(bm):
    """An octahedron, a crystal drop."""
    vs = [bm.verts.new(p) for p in [(0, 0, 0.5), (0.5, 0, 0), (0, 0.5, 0), (-0.5, 0, 0), (0, -0.5, 0), (0, 0, -0.5)]]
    top, ring, bottom = vs[0], vs[1:5], vs[5]
    for i in range(4):
        a, b = ring[i], ring[(i + 1) % 4]
        bm.faces.new((top, a, b))
        bm.faces.new((bottom, b, a))


def lathe(profile, segments=32):
    """A solid of revolution about Z, `profile` (r, z) from one end to the
    other."""

    def make(bm):
        verts = [bm.verts.new((r, 0, z)) for r, z in profile]
        edges = [bm.edges.new((verts[i], verts[i + 1])) for i in range(len(verts) - 1)]
        bmesh.ops.spin(
            bm,
            geom=verts + edges,
            cent=(0, 0, 0),
            axis=(0, 0, 1),
            angle=TAU,
            steps=segments,
            use_duplicate=False,
        )
        bmesh.ops.remove_doubles(bm, verts=bm.verts, dist=1e-5)

    return make


def frames(points, closed):
    """A frame (tangent, normal, binormal) at each point, transported along."""
    n = len(points)
    tangents = []
    for i in range(n):
        if closed:
            t = points[(i + 1) % n] - points[i - 1]
        else:
            t = points[min(i + 1, n - 1)] - points[max(i - 1, 0)]
        tangents.append(t.normalized())
    up = Vector((0, 0, 1)) if abs(tangents[0].z) < 0.9 else Vector((1, 0, 0))
    normal = tangents[0].cross(up).normalized()
    out = []
    for t in tangents:
        normal = (normal - t * normal.dot(t)).normalized()
        out.append((t, normal, t.cross(normal)))
    return out


def tube(points, radius, sides=10, closed=False):
    points = [Vector(p) for p in points]

    def make(bm):
        rings = []
        for p, (t, n, b) in zip(points, frames(points, closed)):
            rings.append(
                [bm.verts.new(p + radius * (math.cos(TAU * k / sides) * n + math.sin(TAU * k / sides) * b)) for k in range(sides)]
            )
        count = len(rings) if closed else len(rings) - 1
        for i in range(count):
            a, b = rings[i], rings[(i + 1) % len(rings)]
            for k in range(sides):
                bm.faces.new((a[k], a[(k + 1) % sides], b[(k + 1) % sides], b[k]))
        if not closed:
            bm.faces.new(rings[0])
            bm.faces.new(list(reversed(rings[-1])))

    return make


def leaf(base, direction, length, width, droop, fold=0.25, steps=8):
    """A leaf: an open surface along a drooping midrib, folded along it."""
    direction = Vector(direction).normalized()
    side = direction.cross(Vector((0, 0, 1)))
    side = side.normalized() if side.length > 1e-6 else Vector((1, 0, 0))
    up = side.cross(direction).normalized()

    def make(bm):
        rows = []
        for i in range(steps + 1):
            u = i / steps
            mid = Vector(base) + direction * length * u - Vector((0, 0, droop * length * u * u))
            w = width * math.sin(math.pi * min(u ** 0.7, 1.0)) / 2
            rows.append(
                [bm.verts.new(mid + side * w * v + up * abs(v) * w * fold) for v in (-1.0, 0.0, 1.0)]
            )
        for i in range(steps):
            a, b = rows[i], rows[i + 1]
            for k in range(2):
                bm.faces.new((a[k], a[k + 1], b[k + 1], b[k]))

    return make


class Part:
    """One object's mesh, built up from primitives, each with a material."""

    def __init__(self):
        self.bm = bmesh.new()
        self.materials = []

    def add(self, make, mat, at=(0, 0, 0), size=(1, 1, 1), turn=None, smooth=False, bevel=0.0, solid=True):
        tmp = bmesh.new()
        make(tmp)
        matrix = Matrix.Translation(at) @ (turn or Matrix.Identity(4)) @ Matrix.Diagonal((*size, 1))
        bmesh.ops.transform(tmp, matrix=matrix, verts=tmp.verts)
        if solid:
            bmesh.ops.recalc_face_normals(tmp, faces=tmp.faces)
        if bevel > 0:
            bmesh.ops.bevel(
                tmp, geom=list(tmp.edges), offset=bevel, offset_type="OFFSET", segments=2, profile=0.5, affect="EDGES"
            )
        if mat not in self.materials:
            self.materials.append(mat)
        index = self.materials.index(mat)
        for f in tmp.faces:
            f.material_index = index
            f.smooth = smooth
        mesh = bpy.data.meshes.new("tmp")
        tmp.to_mesh(mesh)
        tmp.free()
        self.bm.from_mesh(mesh)
        bpy.data.meshes.remove(mesh)

    def finish(self, name, origin=(0, 0, 0)):
        origin = Vector(origin)
        # Hard edges where faces meet at an angle, smooth elsewhere.
        for e in self.bm.edges:
            if len(e.link_faces) == 2 and e.calc_face_angle(0) > math.radians(35):
                e.smooth = False
        bmesh.ops.translate(self.bm, vec=-origin, verts=self.bm.verts)
        mesh = bpy.data.meshes.new(name)
        self.bm.to_mesh(mesh)
        self.bm.free()
        for m in self.materials:
            mesh.materials.append(m)
        obj = bpy.data.objects.new(name, mesh)
        obj.location = origin
        bpy.context.scene.collection.objects.link(obj)
        return obj


def save(name):
    bpy.ops.wm.save_as_mainfile(filepath=f"{OUT}/{name}.blend")
    bpy.ops.export_scene.gltf(filepath=f"{OUT}/{name}.glb", export_format="GLB")


def fresh():
    bpy.ops.wm.read_factory_settings(use_empty=True)


def around(count, offset=0.0):
    for i in range(count):
        a = offset + TAU * i / count
        yield a, Vector((math.sin(a), -math.cos(a), 0))


# The chandelier: its origin where the rod from the ceiling meets it, the
# middle of its hub 0.25 under that, where the app puts its light.
def chandelier():
    fresh()
    brass = material("Brass", (0.82, 0.64, 0.3), rough=0.4, glow=0.12)
    candle = material("Candle", (1.0, 0.95, 0.82), rough=0.6, glow=0.55)
    flame = material("Flame", (1.0, 0.85, 0.5), glow=1.6, glow_color=(1.0, 0.8, 0.45))
    crystal = material("Crystal", (0.85, 0.93, 1.0), rough=0.05, glow=0.25, glow_color=(0.7, 0.8, 0.95), alpha=0.55)
    p = Part()

    hub = -0.25
    p.add(sphere(), brass, at=(0, 0, 0), size=(0.06, 0.06, 0.06), smooth=True)
    p.add(
        lathe(
            [(0, 0.0), (0.03, -0.01), (0.04, -0.04), (0.065, -0.09), (0.088, -0.18), (0.094, hub),
             (0.088, -0.32), (0.065, -0.4), (0.04, -0.46), (0.052, -0.49), (0.05, -0.5), (0, -0.505)]
        ),
        brass,
        smooth=True,
    )
    p.add(
        lathe([(0, -0.5), (0.03, -0.52), (0.045, -0.57), (0.035, -0.62), (0.015, -0.66), (0, -0.68)]),
        brass,
        smooth=True,
    )

    def light(tip):
        # A drip pan and cup, a candle and its flame, on a tip.
        p.add(lathe([(0, -0.012), (0.012, -0.012), (0.05, 0.004), (0.052, 0.012), (0, 0.006)]), brass, at=tip, smooth=True)
        p.add(lathe([(0, 0.0), (0.02, 0.0), (0.03, 0.03), (0.028, 0.035), (0, 0.035)]), brass, at=tip, smooth=True)
        p.add(cyl(16), candle, at=tip + Vector((0, 0, 0.035 + 0.08)), size=(0.044, 0.044, 0.16), smooth=True)
        p.add(cyl(8), brass, at=tip + Vector((0, 0, 0.2)), size=(0.003, 0.003, 0.016))
        p.add(sphere(10, 8), flame, at=tip + Vector((0, 0, 0.235)), size=(0.028, 0.028, 0.06), smooth=True)

    def drop(at, thread):
        p.add(cyl(6), brass, at=at - Vector((0, 0, thread / 2)), size=(0.004, 0.004, thread))
        p.add(diamond, crystal, at=at - Vector((0, 0, thread + 0.035)), size=(0.04, 0.04, 0.07))

    # Eight arms out from the hub, dipping and rising to their candles,
    # and a ring under their cups.
    reach = 0.62
    for a, out in around(8):
        path = []
        for i in range(13):
            t = i / 12
            r = 0.07 + (reach - 0.07) * t
            z = hub - 0.02 - 0.09 * math.sin(math.pi * t) + 0.02 * t
            path.append(out * r + Vector((0, 0, z)))
        p.add(tube(path, 0.013), brass, smooth=True)
        tip = out * reach + Vector((0, 0, hub))
        light(tip)
        drop(tip - Vector((0, 0, 0.012)), 0.05)
    ring = [Vector((math.sin(a) * (reach - 0.01), -math.cos(a) * (reach - 0.01), hub - 0.035)) for a in (TAU * i / 64 for i in range(64))]
    p.add(tube(ring, 0.009, closed=True), brass, smooth=True)
    for a, out in around(8, TAU / 16):
        drop(out * (reach - 0.01) * math.cos(TAU / 16) + Vector((0, 0, hub - 0.045)), 0.11)
    # A long one under the finial.
    drop(Vector((0, 0, -0.68)), 0.06)

    # Four shorter arms above, between the others, curling up.
    for a, out in around(4, TAU / 8):
        path = []
        for i in range(10):
            t = i / 9
            r = 0.02 + 0.32 * t
            z = 0.03 + 0.06 * math.sin(math.pi * t * 0.9) - 0.02 * t
            path.append(out * r + Vector((0, 0, z)))
        p.add(tube(path, 0.011), brass, smooth=True)
        light(out * 0.34 + Vector((0, 0, 0.05)))

    p.finish("Chandelier")
    save("chandelier")


# The pedestal: its origin on the floor, its top 1 m up. The button and the
# glass cover are objects of their own, for the app to move: the button's
# origin is where it rests, the cover's is its hinge, along its back edge.
def pedestal():
    fresh()
    stone = material("Stone", (0.86, 0.84, 0.8), rough=0.7)
    dark = material("Housing", (0.08, 0.08, 0.09), rough=0.5)
    steel = material("Steel", (0.55, 0.56, 0.58), rough=0.35, metal=0.6)
    red = material("Red", (0.85, 0.06, 0.05), rough=0.3, glow=0.12)
    glass = material("Glass", (0.8, 0.9, 0.95), rough=0.2, glow=0.03, alpha=0.2)

    p = Part()
    p.add(box, stone, at=(0, 0, 0.05), size=(0.6, 0.6, 0.1), bevel=0.01)
    p.add(box, stone, at=(0, 0, 0.125), size=(0.52, 0.52, 0.05), bevel=0.008)
    p.add(
        lathe([(0, 0.15), (0.19, 0.15), (0.2, 0.17), (0.17, 0.2), (0.155, 0.22), (0.15, 0.5),
               (0.145, 0.84), (0.16, 0.86), (0.19, 0.89), (0.2, 0.92), (0, 0.92)], 40),
        stone,
        smooth=True,
    )
    p.add(box, stone, at=(0, 0, 0.96), size=(0.5, 0.5, 0.08), bevel=0.012)
    p.add(box, dark, at=(0, 0, 1.01), size=(0.26, 0.26, 0.02), bevel=0.003)
    p.add(cyl(32), dark, at=(0, 0, 1.03), size=(0.16, 0.16, 0.02), smooth=True)
    # The hinge's knuckles, at the back.
    for x in (-0.09, 0.09):
        p.add(cyl(12), steel, at=(x, 0.123, 1.024), size=(0.012, 0.012, 0.04), turn=Matrix.Rotation(math.pi / 2, 4, "Y"), smooth=True)
    p.finish("Pedestal")

    b = Part()
    b.add(cyl(32), red, at=(0, 0, 1.04 + 0.015), size=(0.12, 0.12, 0.03), smooth=True)
    b.add(sphere(32, 12), red, at=(0, 0, 1.07), size=(0.118, 0.118, 0.04), smooth=True)
    b.finish("Button", origin=(0, 0, 1.065))

    # The cover: five panes and a steel frame round its edges, hinged at its
    # back, a hair off the plate.
    c = Part()
    w, d, h, pane, base = 0.24, 0.24, 0.14, 0.006, 1.0205
    side = h - pane
    for at, size in [
        ((0, 0, base + h - pane / 2), (w, d, pane)),
        ((0, -d / 2 + pane / 2, base + side / 2), (w - 2 * pane, pane, side)),
        ((0, d / 2 - pane / 2, base + side / 2), (w - 2 * pane, pane, side)),
        ((-w / 2 + pane / 2, 0, base + side / 2), (pane, d, side)),
        ((w / 2 - pane / 2, 0, base + side / 2), (pane, d, side)),
    ]:
        c.add(box, glass, at=at, size=size)
    bar = 0.008
    for x in (-w / 2, w / 2):
        for y in (-d / 2, d / 2):
            c.add(box, steel, at=(x, y, base + h / 2), size=(bar, bar, h + bar))
    for z in (base + bar / 2, base + h):
        for y in (-d / 2, d / 2):
            c.add(box, steel, at=(0, y, z), size=(w, bar, bar))
        for x in (-w / 2, w / 2):
            c.add(box, steel, at=(x, 0, z), size=(bar, d, bar))
    # A tab at the front to lift it by.
    c.add(box, steel, at=(0, -d / 2 - 0.012, base + h - 0.01), size=(0.06, 0.024, 0.006), bevel=0.002)
    c.finish("Cover", origin=(0, d / 2, base))
    save("pedestal")


# The side table: its origin on the floor, its top 0.55 m up.
def table():
    fresh()
    wood = material("Wood", (0.36, 0.22, 0.12), rough=0.55)
    p = Part()
    top, legs_h = 0.55, 0.51
    p.add(box, wood, at=(0, 0, top - 0.02), size=(0.9, 0.6, 0.04), bevel=0.01)
    hw, hd = 0.4, 0.25
    for x in (-hw, hw):
        for y in (-hd, hd):
            p.add(cyl(4, 0.5, 0.36), wood, at=(x, y, legs_h / 2), size=(0.06, 0.06, legs_h), turn=Matrix.Rotation(math.pi / 4, 4, "Z"))
    for y in (-hd, hd):
        p.add(box, wood, at=(0, y, legs_h - 0.04), size=(2 * hw, 0.025, 0.08))
    for x in (-hw, hw):
        p.add(box, wood, at=(x, 0, legs_h - 0.04), size=(0.025, 2 * hd, 0.08))
    # A shelf low down.
    p.add(box, wood, at=(0, 0, 0.12), size=(2 * hw, 2 * hd, 0.02))
    p.finish("Table")
    save("table")


# The plant: its origin under its pot.
def plant():
    fresh()
    pot = material("Pot", (0.72, 0.38, 0.26), rough=0.85)
    soil = material("Soil", (0.18, 0.12, 0.08), rough=1.0)
    greens = [
        material("Leaf", (0.22, 0.5, 0.22), rough=0.6, two_sided=True),
        material("Leaf.light", (0.35, 0.6, 0.25), rough=0.6, two_sided=True),
    ]
    p = Part()
    p.add(
        lathe([(0, 0), (0.075, 0), (0.08, 0.006), (0.1, 0.17), (0.118, 0.175), (0.12, 0.215),
               (0.105, 0.22), (0.1, 0.19), (0, 0.19)], 32),
        pot,
        smooth=True,
    )
    p.add(cyl(32), soil, at=(0, 0, 0.19), size=(0.2, 0.2, 0.004))
    rng = random.Random(7)
    for i in range(16):
        a = TAU * i / 16 + rng.uniform(-0.2, 0.2)
        rise = math.radians(rng.uniform(30, 75))
        direction = (math.cos(a) * math.cos(rise), math.sin(a) * math.cos(rise), math.sin(rise))
        base = (0.02 * math.cos(a), 0.02 * math.sin(a), 0.19)
        p.add(
            leaf(base, direction, rng.uniform(0.22, 0.36), rng.uniform(0.06, 0.09), rng.uniform(0.3, 0.7)),
            greens[i % 2],
            smooth=True,
            solid=False,
        )
    p.finish("Plant")
    save("plant")


chandelier()
pedestal()
table()
plant()
