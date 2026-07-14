"""Build a .blend file from 3dmodeler interchange JSON.

Run headless:  blender -b --factory-startup -P json_to_blend.py -- in.json out.blend

Objects arrive parents-first with transforms relative to their parent, so
parenting uses an identity parent-inverse and plain loc/rot/scale. Both apps
are Z-up meters, so transforms pass through unchanged.
"""
import bpy
import json
import math
import sys
from mathutils import Matrix, Quaternion, Vector

LIGHT_TYPES = {'Point': 'POINT', 'Sun': 'SUN', 'Spot': 'SPOT'}


def make_material(name, spec):
    mat = bpy.data.materials.new(name)
    mat.use_nodes = True
    color = list(spec['base_color'])[:3] + [1.0]
    bsdf = next((n for n in mat.node_tree.nodes if n.type == 'BSDF_PRINCIPLED'), None)
    if bsdf is not None:
        bsdf.inputs['Base Color'].default_value = color
        bsdf.inputs['Roughness'].default_value = spec['roughness']
        bsdf.inputs['Metallic'].default_value = spec['metallic']
    mat.diffuse_color = color  # solid-shading viewport color
    mat.roughness = spec['roughness']
    mat.metallic = spec['metallic']
    return mat


def make_mesh(name, spec):
    mesh = bpy.data.meshes.new(name)
    p = spec['positions']
    verts = [(p[i], p[i + 1], p[i + 2]) for i in range(0, len(p), 3)]
    idx = spec['indices']
    faces = [(idx[i], idx[i + 1], idx[i + 2]) for i in range(0, len(idx), 3)]
    mesh.from_pydata(verts, [], faces)
    mesh.validate()
    n = spec.get('normals') or []
    if len(n) == len(p):
        normals = [(n[i], n[i + 1], n[i + 2]) for i in range(0, len(n), 3)]
        try:
            mesh.normals_split_custom_set_from_vertices(normals)
        except (AttributeError, RuntimeError):
            pass  # API drift across versions: default normals are fine
    return mesh


def make_light(name, spec):
    light = bpy.data.lights.new(name, LIGHT_TYPES.get(spec['kind'], 'POINT'))
    light.color = list(spec['color'])[:3]
    # the modeler's small unitless intensity maps back to Watts
    light.energy = spec['intensity'] if spec['kind'] == 'Sun' else spec['intensity'] * 100.0
    if spec['kind'] == 'Spot':
        light.spot_size = math.radians(spec['spot_angle_deg'])
    if hasattr(light, 'use_shadow'):
        light.use_shadow = bool(spec['shadows'])
    return light


def main():
    args = sys.argv[sys.argv.index('--') + 1:]
    in_path, out_path = args[0], args[1]
    with open(in_path) as f:
        data = json.load(f)

    # start from an empty scene, not the startup cube/camera/light
    for obj in list(bpy.data.objects):
        bpy.data.objects.remove(obj, do_unlink=True)

    created = {}
    for spec in data['objects']:
        kind = spec['kind']
        name = spec['name']
        if kind == 'mesh':
            obj = bpy.data.objects.new(name, make_mesh(name, spec['mesh']))
            if spec.get('material'):
                obj.data.materials.append(make_material(name, spec['material']))
        elif kind == 'light':
            obj = bpy.data.objects.new(name, make_light(name, spec['light']))
        else:  # empty (and any unknown kind becomes a placeholder empty)
            obj = bpy.data.objects.new(name, None)
            obj.empty_display_type = 'PLAIN_AXES'
            obj.empty_display_size = spec.get('size') or 1.0
        bpy.context.scene.collection.objects.link(obj)
        created[name] = obj

    for spec in data['objects']:
        obj = created[spec['name']]
        parent = spec.get('parent')
        if parent and parent in created:
            obj.parent = created[parent]
            obj.matrix_parent_inverse = Matrix.Identity(4)
        obj.rotation_mode = 'QUATERNION'
        obj.location = Vector(spec['location'])
        obj.rotation_quaternion = Quaternion(spec['rotation_wxyz'])
        obj.scale = Vector(spec['scale'])
        if not spec.get('visible', True):
            obj.hide_viewport = True
            obj.hide_render = True

    bpy.ops.wm.save_as_mainfile(filepath=out_path)


main()
