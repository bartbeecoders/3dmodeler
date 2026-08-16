//! The bridge from the document's texture references to the engine's GPU
//! material set.
//!
//! The document side (`modeler_core::MaterialTextures`) holds CACHE KEYS —
//! strings resolved to image files by the PBR library — and, for terrain
//! biome color, procedurally baked RGBA images. The engine side
//! (`aether_render`) holds four 2D array textures where every material owns
//! one layer, addressed by the `u32` indices inside [`GpuMaterial`].
//!
//! This module owns the mapping: it decodes/bakes images once per distinct
//! texture set, uploads them as an engine material layer, and hands
//! `scene_render` a `GpuMaterial` TEMPLATE carrying the layer indices and
//! `HAS_*` flags. `scene_render::gpu_material` then overwrites the scalar
//! constants per object. Layers unused for a while are evicted so a session
//! that browses many materials doesn't accumulate GPU memory forever.
//!
//! Missing maps are filled with neutral 1×1 placeholders (the engine
//! resamples everything to the working resolution anyway): the GBuffer
//! shader MULTIPLIES maps with the scalar constants, so a white albedo, a
//! flat normal and a white ORM are exact no-ops.

use aether_assets::material_library::{Material as AssetMaterial, MaterialMaps, MaterialParams};
use aether_assets::texture::{MipFilter, Texture2D};
use aether_render::material_textures::MaterialSlot;
use aether_render::types::GpuMaterial;
use aether_render::Renderer;
use std::collections::HashMap;

/// Working resolution of the engine material set. Everything is resampled
/// here on upload; 1K matches what the PBR library downloads.
const WORKING_RES: u32 = 1024;
/// Starting layer capacity (grows on demand).
const CAPACITY: u32 = 8;
/// Frames an entry may go unused before its GPU layer is reclaimed.
const EVICT_AFTER: u64 = 300;
/// Frames a changed bake stamp must hold still before the (expensive)
/// re-bake runs — keeps slider drags responsive; colors catch up on release.
const REBAKE_DEBOUNCE: u64 = 12;

struct Entry {
    slot: MaterialSlot,
    template: GpuMaterial,
    /// Synthetic sets: content stamp; a new stamp re-uploads into the slot.
    stamp: u64,
    last_used: u64,
    /// Debounce state: the stamp waiting to be baked, and since when.
    pending: Option<(u64, u64)>,
}

#[derive(Default)]
pub struct TextureBridge {
    entries: HashMap<String, Entry>,
    /// Sets whose files failed to load — never retried this session, so a
    /// missing texture doesn't cost a disk probe per frame.
    failed: std::collections::HashSet<String>,
    frame: u64,
    initialized: bool,
}

/// The decoded maps of one texture set, engine-ready.
struct ResolvedMaps {
    albedo: Texture2D,
    normal: Texture2D,
    orm: Texture2D,
    /// Which maps were actually present (drives scalar neutralization).
    pub has_albedo: bool,
    pub has_normal: bool,
    pub has_roughness: bool,
    pub has_metallic: bool,
    pub has_occlusion: bool,
}

/// What [`crate::scene_render::gpu_material`] needs to merge a texture set
/// into an object's material: the engine template (layer indices + flags)
/// and which scalar constants the maps now carry.
#[derive(Clone, Copy)]
pub struct TextureSet {
    pub template: GpuMaterial,
    pub has_albedo: bool,
    pub has_roughness: bool,
    pub has_metallic: bool,
    pub has_occlusion: bool,
}

impl TextureBridge {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_init(&mut self, renderer: &mut Renderer) {
        if !self.initialized {
            renderer.reset_material_textures(WORKING_RES, WORKING_RES, CAPACITY);
            self.initialized = true;
        }
    }

