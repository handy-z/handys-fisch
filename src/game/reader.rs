use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use handy_sdk::app::updater_thread::Session;
use handy_sdk::roblox::instance::{
    child_by_class, child_by_name, children, data_model, instance_class, instance_name, read_string,
};
use handy_sdk::roblox::memory::Memory;
use handy_sdk::roblox::mesh::{Mesh, cached_meshes, part_frame, part_mesh_id};
use handy_sdk::roblox::offsets::Offsets;
use handy_sdk::roblox::read::{
    CachedPath, VisibleProbe, character_of, gui_image, gui_layout_order, gui_size, gui_text, int_value, local_player,
    number_value, object_value, parent_of, part_position, player_gui, string_value,
};

use handy_sdk::ui::raster::{Scene, Tri};

use super::catch::{NEON, asset_id, model_tint, parse_xp};

/// How a catch popup shows the catch; see GameReader::catch_look.
#[derive(Debug, Default)]
pub struct CatchLook {
    pub icon: Option<String>,
    pub tint: Option<u32>,
    /// the popup's 3D model in its camera's space (render() takes it as is); empty when the mesh offsets are missing
    /// or nothing could be read
    pub model: Vec<Tri>,
    /// the popup's camera and light
    pub scene: Scene,
    /// the model's texture assets ("rbxassetid://1"): a Tri's tex is 1 + an index here
    pub textures: Vec<String>,
}

// shortcut: a ViewportFrame's lighting isn't in the offset dump; found next to its CurrentCamera pointer (0x668) on
// version-4ea8e85481e34f46. Values out of range fall back to the defaults; search again when the fish look unlit.
const VP_AMBIENT: usize = 0x6a8;
const VP_LIGHT_COLOR: usize = 0x6c0;
const VP_LIGHT_DIRECTION: usize = 0x6cc;
const LOOK_DOWN: f32 = 0.4; // radians the fallback camera looks down at the model from

/// A part's (rgb, transparency, material), for model_tint.
type PartLook = (u32, f32, u16);

/// A model's solid parts (under `kids`, at most 64; meshes from `dm`'s caches) as world-space triangles, painted by material, with the MeshParts'
/// texture assets (a Tri's tex is 1 + an index) and, when `recolored`, every part's (rgb, transparency, material)
/// for model_tint. Empty triangles when no body mesh was read.
fn model_tris(
    m: &dyn Memory,
    o: &Offsets,
    dm: usize,
    kids: Vec<usize>,
    recolored: bool,
) -> (Vec<Tri>, Vec<String>, Vec<PartLook>) {
    let p = m;
    let mut parts = Vec::new();
    let mut meshes = Vec::new(); // (mesh id, rotation, position, size, paint)
    let mut textures: Vec<String> = Vec::new(); // the MeshParts' TextureIDs; a Tri's tex is 1 + an index here
    let mut boxes = Vec::new();
    let mut solids = 0;
    let mut stack = kids;
    while let Some(c) = stack.pop() {
        if solids >= 64 {
            break; // plenty to average and draw; a huge model stays cheap
        }
        // every solid part kind: Piranha and Anomalocaris bodies are unions (PartOperation)
        let class = instance_class(p, o, c);
        if matches!(
            class.as_deref(),
            Some("Part" | "MeshPart" | "PartOperation" | "UnionOperation" | "WedgePart" | "CornerWedgePart")
        ) {
            let rgb = m.get::<[u8; 4]>(o.at(c, "BasePart.Color3")).unwrap_or_default();
            let prim = m.get::<usize>(o.at(c, "BasePart.Primitive")).unwrap_or(0);
            let rgb = u32::from_be_bytes([0, rgb[0], rgb[1], rgb[2]]);
            let transparency = m.get::<f32>(o.at(c, "BasePart.Transparency")).unwrap_or(1.0);
            let material = if prim == 0 {
                0
            } else {
                m.get::<u16>(o.at(prim, "Primitive.Material")).unwrap_or(0)
            };
            solids += 1;
            if recolored {
                parts.push((rgb, transparency, material));
            }
            // fully transparent parts (the Hitbox) aren't drawn by the game either
            if transparency < 1.0
                && let Some(id) = part_mesh_id(m, o, c)
                && let Some((rot, pos, size)) = part_frame(m, o, c)
            {
                let tex = mesh_texture(m, o, c).map_or(0, |t| {
                    let i = textures.iter().position(|x| *x == t).unwrap_or_else(|| {
                        textures.push(t);
                        textures.len() - 1
                    });
                    i as u32 + 1
                });
                meshes.push((
                    id,
                    rot,
                    pos,
                    size,
                    Tri {
                        tex,
                        ..material_look(rgb, material, transparency)
                    },
                ));
            } else if transparency < 1.0
                && matches!(class.as_deref(), Some("Part" | "WedgePart"))
                && let Some((rot, pos, size)) = part_frame(m, o, c)
            {
                // an optional key: without it every plain Part is a block
                let shape = if class.as_deref() == Some("WedgePart") {
                    3
                } else if o.values.contains_key("BasePart.Shape") {
                    m.get::<u8>(o.at(c, "BasePart.Shape")).unwrap_or(1)
                } else {
                    1
                };
                let (mesh, size) = shape_mesh(shape, size);
                boxes.extend(part_tris(
                    mesh,
                    false,
                    &rot,
                    pos,
                    size,
                    material_look(rgb, material, transparency),
                ));
            }
        }
        stack.extend(children(p, o, c));
    }
    let mut model = Vec::new();
    if !meshes.is_empty() {
        let ids: Vec<(u64, bool)> = meshes.iter().map(|x| x.0).collect();
        let cache = cached_meshes(m, o, dm, &ids);
        for (id, rot, pos, size, paint) in &meshes {
            if let Some(mesh) = cache.get(&id.0) {
                model.extend(part_tris(mesh, id.1, rot, *pos, *size, *paint));
            }
        }
    }
    if !model.is_empty() {
        model.extend(boxes); // the eyes of a fish whose body was read; alone they are no picture
    }
    (model, textures, parts)
}

/// The popup's camera (position, row-major rotation whose columns are right, up and back) and Scene: its Camera child
/// and lighting, else a camera looking down at the model's center like the dashboard's old view and the
/// ViewportFrame's default light.
fn popup_view(m: &dyn Memory, o: &Offsets, vp: usize, model: &[Tri]) -> (Scene, [f32; 3], [f32; 9]) {
    let mut scene = Scene::default();
    let color = |off: usize| {
        m.get::<[f32; 3]>(vp + off)
            .filter(|c| c.iter().all(|v| (0.0..=1.0).contains(v)))
    };
    if vp != 0 {
        if let Some(c) = color(VP_AMBIENT) {
            scene.ambient = c;
        }
        if let Some(c) = color(VP_LIGHT_COLOR) {
            scene.color = c;
        }
        if let Some(d) = m
            .get::<[f32; 3]>(vp + VP_LIGHT_DIRECTION)
            .filter(|d| d.iter().all(|v| v.is_finite()) && d.iter().any(|v| *v != 0.0))
        {
            scene.dir = d;
        }
    }
    let cam = children(m, o, vp)
        .into_iter()
        .find(|c| instance_class(m, o, *c).as_deref() == Some("Camera"));
    let read = cam.and_then(|c| {
        let pos = m.get::<[f32; 3]>(o.at(c, "Camera.Position"))?;
        let rot = m.get::<[f32; 9]>(o.at(c, "Camera.Rotation"))?;
        let fov = m.get::<f32>(o.at(c, "Camera.FieldOfView"))?;
        let ok = pos.iter().chain(&rot).all(|v| v.is_finite()) && (0.01..3.0).contains(&fov);
        ok.then_some((pos, rot, fov))
    });
    let world_dir = scene.dir;
    let (eye, rot) = match read {
        Some((pos, rot, fov)) => {
            scene.fov = fov;
            (pos, rot)
        }
        None => {
            let (lo, hi) = model
                .iter()
                .flat_map(|t| t.p)
                .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), v| {
                    (
                        std::array::from_fn(|k| lo[k].min(v[k])),
                        std::array::from_fn(|k| hi[k].max(v[k])),
                    )
                });
            let (sn, cs) = LOOK_DOWN.sin_cos();
            // orthographic: only the direction matters, so the eye can sit at the center
            let center = std::array::from_fn(|k| (lo[k] + hi[k]) / 2.0);
            (center, [1.0, 0.0, 0.0, 0.0, cs, sn, 0.0, -sn, cs])
        }
    };
    scene.dir = turn_into(&rot, world_dir); // Roblox gives it in world space
    (scene, eye, rot)
}

/// World `v` (a direction) in camera space: its dot with the rotation's columns.
fn turn_into(rot: &[f32; 9], v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|c| (0..3).map(|r| rot[3 * r + c] * v[r]).sum())
}

