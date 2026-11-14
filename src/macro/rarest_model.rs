//! A catch's 3D model as the game's popup drew it, and the rarest one on disk so it survives a restart: a
//! little-endian file tagged with the catch's popup text, so a model left over from an older rarest catch is ignored.
use std::path::Path;

use handy_sdk::ui::raster::{Scene, Tri};

/// Triangles in the popup camera's space, its camera and light, and the texture assets the triangles use (a Tri's
/// tex is 1 + an index here).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FishModel {
    pub tris: Vec<Tri>,
    pub scene: Scene,
    pub textures: Vec<String>,
    /// Degrees a second the game turns every part's hue (a rainbow catch); 0 for still colors.
    pub hue_rate: f32,
}

const MAGIC: &[u8; 4] = b"HFRM";
const VERSION: u32 = 4; // 3: no hue_rate
// corners, normals, uvs (24 f32), rgb, flags (glow, shimmer), tex, vertex colors (3 x 4), clear, shine
const TRI_BYTES: usize = 24 * 4 + 4 + 1 + 4 + 12 + 4 + 4;
// version 2: no vertex colors, clear or shine; flag 2 was "see-through" (then always a ForceField)
const V2_TRI_BYTES: usize = 24 * 4 + 4 + 1 + 4;
const V2_CLEAR: f32 = 0.6;
// version 1: world-space triangles (corners, normals, rgb, glow), no uv, texture or camera
const V1_TRI_BYTES: usize = 18 * 4 + 4 + 1;
const V1_LOOK_DOWN: f32 = 0.4; // radians: how the dashboard used to show them

pub fn encode(text: &str, model: &FishModel) -> Vec<u8> {
    let mut b = Vec::with_capacity(64 + text.len() + model.tris.len() * TRI_BYTES);
    let str_ = |b: &mut Vec<u8>, s: &str| {
        b.extend((s.len() as u32).to_le_bytes());
        b.extend(s.as_bytes());
    };
    b.extend(MAGIC);
    b.extend(VERSION.to_le_bytes());
    str_(&mut b, text);
    let s = &model.scene;
    for v in [s.fov].iter().chain(&s.ambient).chain(&s.color).chain(&s.dir) {
        b.extend(v.to_le_bytes());
    }
    b.extend((model.textures.len() as u32).to_le_bytes());
    for t in &model.textures {
        str_(&mut b, t);
    }
    b.extend(model.hue_rate.to_le_bytes());
    b.extend((model.tris.len() as u32).to_le_bytes());
    for t in &model.tris {
        for v in t.p.iter().chain(&t.n).flatten().chain(t.uv.iter().flatten()) {
            b.extend(v.to_le_bytes());
        }
        b.extend(t.rgb.to_le_bytes());
        b.push(u8::from(t.glow) | u8::from(t.shimmer) << 1);
        b.extend(t.tex.to_le_bytes());
        b.extend(t.vc.as_flattened());
        b.extend(t.clear.to_le_bytes());
        b.extend(t.shine.to_le_bytes());
    }
    b
}

