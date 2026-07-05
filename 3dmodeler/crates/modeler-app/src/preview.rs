//! Software-rendered library-asset previews.
//!
//! A tiny CPU rasterizer (orthographic isometric view, z-buffer, flat
//! lambert shading) so previews are deterministic, independent of the
//! viewport camera/scene, and work identically on native, wasm and headless
//! MCP calls. Assets are small object groups, so speed is a non-issue.

use modeler_core::glam::Vec3;
use modeler_core::Object;

pub const PREVIEW_SIZE: u32 = 128;

/// World transform of an object WITHIN an asset's object list (parents are
/// resolved against the list, not a scene).
fn local_world(objects: &[Object], object: &Object) -> modeler_core::Transform {
    match object.parent.and_then(|p| objects.iter().find(|o| o.id == p)) {
        Some(parent) => modeler_core::Transform::compose(
            &local_world(objects, parent),
            &object.transform,
        ),
        None => object.transform,
    }
}

/// Render the asset's objects to a PREVIEW_SIZE² PNG (base64, transparent
/// background). None if there is nothing visible to draw.
pub fn render_preview_base64(objects: &[Object]) -> Option<String> {
    // gather world-space triangles with their base colors
    let mut triangles: Vec<([Vec3; 3], [f32; 3])> = Vec::new();
    for object in objects {
        if !object.visible {
            continue;
        }
        let t = local_world(objects, object);
        let mesh = object.render_mesh();
        for tri in mesh.indices.chunks_exact(3) {
            let world = |i: u32| {
                let p = mesh.positions[i as usize];
                t.location + t.rotation * (p * t.scale)
            };
            triangles.push((
                [world(tri[0]), world(tri[1]), world(tri[2])],
                object.material.base_color,
            ));
        }
    }
    if triangles.is_empty() {
        return None;
    }

    // isometric-ish camera basis looking at the bounds center
    let (min, max) = triangles.iter().flat_map(|(v, _)| v.iter()).fold(
        (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
        |(lo, hi), p| (lo.min(*p), hi.max(*p)),
    );
    let center = 0.5 * (min + max);
    let forward = Vec3::new(-0.66, 0.6, -0.45).normalize(); // eye at +x, -y, above
    let right = forward.cross(Vec3::Z).normalize();
    let up = right.cross(forward).normalize();

    // orthographic fit: max projected extent -> image size minus a margin
    let mut extent = 1e-6f32;
    for (v, _) in &triangles {
        for p in v {
            let d = *p - center;
            extent = extent.max(d.dot(right).abs()).max(d.dot(up).abs());
        }
    }
    let size = PREVIEW_SIZE as i32;
    let scale = (PREVIEW_SIZE as f32 * 0.5 - 6.0) / extent;
    let project = |p: Vec3| {
        let d = p - center;
        (
            PREVIEW_SIZE as f32 * 0.5 + d.dot(right) * scale,
            PREVIEW_SIZE as f32 * 0.5 - d.dot(up) * scale,
            d.dot(forward), // depth grows away from the eye
        )
    };

    let light = Vec3::new(-0.35, 0.45, 0.82).normalize();
    let mut rgba = vec![0u8; (PREVIEW_SIZE * PREVIEW_SIZE * 4) as usize];
    let mut depth = vec![f32::INFINITY; (PREVIEW_SIZE * PREVIEW_SIZE) as usize];

    for (v, base_color) in &triangles {
        let n = (v[1] - v[0]).cross(v[2] - v[0]).normalize_or_zero();
        // double-sided lambert with a floor so bottom faces stay readable
        let brightness = 0.3 + 0.7 * n.dot(light).abs();
        let color = base_color.map(|c| ((c * brightness).clamp(0.0, 1.0) * 255.0) as u8);

        let (ax, ay, az) = project(v[0]);
        let (bx, by, bz) = project(v[1]);
        let (cx, cy, cz) = project(v[2]);
        let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
        if area.abs() < 1e-6 {
            continue;
        }
        let x0 = (ax.min(bx).min(cx).floor() as i32).clamp(0, size - 1);
        let x1 = (ax.max(bx).max(cx).ceil() as i32).clamp(0, size - 1);
        let y0 = (ay.min(by).min(cy).floor() as i32).clamp(0, size - 1);
        let y1 = (ay.max(by).max(cy).ceil() as i32).clamp(0, size - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // barycentric coordinates
                let w0 = ((bx - ax) * (py - ay) - (by - ay) * (px - ax)) / area;
                let w1 = ((cx - bx) * (py - by) - (cy - by) * (px - bx)) / area;
                let w2 = 1.0 - w0 - w1;
                // w0 is the weight of c, w1 of a, w2 of b (edge order)
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w1 * az + w2 * bz + w0 * cz;
                let i = (y * size + x) as usize;
                if z < depth[i] {
                    depth[i] = z;
                    rgba[i * 4..i * 4 + 4]
                        .copy_from_slice(&[color[0], color[1], color[2], 255]);
                }
            }
        }
    }

    // encode as PNG + base64
    let image = image::RgbaImage::from_raw(PREVIEW_SIZE, PREVIEW_SIZE, rgba)?;
    let mut png_bytes: Vec<u8> = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )
        .ok()?;
    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(&png_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use modeler_core::{Primitive, Scene, Transform};

    #[test]
    fn preview_renders_a_cube() {
        let mut scene = Scene::default_scene();
        let id = scene.objects()[0].id;
        scene.object_mut(id).unwrap().material.base_color = [1.0, 0.2, 0.2];
        let objects = modeler_core::library::capture_objects(&scene, &[id]);

        let b64 = render_preview_base64(&objects).expect("preview");
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");

        // decodes to the right size with some opaque reddish pixels
        let img = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (PREVIEW_SIZE, PREVIEW_SIZE));
        let lit = img
            .pixels()
            .filter(|p| p[3] == 255 && p[0] > p[2])
            .count();
        assert!(lit > 500, "expected a visible red cube, got {lit} pixels");
        // corners stay transparent
        assert_eq!(img.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn preview_of_nothing_is_none() {
        assert!(render_preview_base64(&[]).is_none());
        // invisible objects don't render
        let mut scene = Scene::new();
        let id = scene.add_object(Primitive::Cube { size: 2.0 }, Transform::default());
        scene.object_mut(id).unwrap().visible = false;
        let objects = modeler_core::library::capture_objects(&scene, &[id]);
        assert!(render_preview_base64(&objects).is_none());
    }
}