fn to_camera(t: &Tri, eye: [f32; 3], rot: &[f32; 9]) -> Tri {
    Tri {
        p: t.p.map(|v| turn_into(rot, std::array::from_fn(|k| v[k] - eye[k]))),
        n: t.n.map(|n| turn_into(rot, n)),
        ..*t
    }
}

// Roblox Material enum values (as Primitive.Material holds them)
const PLASTIC: u16 = 256;
const SMOOTH_PLASTIC: u16 = 272;
const MARBLE: u16 = 784;
const CORRODED_METAL: u16 = 1040;
const DIAMOND_PLATE: u16 = 1056;
const FOIL: u16 = 1072;
const METAL: u16 = 1088;
const ICE: u16 = 1536;
const GLACIER: u16 = 1552;
const GLASS: u16 = 1568;
const FORCE_FIELD: u16 = 1584;
const FORCE_FIELD_CLEAR: f32 = 0.85; // how much a ForceField lets through: the game's is a faint speckle

/// How a part of color `rgb`, its material and Transparency is painted, as Roblox draws them: Neon glows, ForceField
/// is a faint rippling see-through speckle, Ice and Glacier glossy, Glass
/// paler and glossy, metals and smooth plastic with a glint; the part's own Transparency on top.
fn material_look(rgb: u32, material: u16, transparency: f32) -> Tri {
    let mix = |to: [f32; 3], k: f32| {
        let c = [16, 8, 0].map(|s| ((rgb >> s) & 0xff) as f32);
        let m: [u32; 3] = std::array::from_fn(|i| (c[i] + (to[i] - c[i]) * k).round() as u32);
        (m[0] << 16) | (m[1] << 8) | m[2]
    };
    let mut t = Tri { rgb, ..Tri::default() };
    match material {
        NEON => t.glow = true,
        FORCE_FIELD => {
            t.clear = FORCE_FIELD_CLEAR;
            t.shimmer = true;
        }
        ICE | GLACIER => t.shine = 0.7, // the game keeps their color: only the gloss shows
        GLASS => {
            t.rgb = mix([255.0; 3], 0.2);
            t.shine = 0.9;
        }
        METAL | DIAMOND_PLATE | FOIL => t.shine = 0.6,
        CORRODED_METAL | MARBLE => t.shine = 0.25,
        SMOOTH_PLASTIC => t.shine = 0.25,
        PLASTIC => t.shine = 0.1,
        _ => {}
    }
    // both let light through: what passes one passes the other
    let part = transparency.clamp(0.0, 1.0);
    t.clear = 1.0 - (1.0 - t.clear) * (1.0 - part);
    t
}

/// A MeshPart's texture: its SurfaceAppearance's ColorMap when it has one (that replaces the TextureID in Roblox),
/// else its TextureID ("rbxassetid://1"); None without either.
fn mesh_texture(m: &dyn Memory, o: &Offsets, part: usize) -> Option<String> {
    if !o.values.contains_key("MeshPart.Texture") || instance_class(m, o, part).as_deref() != Some("MeshPart") {
        return None;
    }
    let content = |at: usize| read_string(m, o, at).filter(|t| asset_id(t).is_some());
    if o.values.contains_key("SurfaceAppearance.ColorMap")
        && let Some(sa) = children(m, o, part)
            .into_iter()
            .find(|c| instance_class(m, o, *c).as_deref() == Some("SurfaceAppearance"))
        && let Some(map) = content(o.at(sa, "SurfaceAppearance.ColorMap"))
    {
        return Some(map);
    }
    content(o.at(part, "MeshPart.Texture"))
}

/// A unit cube, for plain Parts.
static BLOCK: LazyLock<Mesh> = LazyLock::new(|| Mesh {
    positions: (0..8)
        .map(|i| [0, 1, 2].map(|k| if i >> k & 1 == 1 { 0.5 } else { -0.5 }))
        .collect(),
    tris: [
        [0, 1, 3],
        [0, 3, 2],
        [4, 6, 7],
        [4, 7, 5],
        [0, 4, 5],
        [0, 5, 1],
        [2, 3, 7],
        [2, 7, 6],
        [0, 2, 6],
        [0, 6, 4],
        [1, 5, 7],
        [1, 7, 3],
    ]
    .to_vec(),
    normals: Vec::new(),
    uvs: Vec::new(),
    colors: Vec::new(),
});

/// A unit-diameter UV sphere with smooth normals, for Ball Parts.
static BALL: LazyLock<Mesh> = LazyLock::new(|| {
    const LON: u32 = 12;
    const LAT: u32 = 8;
    let mut positions = Vec::new();
    for i in 0..=LAT {
        let (s, c) = (std::f32::consts::PI * i as f32 / LAT as f32).sin_cos();
        for j in 0..LON {
            let (sj, cj) = (std::f32::consts::TAU * j as f32 / LON as f32).sin_cos();
            positions.push([s * cj / 2.0, c / 2.0, s * sj / 2.0]);
        }
    }
    let at = |i: u32, j: u32| i * LON + j % LON;
    let tris = (0..LAT)
        .flat_map(|i| {
            (0..LON).flat_map(move |j| {
                [
                    [at(i, j), at(i + 1, j), at(i + 1, j + 1)],
                    [at(i, j), at(i + 1, j + 1), at(i, j + 1)],
                ]
            })
        })
        .collect();
    let normals = positions.iter().map(|v| v.map(|c| c * 2.0)).collect();
    Mesh {
        positions,
        normals,
        uvs: Vec::new(),
        colors: Vec::new(),
        tris,
    }
});

/// A unit cylinder along X (as Roblox's run), smooth sides and flat caps, for Cylinder Parts.
static CYLINDER: LazyLock<Mesh> = LazyLock::new(|| {
    const SEG: u32 = 16;
    let (mut positions, mut normals, mut tris) = (Vec::new(), Vec::new(), Vec::new());
    // two rings with radial normals for the side, then the same two again facing along X for the caps
    for cap in [false, true] {
        for x in [-0.5f32, 0.5] {
            for j in 0..SEG {
                let (s, c) = (std::f32::consts::TAU * j as f32 / SEG as f32).sin_cos();
                positions.push([x, c / 2.0, s / 2.0]);
                normals.push(if cap { [x * 2.0, 0.0, 0.0] } else { [0.0, c, s] });
            }
        }
    }
    for j in 0..SEG {
        let k = (j + 1) % SEG;
        tris.push([j, SEG + j, SEG + k]);
        tris.push([j, SEG + k, k]);
    }
    for cap in [2 * SEG, 3 * SEG] {
        for j in 1..SEG - 1 {
            tris.push([cap, cap + j, cap + j + 1]); // a fan: the cap is convex
        }
    }
    Mesh {
        positions,
        normals,
        uvs: Vec::new(),
        colors: Vec::new(),
        tris,
    }
});

/// Roblox's WedgePart in a unit box: full bottom, tall face at the back (+Z), the slope falling from the top-back edge
/// to the bottom-front (-Z, the LookVector side) edge.
static WEDGE: LazyLock<Mesh> = LazyLock::new(|| Mesh {
    positions: vec![
        [-0.5, -0.5, -0.5],
        [0.5, -0.5, -0.5],
        [-0.5, -0.5, 0.5],
        [0.5, -0.5, 0.5],
        [-0.5, 0.5, 0.5],
        [0.5, 0.5, 0.5],
    ],
    normals: Vec::new(),
    uvs: Vec::new(),
    colors: Vec::new(),
    tris: vec![
        [0, 1, 3],
        [0, 3, 2], // bottom
        [2, 3, 5],
        [2, 5, 4], // back
        [0, 1, 5],
        [0, 5, 4], // slope
        [0, 2, 4], // left
        [1, 3, 5], // right
    ],
});

/// A plain Part's unit mesh and the size to fit it to, by its Enum.PartType (0 Ball, 1 Block, 2 Cylinder, 3 Wedge,
/// 4 CornerWedge): balls are spheres of the smallest side, cylinders run along X as wide as the smaller of Y and Z.
fn shape_mesh(shape: u8, size: [f32; 3]) -> (&'static Mesh, [f32; 3]) {
    let d = size[1].min(size[2]);
    match shape {
        0 => (&BALL, [d.min(size[0]); 3]),
        2 => (&CYLINDER, [size[0], d, d]),
        3 => (&WEDGE, size),
        _ => (&BLOCK, size), // shortcut: CornerWedge drawn as a block; give it its own mesh if one shows up big
    }
}