/// The model, when the bytes are a whole file of this format for `text`.
pub fn decode(b: &[u8], text: &str) -> Option<FishModel> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = b.split_at_checked(n)?;
        *b = tail;
        Some(head)
    }
    let u32_ = |b: &mut &[u8]| Some(u32::from_le_bytes(take(b, 4)?.try_into().ok()?));
    let f32_ = |b: &mut &[u8]| Some(f32::from_le_bytes(take(b, 4)?.try_into().ok()?));
    let str_ = |b: &mut &[u8]| {
        let len = u32_(b)? as usize;
        String::from_utf8(take(b, len)?.to_vec()).ok()
    };
    let mut rest = b;
    if take(&mut rest, 4)? != MAGIC {
        return None;
    }
    let version = u32_(&mut rest)?;
    if !(1..=VERSION).contains(&version) || str_(&mut rest)? != text {
        return None;
    }
    if version == 1 {
        return decode_v1(rest);
    }
    let mut v = [0f32; 10];
    for x in &mut v {
        *x = f32_(&mut rest)?;
    }
    let scene = Scene {
        fov: v[0],
        ambient: [v[1], v[2], v[3]],
        color: [v[4], v[5], v[6]],
        dir: [v[7], v[8], v[9]],
        time: 0.0,
    };
    let n = u32_(&mut rest)? as usize;
    let textures = (0..n.min(1024)).map(|_| str_(&mut rest)).collect::<Option<Vec<_>>>()?;
    let hue_rate = if version >= 4 { f32_(&mut rest)? } else { 0.0 };
    let count = u32_(&mut rest)? as usize;
    let size = if version == 2 { V2_TRI_BYTES } else { TRI_BYTES };
    if textures.len() != n || rest.len() != count.checked_mul(size)? {
        return None; // truncated or trailing junk
    }
    let f = |c: &[u8], i: usize| f32::from_le_bytes(c[i * 4..i * 4 + 4].try_into().unwrap());
    let tris = rest
        .chunks_exact(size)
        .map(|c| {
            let flag = c[100] & 2 != 0;
            let mut t = Tri {
                p: [0, 1, 2].map(|v| [0, 1, 2].map(|k| f(c, v * 3 + k))),
                n: [0, 1, 2].map(|v| [0, 1, 2].map(|k| f(c, 9 + v * 3 + k))),
                uv: [0, 1, 2].map(|v| [0, 1].map(|k| f(c, 18 + v * 2 + k))),
                rgb: u32::from_le_bytes(c[96..100].try_into().unwrap()),
                glow: c[100] & 1 != 0,
                shimmer: flag,
                tex: u32::from_le_bytes(c[101..105].try_into().unwrap()),
                ..Tri::default()
            };
            if version == 2 {
                t.clear = if flag { V2_CLEAR } else { 0.0 };
            } else {
                t.vc = [0, 1, 2].map(|v| c[105 + v * 4..109 + v * 4].try_into().unwrap());
                t.clear = f32::from_le_bytes(c[117..121].try_into().unwrap());
                t.shine = f32::from_le_bytes(c[121..125].try_into().unwrap());
            }
            t
        })
        .collect();
    Some(FishModel {
        tris,
        scene,
        textures,
        hue_rate,
    })
}

/// A version 1 file's triangles, seen from the slight elevation the dashboard used, under the default light.
fn decode_v1(mut rest: &[u8]) -> Option<FishModel> {
    let (head, tail) = rest.split_at_checked(4)?;
    rest = tail;
    let count = u32::from_le_bytes(head.try_into().ok()?) as usize;
    if rest.len() != count.checked_mul(V1_TRI_BYTES)? {
        return None;
    }
    let (sn, cs) = V1_LOOK_DOWN.sin_cos();
    let view = |v: [f32; 3]| [v[0], v[1] * cs - v[2] * sn, v[1] * sn + v[2] * cs];
    let f = |c: &[u8], i: usize| f32::from_le_bytes(c[i * 4..i * 4 + 4].try_into().unwrap());
    let tris = rest
        .as_chunks::<V1_TRI_BYTES>()
        .0
        .iter()
        .map(|c| Tri {
            p: [0, 1, 2].map(|v| view([0, 1, 2].map(|k| f(c, v * 3 + k)))),
            n: [0, 1, 2].map(|v| view([0, 1, 2].map(|k| f(c, 9 + v * 3 + k)))),
            rgb: u32::from_le_bytes(c[72..76].try_into().unwrap()),
            glow: c[76] != 0,
            ..Tri::default()
        })
        .collect();
    Some(FishModel {
        tris,
        ..FishModel::default()
    })
}

/// A failed write only costs the model after a restart.
pub fn save(path: &Path, text: &str, model: &FishModel) {
    let _ = std::fs::write(path, encode(text, model));
}