    /// Call once per frame before any `resolve_*`.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Reclaim layers nothing referenced recently. Call after the sync.
    pub fn end_frame(&mut self, renderer: &mut Renderer) {
        let frame = self.frame;
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| frame.saturating_sub(e.last_used) > EVICT_AFTER)
            .map(|(k, _)| k.clone())
            .collect();
        for key in stale {
            if let Some(entry) = self.entries.remove(&key) {
                renderer.remove_material(entry.slot);
            }
        }
    }

    /// Fingerprint of a texture-set reference, for `material_key` hashing.
    pub fn set_key(textures: &modeler_core::MaterialTextures) -> Option<String> {
        if textures.is_empty() {
            return None;
        }
        let part = |t: &Option<String>| t.clone().unwrap_or_default();
        Some(format!(
            "files:{}|{}|{}|{}|{}",
            part(&textures.albedo),
            part(&textures.normal),
            part(&textures.roughness),
            part(&textures.metallic),
            part(&textures.occlusion),
        ))
    }

    /// The engine layer for a document texture set (PBR library materials),
    /// loading and uploading it on first use. `None` when nothing resolves
    /// (all cache keys missing on disk) — callers fall back to scalars.
    pub fn resolve_files(
        &mut self,
        renderer: &mut Renderer,
        textures: &modeler_core::MaterialTextures,
    ) -> Option<TextureSet> {
        let key = Self::set_key(textures)?;
        if self.failed.contains(&key) {
            return None;
        }
        self.ensure_init(renderer);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.frame;
            return Some(TextureSet {
                template: entry.template,
                has_albedo: textures.albedo.is_some(),
                has_roughness: textures.roughness.is_some(),
                has_metallic: textures.metallic.is_some(),
                has_occlusion: textures.occlusion.is_some(),
            });
        }
        let Some(maps) = load_file_maps(textures) else {
            self.failed.insert(key);
            return None;
        };
        let set_flags = (
            maps.has_albedo,
            maps.has_roughness,
            maps.has_metallic,
            maps.has_occlusion,
        );
        let material = asset_material(maps.albedo, Some(maps.normal), Some(maps.orm));
        let template = self.add_entry(renderer, key, 0, &material);
        Some(TextureSet {
            template,
            has_albedo: set_flags.0,
            has_roughness: set_flags.1,
            has_metallic: set_flags.2,
            has_occlusion: set_flags.3,
        })
    }

    /// The engine layer for a procedurally generated RGBA8 albedo (terrain
    /// biome bakes). `stamp` is the content fingerprint: an entry with a
    /// stale stamp is re-uploaded in place, so the key stays stable per
    /// object while the image follows edits.
    pub fn resolve_baked_albedo(
        &mut self,
        renderer: &mut Renderer,
        key: &str,
        stamp: u64,
        bake: impl FnOnce() -> (u32, u32, Vec<u8>),
    ) -> TextureSet {
        self.ensure_init(renderer);
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(key) {
            if entry.stamp == stamp {
                entry.last_used = frame;
                entry.pending = None;
                return TextureSet {
                    template: entry.template,
                    has_albedo: true,
                    has_roughness: false,
                    has_metallic: false,
                    has_occlusion: false,
                };
            }
            // stale bake: wait for the stamp to hold still (a slider drag
            // changes it every frame) before paying for the re-bake; the
            // old colors stay up meanwhile
            entry.last_used = frame;
            match entry.pending {
                Some((pending, since)) if pending == stamp => {
                    if frame.saturating_sub(since) < REBAKE_DEBOUNCE {
                        return TextureSet {
                            template: entry.template,
                            has_albedo: true,
                            has_roughness: false,
                            has_metallic: false,
                            has_occlusion: false,
                        };
                    }
                }
                _ => {
                    entry.pending = Some((stamp, frame));
                    return TextureSet {
                        template: entry.template,
                        has_albedo: true,
                        has_roughness: false,
                        has_metallic: false,
                        has_occlusion: false,
                    };
                }
            }
            // debounce expired: replace the maps in the SAME slot (indices
            // already uploaded inside instances stay valid)
            let (w, h, rgba) = bake();
            let material =
                asset_material(Texture2D::from_rgba8(w as usize, h as usize, &rgba), None, None);
            let template = match renderer.update_material(entry.slot, &material) {
                Some(template) => {
                    entry.template = template;
                    entry.stamp = stamp;
                    entry.pending = None;
                    entry.last_used = frame;
                    template
                }
                None => {
                    // slot went stale (set was reset): re-add the same maps
                    self.entries.remove(key);
                    self.add_entry(renderer, key.to_string(), stamp, &material)
                }
            };
            return TextureSet {
                template,
                has_albedo: true,
                has_roughness: false,
                has_metallic: false,
                has_occlusion: false,
            };
        }
        let (w, h, rgba) = bake();
        let material =
            asset_material(Texture2D::from_rgba8(w as usize, h as usize, &rgba), None, None);
        let template = self.add_entry(renderer, key.to_string(), stamp, &material);
        TextureSet {
            template,
            has_albedo: true,
            has_roughness: false,
            has_metallic: false,
            has_occlusion: false,
        }
    }

    fn add_entry(
        &mut self,
        renderer: &mut Renderer,
        key: String,
        stamp: u64,
        material: &AssetMaterial,
    ) -> GpuMaterial {
        let (slot, template) = renderer.add_material(material);
        self.entries.insert(
            key,
            Entry { slot, template, stamp, last_used: self.frame, pending: None },
        );
        template
    }
}