/// A part's mesh in world space: fitted to the part's size per axis around its bounding-box center (meshes come in
/// their own units, centered on the part's CFrame), then turned and moved by the part's rotation and position.
/// `paint` gives every triangle its color, glow, see_through and texture (tex, used when the mesh has uvs).
fn part_tris(mesh: &Mesh, union: bool, rot: &[f32; 9], pos: [f32; 3], size: [f32; 3], paint: Tri) -> Vec<Tri> {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for v in &mesh.positions {
        for k in 0..3 {
            lo[k] = lo[k].min(v[k]);
            hi[k] = hi[k].max(v[k]);
        }
    }
    let scale: [f32; 3] = std::array::from_fn(|k| {
        let extent = hi[k] - lo[k];
        if extent > 1e-6 { size[k] / extent } else { 1.0 } // flat on that axis: nothing to fit
    });
    // a MeshPart mesh is centered on its part; a union's sits around the part's own origin, often off its center
    // (seen live: the Piranha's pieces)
    let origin: [f32; 3] = std::array::from_fn(|k| if union { 0.0 } else { (lo[k] + hi[k]) / 2.0 });
    // world = R * local with R row-major, unions and MeshParts alike (checked live on a held Mastered Dunkleosteus: its
    // 11 union pieces fit together only this way)
    let turn = |l: [f32; 3]| -> [f32; 3] { std::array::from_fn(|r| (0..3).map(|k| rot[3 * r + k] * l[k]).sum()) };
    let world = |v: [f32; 3]| -> [f32; 3] {
        let w = turn(std::array::from_fn(|k| (v[k] - origin[k]) * scale[k]));
        std::array::from_fn(|r| w[r] + pos[r])
    };
    // normals take the inverse transpose of the scale: divided, not multiplied, per axis
    let normal = |i: u32| -> [f32; 3] {
        let Some(n) = mesh.normals.get(i as usize) else {
            return [0.0; 3];
        };
        let w = turn(std::array::from_fn(|k| n[k] / scale[k]));
        let len = w.iter().map(|c| c * c).sum::<f32>().sqrt();
        let u = w.map(|c| c / len);
        if u.iter().all(|c| c.is_finite()) { u } else { [0.0; 3] } // flat
    };
    let corner = |i: u32| mesh.positions.get(i as usize).map(|&v| world(v));
    let uv = |i: u32| mesh.uvs.get(i as usize).copied().unwrap_or_default();
    let vc = |i: u32| mesh.colors.get(i as usize).copied().unwrap_or_default(); // none: alpha 0, drawn white
    mesh.tris
        .iter()
        .filter_map(|t| {
            Some(Tri {
                p: [corner(t[0])?, corner(t[1])?, corner(t[2])?],
                n: t.map(normal),
                uv: t.map(uv),
                vc: t.map(vc),
                tex: if mesh.uvs.is_empty() { 0 } else { paint.tex },
                ..paint
            })
        })
        .collect()
}

// Fisch rod values/state (IntValue), recorded by tools/debug/fish_record.py.
pub const ROD_REEL_RESET: i64 = 2;
pub const ROD_IDLE: i64 = 3;
pub const ROD_LURE: i64 = 6;
pub const ROD_REEL_LOCKED: i64 = 7;
pub const ROD_REELING: i64 = 8;
pub const ROD_REEL_DONE: i64 = 9;

/// The reel's own states (7 -> 8 -> 9 -> 2). Its GUI lingers ~0.9 s into idle and the next cast, so check this too.
pub fn reel_state(s: Option<i64>) -> bool {
    matches!(s, Some(ROD_REEL_LOCKED | ROD_REELING | ROD_REEL_DONE | ROD_REEL_RESET))
}