pub fn load(path: &Path, text: &str) -> Option<FishModel> {
    decode(&std::fs::read(path).ok()?, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarest_model_round_trips_for_its_catch_only() {
        let model = FishModel {
            tris: vec![
                Tri {
                    p: [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.5]],
                    n: [[0.0, 1.0, 0.0], [0.0; 3], [-1.0, 0.0, 0.0]],
                    rgb: 0x123456,
                    glow: true,
                    uv: [[0.25, 0.5], [1.0, 0.0], [0.0, 1.0]],
                    tex: 1,
                    vc: [[1, 2, 3, 255], [0; 4], [255; 4]],
                    clear: 0.25,
                    shine: 0.7,
                    shimmer: false,
                },
                Tri {
                    rgb: 0xffffff,
                    clear: 0.6,
                    shimmer: true,
                    ..Tri::default()
                },
            ],
            scene: Scene {
                fov: 0.35,
                ..Scene::default()
            },
            textures: vec!["rbxassetid://123".into()],
            hue_rate: -20.5,
        };
        let b = encode("Cod · 1.2 kg", &model);
        assert_eq!(decode(&b, "Cod · 1.2 kg"), Some(model.clone()));
        assert_eq!(decode(&b, "Cod · 1.3 kg"), None); // another catch's model
        assert_eq!(decode(&b[..b.len() - 1], "Cod · 1.2 kg"), None); // truncated
        assert_eq!(decode(&b[..10], "Cod · 1.2 kg"), None);
        assert_eq!(decode(&[], ""), None);
        let dir = std::env::temp_dir().join(format!("handys-fisch-rarest-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rarest_model.bin");
        save(&path, "x", &model);
        assert_eq!(load(&path, "x"), Some(model));
        assert_eq!(load(&dir.join("missing.bin"), "x"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rarest_model_reads_version_2() {
        let mut b = Vec::new();
        b.extend(MAGIC);
        b.extend(2u32.to_le_bytes());
        b.extend(3u32.to_le_bytes());
        b.extend(b"Cod");
        for v in [0.35f32, 0.8, 0.8, 0.8, 0.5, 0.5, 0.5, -1.0, -1.0, -1.0] {
            b.extend(v.to_le_bytes());
        }
        b.extend(0u32.to_le_bytes()); // no textures
        b.extend(1u32.to_le_bytes());
        b.extend([0u8; 96]);
        b.extend(0x123456u32.to_le_bytes());
        b.push(2); // see-through: a ForceField
        b.extend(0u32.to_le_bytes());
        let m = decode(&b, "Cod").unwrap();
        let t = m.tris[0];
        assert_eq!(
            (t.rgb, t.glow, t.shimmer, t.clear, t.vc),
            (0x123456, false, true, V2_CLEAR, [[0; 4]; 3])
        );
        assert_eq!(m.scene.fov, 0.35);
        assert_eq!(decode(&b[..b.len() - 1], "Cod"), None);
    }

    #[test]
    fn rarest_model_reads_version_1() {
        let mut b = Vec::new();
        b.extend(MAGIC);
        b.extend(1u32.to_le_bytes());
        b.extend(3u32.to_le_bytes());
        b.extend(b"Cod");
        b.extend(1u32.to_le_bytes());
        for v in [[0.0f32, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]
            .iter()
            .chain(&[[0.0; 3]; 3])
            .flatten()
        {
            b.extend(v.to_le_bytes());
        }
        b.extend(0xabcdefu32.to_le_bytes());
        b.push(1);
        let m = decode(&b, "Cod").unwrap();
        assert_eq!(
            (m.tris.len(), m.tris[0].rgb, m.tris[0].glow, m.tris[0].tex),
            (1, 0xabcdef, true, 0)
        );
        assert_eq!(m.tris[0].p[1], [1.0, 0.0, 0.0]); // x stays; y and z turn with the look down
        assert!(m.tris[0].p[0][2] > 0.3 && m.scene == Scene::default());
        assert_eq!(decode(&b[..b.len() - 1], "Cod"), None);
    }
}
