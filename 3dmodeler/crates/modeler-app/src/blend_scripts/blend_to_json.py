"""Export the loaded .blend as interchange JSON for the 3dmodeler.

Run headless:  blender -b --factory-startup file.blend -P blend_to_json.py -- out.json

Meshes are exported evaluated (modifier stacks applied), triangulated, in
object-local space, with per-corner normals split into per-vertex normals.
Curves/surfaces/text export as their evaluated mesh; empties and lights map
to the modeler's Empty/Light primitives; everything else is skipped and
counted. Both apps are Z-up meters, so transforms pass through unchanged.
"""
import bpy
import json
import math
import sys

# AREA lights approximate to Point; the modeler has no area light.
LIGHT_KINDS = {'POINT': 'Point', 'SUN': 'Sun', 'SPOT': 'Spot', 'AREA': 'Point'}
MESHABLE = {'MESH', 'CURVE', 'SURFACE', 'META', 'FONT'}


def material_of(obj):
    mat = obj.active_material
    if mat is None:
        return None
    bsdf = None
    if mat.use_nodes:
        bsdf = next((n for n in mat.node_tree.nodes if n.type == 'BSDF_PRINCIPLED'), None)
    if bsdf is not None:
        color = list(bsdf.inputs['Base Color'].default_value)[:3]
        rough = float(bsdf.inputs['Roughness'].default_value)
        metal = float(bsdf.inputs['Metallic'].default_value)
    else:
        color = list(mat.diffuse_color)[:3]
        rough = float(getattr(mat, 'roughness', 0.7))
        metal = float(getattr(mat, 'metallic', 0.0))
    return {'base_color': color, 'roughness': rough, 'metallic': metal}


def mesh_payload(obj, depsgraph):
    """Evaluated triangle mesh; vertices split per unique (vertex, normal)."""
    eval_obj = obj.evaluated_get(depsgraph)
    try:
        mesh = eval_obj.to_mesh()
    except RuntimeError:
        return None
    if mesh is None or len(mesh.polygons) == 0:
        eval_obj.to_mesh_clear()
        return None
    mesh.calc_loop_triangles()
    try:  # Blender 4.1+: per-corner normals (preserves sharp/smooth shading)
        corner_normals = [tuple(n.vector) for n in mesh.corner_normals]
    except AttributeError:
        corner_normals = None
    vert_normals = [tuple(v.normal) for v in mesh.vertices]
    loops = mesh.loops
    positions, normals, indices = [], [], []
    dedup = {}
    for tri in mesh.loop_triangles:
        for loop_index in tri.loops:
            vi = loops[loop_index].vertex_index
            n = corner_normals[loop_index] if corner_normals else vert_normals[vi]
            key = (vi, round(n[0], 3), round(n[1], 3), round(n[2], 3))
            index = dedup.get(key)
            if index is None:
                index = len(positions) // 3
                dedup[key] = index
                co = mesh.vertices[vi].co
                positions += [round(co.x, 6), round(co.y, 6), round(co.z, 6)]
                normals += [round(n[0], 4), round(n[1], 4), round(n[2], 4)]
            indices.append(index)
    eval_obj.to_mesh_clear()
    return {'positions': positions, 'normals': normals, 'indices': indices}


def light_payload(light):
    # point/spot energy is Watts; the modeler uses small unitless intensities
    energy = float(light.energy)
    return {
        'kind': LIGHT_KINDS[light.type],
        'color': list(light.color)[:3],
        'intensity': energy if light.type == 'SUN' else energy / 100.0,
        'spot_angle_deg': math.degrees(light.spot_size) if light.type == 'SPOT' else 45.0,
        'shadows': bool(getattr(light, 'use_shadow', True)),
    }


def is_hidden(obj):
    try:
        return obj.hide_viewport or obj.hide_get()
    except RuntimeError:
        return obj.hide_viewport


def main():
    out_path = sys.argv[sys.argv.index('--') + 1]
    depsgraph = bpy.context.evaluated_depsgraph_get()
    scene = bpy.context.scene

    # parents before children so the importer resolves links in one pass
    in_scene = set(scene.objects)
    ordered = []

    def add(obj):
        ordered.append(obj)
        for child in obj.children:
            if child in in_scene:
                add(child)

    for obj in scene.objects:
        if obj.parent is None or obj.parent not in in_scene:
            add(obj)

    exported = {}  # blender object -> exported name
    objects = []
    skipped = {}
    for obj in ordered:
        entry = None
        if obj.type in MESHABLE:
            mesh = mesh_payload(obj, depsgraph)
            if mesh is not None:
                entry = {'kind': 'mesh', 'mesh': mesh, 'material': material_of(obj)}
        elif obj.type == 'EMPTY':
            entry = {'kind': 'empty', 'size': float(obj.empty_display_size)}
        elif obj.type == 'LIGHT' and obj.data.type in LIGHT_KINDS:
            entry = {'kind': 'light', 'light': light_payload(obj.data)}
        if entry is None:
            skipped[obj.type] = skipped.get(obj.type, 0) + 1
            continue
        # transform relative to the exported parent (world when unparented)
        if obj.parent in exported and obj.parent_type == 'OBJECT':
            entry['parent'] = exported[obj.parent]
            matrix = obj.parent.matrix_world.inverted_safe() @ obj.matrix_world
        else:
            entry['parent'] = None
            matrix = obj.matrix_world
        loc, rot, scale = matrix.decompose()
        entry['name'] = obj.name
        entry['location'] = [loc.x, loc.y, loc.z]
        entry['rotation_wxyz'] = [rot.w, rot.x, rot.y, rot.z]
        entry['scale'] = [scale.x, scale.y, scale.z]
        entry['visible'] = not is_hidden(obj)
        exported[obj] = obj.name
        objects.append(entry)

    payload = {
        'blender_version': bpy.app.version_string,
        'objects': objects,
        'skipped': skipped,
    }
    with open(out_path, 'w') as f:
        json.dump(payload, f)


main()