/// Cast (4, 5) or waiting / shaking (6): the line is out but nothing is hooked yet.
pub fn line_out(s: Option<i64>) -> bool {
    matches!(s, Some(4 | 5 | ROD_LURE))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reel {
    /// X scales in reel/bar; progress 0..1
    pub bar: f32,
    pub bar_width: f32,
    pub fish: f32,
    pub progress: f32,
    /// reel/bar/progressspeed, the game's total for this reel (rod, enchant, bait, the fish's own): "+55% Progress
    /// Speed"; empty while the game hides it
    pub progress_speed: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FishState {
    /// equipped rod name, empty when none
    pub rod: String,
    /// rod values/state
    pub rod_state: Option<i64>,
    /// PlayerGui/shakeui/safezone/<button> X scale while luring
    pub shake_button_x: Option<f32>,
    /// PlayerGui/reel while reeling
    pub reel: Option<Reel>,
    /// not read here: the bot sets it from RobloxWatcher (chat focus)
    pub typing: bool,
    /// the character model: a new one after death / respawn
    pub character: usize,
}

/// (item name, key label) per hotbar slot.
pub type Slots = Vec<(String, String)>;

/// Hotbar key (0-9) of the first rod (in the given priority order) found in a slot. Slots: (item name, key label);
/// item names carry mutation prefixes ("Abyssal Fallen Rod" for the tool "Fallen Rod"), so the exact name wins, then a
/// name ending in the rod's.
pub fn hotbar_key_for(rods: &[String], slots: &[(String, String)]) -> Option<i32> {
    for r in rods {
        // rods in priority order
        let mut prefixed = None;
        for (item, key) in slots {
            let k = key.as_bytes();
            if r.is_empty() || k.len() != 1 || !k[0].is_ascii_digit() {
                continue;
            }
            let digit = i32::from(k[0] - b'0');
            if item == r {
                return Some(digit); // first: "Original No-Life Rod" also ends with "No-Life Rod"
            }
            if prefixed.is_none() && item.ends_with(&format!(" {r}")) {
                prefixed = Some(digit);
            }
        }
        // shortcut: an enchanted No-Life Rod next to an Original No-Life Rod can still pick the wrong one, check against
        // RODS if a player ever owns both.
        if prefixed.is_some() {
            return prefixed;
        }
    }
    None
}

/// "[Momentum] Carrot Rod": the rod with the enchant its hotbar label puts in front ("Momentum Carrot Rod"); just the
/// rod when it has none or isn't on the hotbar.
pub fn enchanted_rod(rod: &str, slots: &[(String, String)]) -> String {
    if slots.iter().any(|(item, _)| item == rod) {
        return rod.into();
    }
    for (item, _) in slots {
        if !rod.is_empty() && item.len() > rod.len() + 1 && item.ends_with(&format!(" {rod}")) {
            return format!("[{}] {rod}", &item[..item.len() - rod.len() - 1]);
        }
    }
    rod.into()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelProgress {
    pub level: i32,
    pub xp: f64,
}

/// The player beyond fishing, for the dashboard. Each field stays empty / 0 when it can't be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInfo {
    /// Stats/rod ("Carrot Rod")
    pub rod: String,
    /// enchanted_rod() of it
    pub rod_label: String,
    /// The rod's live stats from the game's equipment panel, enchant bonuses included, in its order:
    /// "Lure Speed: 85% (+28%)", "Luck: 125%", ... "Caught: 737". Empty when the panel has no such rod.
    pub rod_stats: Vec<String>,
    /// Stats/bait, Cache/CurrentWorld ("Sea 1"), the character's zone
    pub bait: String,
    pub world: String,
    pub zone: String,
    /// HumanoidRootPart, studs
    pub pos: Option<[f32; 3]>,
    /// overall, from the player list ("14%")
    pub bestiary: String,
    /// Stats/tracker_*
    pub fish_caught: f64,
    pub shiny: f64,
    pub sparkling: f64,
    pub seconds_played: f64,
}

/// Server-wide conditions from ReplicatedStorage/world, for the dashboard. Each field stays empty when it can't be read;
/// the game's "None" is left out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct World {
    /// weather ("Windy"), then its active secondary weathers (squall, sovereign, meteorological, astral)
    pub weather: Vec<String>,
    /// cycle ("Day" / "Night"), season ("Winter")
    pub cycle: String,
    pub season: String,
    /// event, admin_event, then "Meteor" / "Blizzard" while those are on
    pub events: Vec<String>,
    /// server multipliers that aren't 1: ("Luck", 2.0)
    pub boosts: Vec<(&'static str, f64)>,
    /// the game's version ("2.09.2")
    pub version: String,
}

/// world/<value> -> what the dashboard calls the multiplier
const BOOSTS: [(&str, &str); 7] = [
    ("luck_Luck", "Luck"),
    ("luck_Server", "Server luck"),
    ("luck_Shiny", "Shiny"),
    ("luck_Sparkling", "Sparkling"),
    ("luck_Mutation", "Mutation"),
    ("luck_Weight", "Weight"),
    ("multiple_XP", "XP"),
];

/// A parent's children by name; the names are read again only when its children change.
#[derive(Default)]
struct NamedKids {
    kids: Vec<usize>,
    by_name: HashMap<String, usize>,
}

impl NamedKids {
    fn refresh(&mut self, m: &dyn Memory, o: &Offsets, parent: usize) -> &HashMap<String, usize> {
        let kids = children(m, o, parent);
        if kids != self.kids {
            self.by_name = kids
                .iter()
                .filter_map(|k| Some((instance_name(m, o, *k)?, *k)))
                .collect();
            self.kids = kids;
        }
        &self.by_name
    }
}

/// `c` re-made when `names` differ from last time (they hold the rod's or the player's name).
fn path_to(m: &dyn Memory, sess: &Session, c: &mut Option<CachedPath>, names: &[&str], root: usize) -> usize {
    if c.as_ref().is_none_or(|c| c.names() != names) {
        *c = Some(CachedPath::new(names));
    }
    c.as_mut().map_or(0, |c| c.get(m, &sess.offsets, root))
}

/// Read-only view of the local player's fishing state. Not thread-safe; one per thread. Every offset it reads is in
/// required_keys(), which the Updater enforces, so none can be missing.
pub struct GameReader {
    sess: Arc<Session>,
    players: usize,
    lp: usize,
    gui: usize,
    stats_owner: usize, // cached PlayerStats/<name>/T/<name> + its Stats folder
    stats: usize,
    prompt_gui: usize, // cached CoreGui/RobloxPromptGui + its promptOverlay
    overlay: usize,
    rod: usize, // cached Tool + its values/state
    rod_state: usize,
    reel: usize, // cached reel ScreenGui + parts
    bar: usize,
    fish: usize,
    prog: usize,
    prog_speed: usize,
    seen_popups: Option<Vec<usize>>, // unset until the first take_catch_popup()
    // Children seen by the last character / PlayerGui scan: sample() reads names again only when a list changed.
    scanned_chr: usize, // the character chr_kids is of
    shake: usize,       // the shakeui that scan found
    chr_kids: Vec<usize>,
    gui_kids: Vec<usize>,
    // Paths read every tick or every poll; each checks its cached nodes instead of reading every name on the way.
    coins: CachedPath,
    xpup: CachedPath,
    // always shown / hidden unless opened: tell a wrong dumped Visible offset
    safezone: CachedPath,
    inventory: CachedPath,
    fade: CachedPath,
    vis: VisibleProbe,
    announcements: CachedPath,
    power: CachedPath,
    feedback: CachedPath,
    level: CachedPath, // in stats()
    xp: CachedPath,
    hotbar: CachedPath,
    world: CachedPath,
    zone: CachedPath,
    root: CachedPath,
    stat_paths: HashMap<String, CachedPath>,
    rod_stats: Option<CachedPath>,
    bestiary: Option<CachedPath>,
    rs_world: CachedPath, // ReplicatedStorage/world, from the DataModel
    world_kids: NamedKids,
    owner_kids: NamedKids,    // stats_owner's: Stats, Settings, ...
    settings_kids: NamedKids, // its Settings
    weather_kids: NamedKids,
}

impl GameReader {
    pub fn new(sess: Arc<Session>) -> Self {
        Self {
            sess,
            players: 0,
            lp: 0,
            gui: 0,
            stats_owner: 0,
            stats: 0,
            prompt_gui: 0,
            overlay: 0,
            rod: 0,
            rod_state: 0,
            reel: 0,
            bar: 0,
            fish: 0,
            prog: 0,
            prog_speed: 0,
            seen_popups: None,
            scanned_chr: 0,
            shake: 0,
            chr_kids: Vec::new(),
            gui_kids: Vec::new(),
            coins: CachedPath::new(&["hud", "safezone", "coins"]),
            xpup: CachedPath::new(&["hud", "safezone", "xpup"]),
            safezone: CachedPath::new(&["hud", "safezone"]),
            inventory: CachedPath::new(&["backpack", "inventory"]),
            fade: CachedPath::new(&["death", "fade"]),
            vis: VisibleProbe::default(),
            announcements: CachedPath::new(&["hud", "safezone", "announcements"]),
            power: CachedPath::new(&["HumanoidRootPart", "power", "powerbar", "bar"]),
            feedback: CachedPath::new(&["HumanoidRootPart", "powerfeedback", "title"]),
            level: CachedPath::new(&["level"]),
            xp: CachedPath::new(&["xp"]),
            hotbar: CachedPath::new(&["backpack", "hotbar"]),
            world: CachedPath::new(&["Cache", "CurrentWorld"]),
            zone: CachedPath::new(&["zone"]),
            root: CachedPath::new(&["HumanoidRootPart"]),
            stat_paths: HashMap::new(),
            rod_stats: None,
            bestiary: None,
            rs_world: CachedPath::new(&["ReplicatedStorage", "world"]),
            world_kids: NamedKids::default(),
            owner_kids: NamedKids::default(),
            settings_kids: NamedKids::default(),
            weather_kids: NamedKids::default(),
        }
    }

    /// Hotbar key of a rod in the Backpack, `preferred` first if it's there; slow (walks the Backpack).
    pub fn rod_hotkey(&mut self, preferred: &str) -> Option<i32> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut rods = Vec::new();
        let bp = child_by_class(p, o, self.lp, "Backpack");
        if bp != 0 {
            for c in children(p, o, bp) {
                if instance_class(p, o, c).as_deref() == Some("Tool") && child_by_name(p, o, c, "rodClient") != 0 {
                    rods.push(instance_name(p, o, c).unwrap_or_default());
                }
            }
        }
        let (mut first, rest): (Vec<String>, Vec<String>) = rods.into_iter().partition(|r| r == preferred);
        first.extend(rest); // a stable partition
        if first.is_empty() {
            None
        } else {
            hotbar_key_for(&first, &self.hotbar_slots())
        }
    }

    /// e.g. the equipped rod's key, to unequip it
    pub fn hotbar_key_of(&mut self, item: &str) -> Option<i32> {
        if self.resolve_player() {
            hotbar_key_for(&[item.to_owned()], &self.hotbar_slots())
        } else {
            None
        }
    }

    /// (item name, key label) per hotbar slot
    fn hotbar_slots(&mut self) -> Slots {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let bar = self.hotbar.get(p, o, self.gui);
        let text = |slot: usize, label: &str| gui_text(p, o, child_by_name(p, o, slot, label)).unwrap_or_default();
        let mut slots = Vec::new();
        for s in if bar != 0 { children(p, o, bar) } else { Vec::new() } {
            if instance_name(p, o, s).as_deref() == Some("ItemTemplate") {
                slots.push((text(s, "ItemName"), text(s, "TextLabel")));
            }
        }
        slots
    }

    /// The HUD's balance text, e.g. "4,105,298 C$" (hud/safezone/coins)
    pub fn coins(&mut self) -> Option<String> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_text(p, o, self.coins.get(p, o, self.gui))
    }

    /// Roblox's disconnect / kick dialog is up (CoreGui/RobloxPromptGui/promptOverlay/ErrorPrompt)
    pub fn error_prompt(&mut self) -> bool {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        if !self.parent_is(self.overlay, self.prompt_gui) {
            // CoreGui/RobloxPromptGui/promptOverlay
            let dm = data_model(p, o);
            let cg = if dm != 0 {
                child_by_class(p, o, dm, "CoreGui")
            } else {
                0
            };
            self.prompt_gui = if cg != 0 {
                child_by_name(p, o, cg, "RobloxPromptGui")
            } else {
                0
            };
            self.overlay = if self.prompt_gui != 0 {
                child_by_name(p, o, self.prompt_gui, "promptOverlay")
            } else {
                0
            };
        }
        self.overlay != 0 && child_by_name(p, o, self.overlay, "ErrorPrompt") != 0
    }

    /// Workspace/PlayerStats/<name>/T/<name>/Stats, cached; 0 if not there. After resolve_player().
    fn stats(&mut self) -> usize {
        if !self.parent_is(self.stats, self.stats_owner) {
            // replicated by the game
            let sess = self.sess.clone();
            let (p, o) = (&sess.proc, &sess.offsets);
            let me = instance_name(p, o, self.lp).unwrap_or_default();
            let dm = if me.is_empty() { 0 } else { data_model(p, o) };
            let mut i = if dm != 0 {
                child_by_class(p, o, dm, "Workspace")
            } else {
                0
            };
            for n in ["PlayerStats", me.as_str(), "T", me.as_str()] {
                i = if i != 0 { child_by_name(p, o, i, n) } else { 0 };
            }
            self.stats_owner = i;
            self.stats = if i != 0 { child_by_name(p, o, i, "Stats") } else { 0 };
        }
        self.stats
    }

    /// The game's own level + XP into it (Workspace/PlayerStats/...)
    pub fn level_progress(&mut self) -> Option<LevelProgress> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let level = if self.stats() != 0 {
            self.level.get(p, o, self.stats)
        } else {
            0
        };
        let level = number_value(p, o, level);
        // xp: into the current level, +each catch's popup XP
        let xp = if self.stats() != 0 {
            self.xp.get(p, o, self.stats)
        } else {
            0
        };
        let xp = number_value(p, o, xp);
        match (level, xp) {
            (Some(level), Some(xp)) if level >= 1.0 => Some(LevelProgress {
                level: level as i32,
                xp,
            }),
            _ => None,
        }
    }

    /// Stats/<name>, cached per name; 0 if not there
    fn stat(&mut self, name: &str) -> usize {
        if self.stats() == 0 {
            return 0;
        }
        let sess = self.sess.clone();
        self.stat_paths
            .entry(name.to_owned())
            .or_insert_with(|| CachedPath::new(&[name]))
            .get(&sess.proc, &sess.offsets, self.stats)
    }

    /// Slow (walks the hotbar): not while reeling.
    pub fn player_info(&mut self) -> Option<PlayerInfo> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut info = PlayerInfo::default();
        let text = |me: &mut Self, n: &str| string_value(p, o, me.stat(n)).unwrap_or_default();
        info.rod = text(self, "rod");
        info.rod_label = enchanted_rod(&info.rod, &self.hotbar_slots());
        // hud/.../ScrollingFrame/<rod>/Stats/<stat>/Label, always loaded
        let stats = if info.rod.is_empty() {
            0
        } else {
            let names = [
                "hud",
                "safezone",
                "equipment",
                "Container",
                "Rods",
                "Main",
                "ScrollingFrame",
                info.rod.as_str(),
                "Stats",
            ];
            path_to(p, &sess, &mut self.rod_stats, &names, self.gui)
        };
        let mut lines: Vec<(i32, String)> = Vec::new(); // LayoutOrder, text
        for s in if stats != 0 { children(p, o, stats) } else { Vec::new() } {
            if !self.vis.read(p, o, s).unwrap_or(false) {
                continue; // hidden: Disturbance...
            }
            if let Some(t) = gui_text(p, o, child_by_name(p, o, s, "Label")).filter(|t| !t.is_empty()) {
                lines.push((gui_layout_order(p, o, s).unwrap_or(0), t));
            }
        }
        lines.sort_by_key(|l| l.0); // stable
        info.rod_stats = lines.into_iter().map(|l| l.1).collect();
        info.bait = text(self, "bait");
        // Cache is Stats' sibling
        info.world = string_value(p, o, self.world.get(p, o, self.stats_owner)).unwrap_or_default();
        let num = |me: &mut Self, n: &str| number_value(p, o, me.stat(n)).unwrap_or(0.0);
        info.fish_caught = num(self, "tracker_fishcaught");
        info.shiny = num(self, "tracker_shinycaught");
        info.sparkling = num(self, "tracker_sparklingcaught");
        info.seconds_played = num(self, "tracker_timeplayed");

        let chr = character_of(p, o, self.lp);
        if chr != 0 {
            let zone = self.zone.get(p, o, chr); // ObjectValue -> the zone Part the game puts you in
            let z = object_value(p, o, zone);
            if z != 0 {
                info.zone = instance_name(p, o, z).unwrap_or_default();
            }
            if let Some(pos) = part_position(p, o, self.root.get(p, o, chr)) {
                info.pos = Some([pos.x, pos.y, pos.z]);
            }
        }

        // the player list shows everyone's overall bestiary %
        let me = instance_name(p, o, self.lp).unwrap_or_default();
        let row = path_to(
            p,
            &sess,
            &mut self.bestiary,
            &["PlayerList", "Panel", "List", me.as_str(), "Bestiary"],
            self.gui,
        );
        info.bestiary = gui_text(p, o, row).unwrap_or_default();
        Some(info)
    }

    /// The game's Sell All filters, from the player's Settings (Stats' sibling): willautosell_<rarity> as
    /// ("legendary", sells); None when unreadable. Only the higher rarities have one; ~16 reads.
    pub fn sell_filter(&mut self) -> Option<HashMap<String, bool>> {
        if !self.resolve_player() || self.stats() == 0 {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let m: &dyn Memory = p;
        let settings = *self.owner_kids.refresh(p, o, self.stats_owner).get("Settings")?;
        let filter: HashMap<String, bool> = self
            .settings_kids
            .refresh(p, o, settings)
            .iter()
            .filter_map(|(n, c)| {
                let rarity = n.strip_prefix("willautosell_")?;
                Some((rarity.to_owned(), m.get::<u8>(o.at(*c, "Misc.Value"))? != 0))
            })
            .collect();
        (!filter.is_empty()).then_some(filter)
    }

    /// Weather, time, season, events and server boosts (ReplicatedStorage/world); ~40 reads.
    pub fn world(&mut self) -> Option<World> {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let w = self.rs_world.get(p, o, data_model(p, o));
        if w == 0 {
            return None;
        }
        let k = self.world_kids.refresh(p, o, w);
        let text = |k: &HashMap<String, usize>, n: &str| {
            k.get(n)
                .and_then(|v| string_value(p, o, *v))
                .filter(|s| !s.is_empty() && s != "None")
        };
        let m: &dyn Memory = p;
        let on = |n: &str| {
            k.get(n)
                .and_then(|v| m.get::<u8>(o.at(*v, "Misc.Value")))
                .is_some_and(|b| b != 0)
        };
        let mut out = World {
            cycle: text(k, "cycle").unwrap_or_default(),
            season: text(k, "season").unwrap_or_default(),
            version: text(k, "version").unwrap_or_default(),
            ..World::default()
        };
        out.weather.extend(text(k, "weather"));
        out.events
            .extend(["event", "admin_event"].iter().filter_map(|n| text(k, n)));
        for (n, label) in [("meteor_active", "Meteor"), ("blizzard_active", "Blizzard")] {
            if on(n) {
                out.events.push(label.into());
            }
        }
        for (n, label) in BOOSTS {
            if let Some(x) = k.get(n).and_then(|v| number_value(p, o, *v))
                && x > 0.0
                && (x - 1.0).abs() > 1e-6
            {
                out.boosts.push((label, x));
            }
        }
        let weather = k.get("weather").copied().unwrap_or(0);
        if weather != 0 {
            let wk = self.weather_kids.refresh(p, o, weather);
            for n in ["squall", "sovereign", "meteorological", "astral"] {
                if let Some(t) = text(wk, n)
                    && !out.weather.contains(&t)
                {
                    out.weather.push(t);
                }
            }
        }
        Some(out)
    }

    /// The "+45xp" popup shown at a catch (hud/safezone/xpup), while it's visible
    pub fn catch_xp(&mut self) -> Option<i64> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let i = self.xpup.get(p, o, self.gui);
        if !self.vis.read(p, o, i).unwrap_or(false) {
            return None; // shown per catch
        }
        gui_text(p, o, child_by_name(p, o, i, "xp")).and_then(|t| parse_xp(&t))
    }

    /// Cast power 0..1 (HumanoidRootPart/power/powerbar/bar height) while charging, None otherwise. Call after
    /// sample().
    pub fn cast_power(&mut self) -> Option<f32> {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_size(p, o, self.power.get(p, o, character_of(p, o, self.lp))).map(|s| s.y_scale)
    }

    /// The game's rating over the character (HumanoidRootPart/powerfeedback/title): "PERFECT!" / "Meh.." just after a
    /// cast, "PERFECT CATCH!" after a catch. Shown ~1.4 s. Call after sample().
    pub fn cast_feedback(&mut self) -> Option<String> {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_text(p, o, self.feedback.get(p, o, character_of(p, o, self.lp)))
    }

    /// Text of a catch popup (hud/safezone/announcements/catch, one per catch) not returned before; popups already up
    /// on the first call are skipped. The fish is only revealed here: while reeling the GUI shows a generic icon.
    pub fn take_catch_popup(&mut self) -> Option<(String, usize)> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let i = self.announcements.get(p, o, self.gui);
        if i == 0 {
            return None;
        }
        let popups: Vec<usize> = children(p, o, i)
            .into_iter()
            .filter(|&c| instance_name(p, o, c).as_deref() == Some("catch"))
            .collect();
        let first = self.seen_popups.is_none();
        let seen = self.seen_popups.get_or_insert_with(|| popups.clone());
        seen.retain(|c| popups.contains(c));
        if first {
            return None;
        }
        for &c in &popups {
            if seen.contains(&c) {
                continue;
            }
            match gui_text(p, o, child_by_name(p, o, c, "Main")) {
                Some(text) if !text.is_empty() => {
                    seen.push(c);
                    return Some((text, c));
                }
                _ => continue, // just created: filled in a moment later, retry next call
            }
        }
        None
    }

    /// How a catch popup from take_catch_popup shows the catch: its flat icon (catch/fih/icon, "rbxassetid://123"; None
    /// for fish shown as a 3D model, most of them) and the color the game recolored that model to (model_tint; None
    /// unless it marks it recolored with a "<mutation>template" part). None once the popup is gone. Set a moment after
    /// the text: read it with the XP. Also the model's visible mesh parts as triangles (CatchLook::model).
    pub fn catch_look(&mut self, popup: usize) -> Option<CatchLook> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let ann = self.announcements.get(p, o, self.gui);
        if !self.parent_is(popup, ann) {
            return None; // freed: its memory may hold anything now
        }
        let fih = child_by_name(p, o, popup, "fih");
        let icon = gui_image(p, o, child_by_name(p, o, fih, "icon")).filter(|s| asset_id(s).is_some());
        let model = child_by_name(p, o, child_by_name(p, o, fih, "vp"), "viewmodel");
        let kids = children(p, o, model);
        let recolored = kids
            .iter()
            .any(|&c| instance_name(p, o, c).is_some_and(|n| n.ends_with("template")));
        let m: &dyn Memory = p;
        let (mut model, textures, parts) = model_tris(m, o, data_model(p, o), kids, recolored);
        let vp = child_by_name(p, o, fih, "vp");
        let (scene, eye, rot) = popup_view(m, o, vp, &model);
        for t in &mut model {
            *t = to_camera(t, eye, &rot);
        }
        Some(CatchLook {
            icon,
            tint: model_tint(&parts), // empty unless recolored
            model,
            scene,
            textures,
        })
    }

    fn parent_is(&self, inst: usize, parent: usize) -> bool {
        inst != 0 && parent != 0 && parent_of(&self.sess.proc, &self.sess.offsets, inst) == parent
    }

    fn scale_x(&self, gui: usize, field: &str) -> f32 {
        let m: &dyn Memory = &self.sess.proc;
        m.get::<f32>(self.sess.offsets.at(gui, field)).unwrap_or(0.0) // UDim2 starts with X.Scale
    }

    fn resolve_player(&mut self) -> bool {
        // A rejoin or teleport frees the old player, whose stale memory still passes parent_is(gui, lp): seen live,
        // every character and PlayerGui read went empty until restart. So check down from the DataModel each time (4
        // reads).
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let dm = data_model(p, o);
        if !self.parent_is(self.players, dm) {
            self.players = if dm != 0 {
                child_by_class(p, o, dm, "Players")
            } else {
                0
            };
        }
        let lp = local_player(p, o, self.players);
        if lp != 0 && lp == self.lp && self.parent_is(self.gui, self.lp) {
            return true;
        }
        if lp != self.lp {
            // the caches below hang off the old player's game, freed just the same
            self.stats_owner = 0;
            self.stats = 0;
            self.prompt_gui = 0;
            self.overlay = 0;
            self.rod = 0;
            self.rod_state = 0;
            self.reel = 0;
            self.scanned_chr = 0;
            self.shake = 0;
            self.seen_popups = None;
            self.chr_kids.clear();
            self.gui_kids.clear();
        }
        self.lp = lp;
        self.gui = player_gui(p, o, self.lp);
        if self.gui != 0 {
            // hotbar alone left 5 offsets fitting (live, version-4ea8e85481e34f46: the real 0x345 and 0x1ed, 0x1f5,
            // 0x22d, 0x235), so the scan kept the wrong dumped 0x72d; safezone is 0 at those four
            let shown = [self.hotbar.get(p, o, self.gui), self.safezone.get(p, o, self.gui)];
            let hidden = [self.inventory.get(p, o, self.gui), self.fade.get(p, o, self.gui)];
            self.vis.calibrate(p, o, &shown, &hidden, &[]);
        }
        self.gui != 0
    }

    /// None while loading / not in game
    pub fn sample(&mut self) -> Option<FishState> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut s = FishState::default();
        // Equipped tools are parented to the character; Fisch rods carry a rodClient script.
        let chr = character_of(p, o, self.lp);
        s.character = chr;
        if !self.parent_is(self.rod, chr) || self.rod_state == 0 {
            // No rod last time and the same children: nothing to find. A rod without its state yet keeps looking.
            let kids = if chr != 0 { children(p, o, chr) } else { Vec::new() };
            let same = self.rod == 0 && chr == self.scanned_chr && kids == self.chr_kids;
            self.scanned_chr = chr;
            self.chr_kids = kids;
            if !same {
                self.rod = 0;
                self.rod_state = 0;
                for &c in &self.chr_kids {
                    if instance_class(p, o, c).as_deref() == Some("Tool") && child_by_name(p, o, c, "rodClient") != 0 {
                        self.rod = c;
                        let vals = child_by_name(p, o, c, "values");
                        self.rod_state = if vals != 0 {
                            child_by_name(p, o, vals, "state")
                        } else {
                            0
                        };
                        break;
                    }
                }
            }
        }
        if self.rod != 0 {
            s.rod = instance_name(p, o, self.rod).unwrap_or_else(|| "?".into());
            if self.rod_state != 0 {
                s.rod_state = int_value(p, o, self.rod_state);
            }
        }

        if !self.parent_is(self.reel, self.gui) {
            // The reel and shakeui come and go as PlayerGui children: read their names only when that list changed.
            let kids = children(p, o, self.gui);
            let fresh = kids != self.gui_kids;
            if fresh {
                self.gui_kids = kids;
                self.reel = 0;
                self.bar = 0;
                self.fish = 0;
                self.prog = 0;
                self.prog_speed = 0;
                self.shake = 0;
            }
            // nothing new: the shakeui found before is read below
            for &c in if fresh { &self.gui_kids[..] } else { &[] } {
                let n = instance_name(p, o, c);
                if n.as_deref() == Some("reel") {
                    let bar = child_by_name(p, o, c, "bar");
                    let child = |name: &str| if bar != 0 { child_by_name(p, o, bar, name) } else { 0 };
                    let prog = child("progress");
                    self.reel = c;
                    self.bar = child("playerbar");
                    self.fish = child("fish");
                    self.prog = if prog != 0 { child_by_name(p, o, prog, "bar") } else { 0 };
                    self.prog_speed = child("progressspeed");
                } else if n.as_deref() == Some("shakeui") {
                    self.shake = c;
                }
            }
            if self.shake != 0 {
                // every tick while luring: the button moves and is replaced after each click
                let zone = child_by_name(p, o, self.shake, "safezone");
                let mut btn = if zone != 0 {
                    child_by_name(p, o, zone, "button")
                } else {
                    0
                };
                if btn == 0 && zone != 0 {
                    btn = child_by_name(p, o, zone, "default");
                }
                if btn != 0 {
                    s.shake_button_x = Some(self.scale_x(btn, "GuiObject.Position"));
                }
            }
        }
        if self.reel != 0 && self.bar != 0 && self.fish != 0 && self.prog != 0 {
            s.reel = Some(Reel {
                bar: self.scale_x(self.bar, "GuiObject.Position"),
                bar_width: self.scale_x(self.bar, "GuiObject.Size"),
                fish: self.scale_x(self.fish, "GuiObject.Position"),
                progress: self.scale_x(self.prog, "GuiObject.Size"),
                // hidden on most reels, still holding "-100% Progress Speed": only what the game shows
                progress_speed: if self.vis.read(p, o, self.prog_speed).unwrap_or(false) {
                    gui_text(p, o, self.prog_speed).unwrap_or_default()
                } else {
                    String::new()
                },
            });
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paint(rgb: u32, glow: bool) -> Tri {
        Tri {
            rgb,
            glow,
            ..Tri::default()
        }
    }

    fn slots(v: &[(&str, &str)]) -> Slots {
        v.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }
    fn rods(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn bot_hotbar_key_matches_rod_inside_mutated_name() {
        let s = slots(&[("Equipment Bag", "2"), ("Abyssal Fallen Rod", "1"), ("GPS", "5")]);
        assert_eq!(hotbar_key_for(&rods(&["Fallen Rod"]), &s), Some(1));
        assert!(hotbar_key_for(&rods(&["Rod Of The Depths"]), &s).is_none()); // rod not on the hotbar
        assert!(hotbar_key_for(&rods(&[""]), &s).is_none()); // unnamed tool never matches everything
        assert_eq!(hotbar_key_for(&rods(&["GPS"]), &slots(&[("GPS", "0")])), Some(0));
        assert!(hotbar_key_for(&rods(&["GPS"]), &slots(&[("GPS", "10")])).is_none()); // not a single key
        // rods named inside other rods (the wiki has these)
        let nolife = slots(&[("Original No-Life Rod", "1"), ("No-Life Rod", "2")]);
        assert_eq!(hotbar_key_for(&rods(&["No-Life Rod"]), &nolife), Some(2));
        assert!(hotbar_key_for(&rods(&["Bloomspire"]), &slots(&[("Bloomspire: Twisted Toxins", "1")])).is_none());
        assert_eq!(enchanted_rod("No-Life Rod", &nolife), "No-Life Rod");
    }

    #[test]
    fn reader_part_tris_fits_turns_and_moves_the_mesh() {
        // off-center mesh 2 wide on each axis, part 4x2x2, turned 90 degrees about Y, at (10, 20, 30)
        let mesh = Mesh {
            positions: vec![[4.0, -1.0, -1.0], [6.0, 1.0, 1.0], [6.0, 0.0, 0.0]],
            normals: vec![[1.0, 0.0, 0.0], [0.0, 1.0, 1.0], [0.0; 3]],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            colors: Vec::new(),
            tris: vec![[0, 1, 2], [0, 1, 9]], // the second points past the vertices: dropped
        };
        let rot = [0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0];
        let tris = part_tris(
            &mesh,
            false,
            &rot,
            [10.0, 20.0, 30.0],
            [4.0, 2.0, 2.0],
            paint(0x123456, true),
        );
        assert_eq!(tris.len(), 1);
        assert_eq!((tris[0].rgb, tris[0].glow), (0x123456, true));
        // local (1,0,0) scaled to (2,0,0), turned to (0,0,-2); (1,1,1) -> (2,1,1) -> (1,1,-2)
        assert_eq!(tris[0].p[2], [10.0, 20.0, 28.0]);
        assert_eq!(tris[0].p[1], [11.0, 21.0, 28.0]);
        assert_eq!(tris[0].p[0], [9.0, 19.0, 32.0]);
        // normals: (1,0,0) / scale (2,1,1), turned to (0,0,-1); (0,1,1) -> (1,1,0)/sqrt2; a zero one stays flat
        let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-6);
        assert!(close(tris[0].n[0], [0.0, 0.0, -1.0]));
        let r = 0.5f32.sqrt();
        assert!(close(tris[0].n[1], [r, r, 0.0]));
        assert_eq!(tris[0].n[2], [0.0; 3]);
        // a squashed axis tilts a normal toward it: (1,1,0) on a part 1 wide (scale 0.5) in x, 2 in y
        let tilt = Mesh {
            normals: vec![[1.0, 1.0, 0.0]; 3],
            ..mesh.clone()
        };
        let t = part_tris(
            &tilt,
            false,
            &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [0.0; 3],
            [1.0, 2.0, 2.0],
            paint(0, false),
        );
        let k = 1.0 / 5f32.sqrt();
        assert!(close(t[0].n[0], [2.0 * k, k, 0.0]));
        // no normals at all: every corner flat
        let flat = Mesh {
            normals: Vec::new(),
            ..mesh.clone()
        };
        let tris = part_tris(&flat, false, &rot, [0.0; 3], [4.0, 2.0, 2.0], paint(0, false));
        assert_eq!(tris[0].n, [[0.0; 3]; 3]);
        // a union keeps its offset from the part's origin: (6,0,0) -> (12,0,0)
        let tris = part_tris(
            &mesh,
            true,
            &rot,
            [10.0, 20.0, 30.0],
            [4.0, 2.0, 2.0],
            paint(0x123456, true),
        );
        // turned: (12,0,0) -> (0,0,-12)
        assert_eq!(tris[0].p[2], [10.0, 20.0, 18.0]);
    }

    #[test]
    fn reader_part_shapes_fit_like_roblox() {
        let id = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let pos = [10.0, 20.0, 30.0];
        let shape = |s: u8, size: [f32; 3]| {
            let (mesh, size) = shape_mesh(s, size);
            part_tris(mesh, false, &id, pos, size, paint(0, false))
        };
        let corners = |t: &[Tri]| {
            t.iter()
                .flat_map(|t| t.p)
                .map(|p| [0, 1, 2].map(|k| p[k] - pos[k]))
                .collect::<Vec<_>>()
        };
        let len = |v: [f32; 3]| v.iter().map(|c| c * c).sum::<f32>().sqrt();
        // ball: a sphere of the smallest side, smooth normals pointing out
        let ball = shape(0, [3.0, 2.0, 5.0]);
        assert!(corners(&ball).iter().all(|&v| (len(v) - 1.0).abs() < 1e-5));
        assert!(ball.iter().all(|t| (0..3).all(|i| {
            let r = [0, 1, 2].map(|k| t.p[i][k] - pos[k]);
            (0..3).all(|k| (t.n[i][k] - r[k]).abs() < 1e-5)
        })));
        // cylinder: along X, size.x long, min(y, z) wide, round caps facing +-X
        let cyl = shape(2, [4.0, 3.0, 1.0]);
        let c = corners(&cyl);
        assert!(c.iter().all(|v| (v[0].abs() - 2.0).abs() < 1e-5));
        assert!(c.iter().all(|v| (len([0.0, v[1], v[2]]) - 0.5).abs() < 1e-5));
        assert!(cyl.iter().any(|t| t.n == [[1.0, 0.0, 0.0]; 3]) && cyl.iter().any(|t| t.n == [[-1.0, 0.0, 0.0]; 3]));
        // wedge: full bottom, tall back (+Z), the top only along the back edge
        let mut w = corners(&shape(3, [2.0, 4.0, 6.0]));
        w.sort_by(|a, b| a.partial_cmp(b).unwrap());
        w.dedup();
        let want = [
            [-1.0, -2.0, -3.0],
            [-1.0, -2.0, 3.0],
            [-1.0, 2.0, 3.0],
            [1.0, -2.0, -3.0],
            [1.0, -2.0, 3.0],
            [1.0, 2.0, 3.0],
        ];
        assert_eq!(w, want);
        // block and anything unknown (CornerWedge too): the cube
        assert_eq!(shape(1, [1.0; 3]).len(), 12);
        assert_eq!(shape(4, [1.0; 3]).len(), 12);
    }

    #[test]
    fn reader_enchanted_rod_from_hotbar_label() {
        let s = slots(&[("Equipment Bag", "2"), ("Momentum Carrot Rod", "1")]);
        assert_eq!(enchanted_rod("Carrot Rod", &s), "[Momentum] Carrot Rod");
        assert_eq!(
            enchanted_rod("Carrot Rod", &slots(&[("Carrot Rod", "1")])),
            "Carrot Rod"
        ); // no enchant
        assert_eq!(enchanted_rod("Fallen Rod", &s), "Fallen Rod"); // not on the hotbar
        assert!(enchanted_rod("", &s).is_empty());
    }

    #[test]
    fn bot_reel_state_excludes_cast_states() {
        assert!(reel_state(Some(7)) && reel_state(Some(8)) && reel_state(Some(9)) && reel_state(Some(2)));
        for s in [Some(3), Some(4), Some(5), Some(6), None] {
            assert!(!reel_state(s));
        }
    }

    #[test]
    fn bot_hotbar_key_prefers_rods_in_order() {
        let s = slots(&[("Abyssal Fallen Rod", "1"), ("Bestiary", "2"), ("Steady Rod", "3")]);
        assert_eq!(hotbar_key_for(&rods(&["Steady Rod", "Fallen Rod"]), &s), Some(3)); // preferred first, in slot 3
        assert_eq!(hotbar_key_for(&rods(&["Kings Rod", "Fallen Rod"]), &s), Some(1)); // preferred not there: any rod
    }

    // Live: needs Roblox in Fisch + network. Read-only: no hooks, no input. The Updater works in a temp dir, so it
    // never races a running handys-fisch.
    #[test]
    #[ignore]
    fn live_reader_costs() {
        use handy_sdk::app::updater_thread::{AppStatus, Updater, production_smoke_test};
        use handy_sdk::roblox::http::win_http_get;
        use handy_sdk::roblox::process::RobloxProcess;
        use handy_sdk::util::thread::StopThread;
        use std::time::{Duration, Instant};

        if RobloxProcess::find().is_none() {
            println!("  SKIP: Roblox not running");
            return;
        }
        let u = Arc::new(Updater::new(
            Arc::new(win_http_get),
            production_smoke_test,
            std::env::temp_dir().join("handys_fisch_live"),
            crate::app::offset_keys::required_keys(),
        ));
        let uu = u.clone();
        let _t = StopThread::spawn(move |st| uu.run(st));
        for _ in 0..300 {
            if u.status() == AppStatus::Ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Some(s) = u.session() else {
            println!("  SKIP: offsets not ready");
            return;
        };
        let mut r = GameReader::new(s);
        let st = r.sample();
        assert!(st.is_some());
        if let Some(st) = &st {
            println!("  rod='{}' phase={}", st.rod, crate::r#macro::bot::describe_phase(st));
        }
        // Average reads and microseconds of `f` over `n` calls, after one warm-up call.
        let mut cost = |what: &str, n: u32, f: &mut dyn FnMut(&mut GameReader)| {
            f(&mut r);
            let r0 = RobloxProcess::reads_on_this_thread();
            let t0 = Instant::now();
            for _ in 0..n {
                f(&mut r);
            }
            let us = t0.elapsed().as_secs_f64() * 1e6 / f64::from(n);
            let reads = (RobloxProcess::reads_on_this_thread() - r0) as f64 / f64::from(n);
            println!("  {what:<16} {reads:7.1} reads {us:9.1} us");
        };
        cost("sample", 2000, &mut |r| {
            let _ = r.sample();
        });
        cost("castPower", 2000, &mut |r| {
            let _ = r.cast_power();
        });
        cost("castFeedback", 2000, &mut |r| {
            let _ = r.cast_feedback();
        });
        cost("coins", 500, &mut |r| {
            let _ = r.coins();
        });
        cost("errorPrompt", 500, &mut |r| {
            let _ = r.error_prompt();
        });
        cost("levelProgress", 500, &mut |r| {
            let _ = r.level_progress();
        });
        cost("takeCatchPopup", 500, &mut |r| {
            let _ = r.take_catch_popup();
        });
        cost("playerInfo", 100, &mut |r| {
            let _ = r.player_info();
        });
        cost("world", 100, &mut |r| {
            let _ = r.world();
        });
        println!("  world={:?}", r.world());
        if let Some(pi) = r.player_info() {
            println!("  rod='{}' rod_stats={:?}", pi.rod, pi.rod_stats);
        }
    }
}

// Live tuning aid, read-only (memory reads and a screen copy, never input): at the next catch, the game popup's
// picture beside the app's, as BMPs in HANDY_PROBE_DIR (default: the temp dir), and what the model's parts are made of.
// Run: set HANDY_PROBE_DIR, then `cargo test --lib live_compare_popup -- --ignored --nocapture`, and catch a fish with
// the Roblox window uncovered.
#[cfg(test)]
mod live_compare {
    use super::*;

    /// Premultiplied BGRA over a mid grey (the dashboard's card), as a 32-bit BMP.
    fn bmp(path: &std::path::Path, w: u32, h: u32, bgra: &[u8]) {
        let mut b = Vec::new();
        b.extend(b"BM");
        for v in [54 + w * h * 4, 0, 54, 40] {
            b.extend(v.to_le_bytes());
        }
        b.extend((w as i32).to_le_bytes());
        b.extend((-(h as i32)).to_le_bytes()); // top-down
        b.extend(1u16.to_le_bytes());
        b.extend(32u16.to_le_bytes());
        for v in [0u32, w * h * 4, 2835, 2835, 0, 0] {
            b.extend(v.to_le_bytes());
        }
        for p in bgra.chunks(4) {
            let a = u32::from(p[3]);
            for c in &p[..3] {
                b.push((u32::from(*c) + 0x2b * (255 - a) / 255) as u8);
            }
            b.push(255);
        }
        std::fs::write(path, b).unwrap();
    }

    /// The screen's pixels in a rect (physical pixels), as BGRA.
    fn screen(x: i32, y: i32, w: i32, h: i32) -> Vec<u8> {
        use windows::Win32::Graphics::Gdi::*;
        let mut px = vec![0u8; (w * h * 4) as usize];
        unsafe {
            let dc = GetDC(None);
            let mem = CreateCompatibleDC(Some(dc));
            let bm = CreateCompatibleBitmap(dc, w, h);
            let old = SelectObject(mem, bm.into());
            let _ = BitBlt(mem, 0, 0, w, h, Some(dc), x, y, SRCCOPY);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            GetDIBits(
                mem,
                bm,
                0,
                h as u32,
                Some(px.as_mut_ptr().cast()),
                &mut bi,
                DIB_RGB_COLORS,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bm.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, dc);
        }
        for p in px.chunks_mut(4) {
            p[3] = 255;
        }
        px
    }

    #[test]
    #[ignore]
    fn live_compare_popup() {
        use handy_sdk::app::updater_thread::{AppStatus, Updater, production_smoke_test};
        use handy_sdk::input::focus::process_window;
        use handy_sdk::roblox::http::win_http_get;
        use handy_sdk::roblox::read::gui_absolute_rect;
        use handy_sdk::ui::raster::render;
        use handy_sdk::util::thread::StopThread;
        use std::time::{Duration, Instant};
        use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};

        let dir = std::env::var_os("HANDY_PROBE_DIR").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let u = Arc::new(Updater::new(
            Arc::new(win_http_get),
            production_smoke_test,
            std::env::temp_dir().join("handys_fisch_live"),
            crate::app::offset_keys::required_keys(),
        ));
        let uu = u.clone();
        let _t = StopThread::spawn(move |st| uu.run(st));
        for _ in 0..300 {
            if u.status() == AppStatus::Ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Some(s) = u.session() else {
            println!("  SKIP: Roblox or offsets not ready");
            return;
        };
        let mut r = GameReader::new(s.clone());
        let (p, o) = (&s.proc, &s.offsets);
        // Roblox lays its GUI out in physical pixels: so must the window origin and the screen copy be
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let _ = r.take_catch_popup(); // only a popup that comes after this
        let t0 = Instant::now();
        let (text, popup) = loop {
            if let Some(c) = r.take_catch_popup().filter(|c| c.0.contains("caught")) {
                break c;
            }
            assert!(t0.elapsed() < Duration::from_secs(900), "no catch in 15 minutes");
            std::thread::sleep(Duration::from_millis(20));
        };
        println!("  {text}");
        std::thread::sleep(Duration::from_millis(450)); // fully shown, not fading yet
        let vp = child_by_name(p, o, child_by_name(p, o, popup, "fih"), "vp");
        let rect = gui_absolute_rect(p, o, vp).expect("the popup's picture area");
        let mut origin = windows::Win32::Foundation::POINT::default();
        if let Some(h) = process_window(p.pid()) {
            let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(h, &mut origin) };
        }
        let (w, h) = (rect.size.x.round().max(1.0) as u32, rect.size.y.round().max(1.0) as u32);
        let game = screen(
            origin.x + rect.pos.x as i32,
            origin.y + rect.pos.y as i32,
            w as i32,
            h as i32,
        );
        bmp(&dir.join("game.bmp"), w, h, &game);
        // the meshes load into the game's cache a little after the popup shows
        let mut look = None;
        for _ in 0..25 {
            look = r.catch_look(popup).filter(|l| !l.model.is_empty());
            if look.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let Some(look) = look else {
            println!(
                "  no 3D model (a flat icon: {:?})",
                r.catch_look(popup).and_then(|l| l.icon)
            );
            return;
        };
        bmp(&dir.join("app.bmp"), w, h, &render(&look.model, &[], &look.scene, w, h));
        let model = crate::r#macro::rarest_model::FishModel {
            tris: look.model.clone(),
            scene: look.scene,
            textures: look.textures.clone(),
            hue_rate: 0.0,
        };
        crate::r#macro::rarest_model::save(&dir.join("model.bin"), "probe", &model); // for live_render_saved
        bmp(
            &dir.join("app_big.bmp"),
            w * 4,
            h * 4,
            &render(&look.model, &[], &look.scene, w * 4, h * 4),
        );
        println!(
            "  {} triangles, textures {:?}, {:?}",
            look.model.len(),
            look.textures,
            look.scene
        );
        // what the game paints: each kind of part once, with how many there are
        let m: &dyn Memory = p;
        let mut kinds = std::collections::BTreeMap::new();
        let mut stack = children(p, o, child_by_name(p, o, vp, "viewmodel"));
        while let Some(c) = stack.pop() {
            stack.extend(children(p, o, c));
            let class = instance_class(p, o, c).unwrap_or_default();
            let what = match class.as_str() {
                "Part" | "MeshPart" | "PartOperation" | "UnionOperation" | "WedgePart" => {
                    let prim = m.get::<usize>(o.at(c, "BasePart.Primitive")).unwrap_or(0);
                    format!(
                        "{class} rgb={:?} material={:?} transparency={:?}",
                        m.get::<[u8; 3]>(o.at(c, "BasePart.Color3")),
                        m.get::<u16>(o.at(prim, "Primitive.Material")),
                        m.get::<f32>(o.at(c, "BasePart.Transparency")),
                    )
                }
                "Decal" | "Texture" | "SurfaceAppearance" => class,
                _ => continue,
            };
            *kinds.entry(what).or_insert(0) += 1;
        }
        for (what, n) in kinds {
            println!("  {n:3} {what}");
        }
        println!("  pictures in {}", dir.display());
    }

    /// Offline tuning: the model live_compare_popup saved, drawn again (240 px) into HANDY_PROBE_DIR/app_again.bmp.
    #[test]
    #[ignore]
    fn live_render_saved() {
        let dir = std::env::var_os("HANDY_PROBE_DIR").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let Some(m) = crate::r#macro::rarest_model::load(&dir.join("model.bin"), "probe") else {
            println!("  SKIP: no model.bin from live_compare_popup");
            return;
        };
        let px = handy_sdk::ui::raster::render(&m.tris, &[], &m.scene, 240, 240);
        bmp(&dir.join("app_again.bmp"), 240, 240, &px);
    }
}