/// Wrap maps into the engine's material record with NEUTRAL params — the
/// scalar constants are overwritten per object by `gpu_material` anyway;
/// only the maps (and the layer they land in) matter here.
///
/// Every map is brought to the ALBEDO's resolution first: the engine sizes
/// a material by its albedo and skips its own resample when that matches
/// the working resolution — mismatched sibling maps would then be uploaded
/// as-is into full-size layers (a silent validation failure that samples
/// as garbage).
fn asset_material(
    albedo: Texture2D,
    normal: Option<Texture2D>,
    orm: Option<Texture2D>,
) -> AssetMaterial {
    let (w, h) = (albedo.width(), albedo.height());
    let fit = |t: Texture2D, filter: MipFilter| {
        if (t.width(), t.height()) == (w, h) {
            t
        } else {
            t.resized(w, h, filter)
        }
    };
    AssetMaterial {
        name: "bridged",
        maps: MaterialMaps {
            albedo,
            normal: fit(
                normal.unwrap_or_else(|| flat_normal(1, 1)),
                MipFilter::Normal,
            ),
            orm: fit(orm.unwrap_or_else(|| neutral_orm(1, 1)), MipFilter::Linear),
            height: Texture2D::filled(w, h, 1, 0.5),
            emissive: None,
        },
        params: MaterialParams::default(),
    }
}

fn flat_normal(w: usize, h: usize) -> Texture2D {
    Texture2D::from_fn(w, h, 3, |_, _| [0.5, 0.5, 1.0, 0.0])
}

fn neutral_orm(w: usize, h: usize) -> Texture2D {
    Texture2D::filled(w, h, 3, 1.0)
}

/// Decode a grayscale channel map to a Texture2D (first channel used).
fn decode(key: &str) -> Option<Texture2D> {
    let bytes = crate::pbr_library::load_texture_bytes(key)?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = image.dimensions();
    Some(Texture2D::from_rgba8(w as usize, h as usize, image.as_raw()))
}

fn load_file_maps(textures: &modeler_core::MaterialTextures) -> Option<ResolvedMaps> {
    let albedo = textures.albedo.as_deref().and_then(decode);
    let normal = textures.normal.as_deref().and_then(decode);
    let rough = textures.roughness.as_deref().and_then(decode);
    let metal = textures.metallic.as_deref().and_then(decode);
    let occ = textures.occlusion.as_deref().and_then(decode);
    if albedo.is_none()
        && normal.is_none()
        && rough.is_none()
        && metal.is_none()
        && occ.is_none()
    {
        return None; // nothing on disk: keep the scalar material
    }
    let has = (
        albedo.is_some(),
        normal.is_some(),
        rough.is_some(),
        metal.is_some(),
        occ.is_some(),
    );

    // Compose ORM: R = occlusion, G = roughness, B = metallic. Missing
    // channels are 1.0 — a no-op under the shader's multiply. Inputs may
    // disagree on resolution, so everything is resampled to the largest.
    let orm = if has.2 || has.3 || has.4 {
        let (w, h) = [&rough, &metal, &occ]
            .iter()
            .filter_map(|t| t.as_ref())
            .map(|t| (t.width(), t.height()))
            .max()
            .unwrap_or((1, 1));
        let sized = |t: &Option<Texture2D>| {
            t.as_ref().map(|t| t.resized(w, h, MipFilter::Linear))
        };
        let (r, g, b) = (sized(&occ), sized(&rough), sized(&metal));
        let mut orm = Texture2D::new(w, h, 3);
        for y in 0..h {
            for x in 0..w {
                let ch = |t: &Option<Texture2D>| t.as_ref().map_or(1.0, |t| t.get(x, y)[0]);
                orm.set(x, y, [ch(&r), ch(&g), ch(&b), 0.0]);
            }
        }
        orm
    } else {
        neutral_orm(1, 1)
    };

    Some(ResolvedMaps {
        albedo: albedo.unwrap_or_else(|| Texture2D::filled(1, 1, 3, 1.0)),
        normal: normal.unwrap_or_else(|| flat_normal(1, 1)),
        orm,
        has_albedo: has.0,
        has_normal: has.1,
        has_roughness: has.2,
        has_metallic: has.3,
        has_occlusion: has.4,
    })
}
