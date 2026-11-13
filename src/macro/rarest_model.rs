//! The rarest catch's 3D model on disk, so it survives a restart: a little-endian file tagged with the catch's popup
//! text, so a model left over from an older rarest catch is ignored.
use std::path::Path;

use handy_sdk::ui::raster::Tri;

const MAGIC: &[u8; 4] = b"HFRM";
const VERSION: u32 = 1;
const TRI_BYTES: usize = 18 * 4 + 4 + 1;

pub fn encode(text: &str, tris: &[Tri]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + text.len() + tris.len() * TRI_BYTES);
    b.extend(MAGIC);
    b.extend(VERSION.to_le_bytes());
    b.extend((text.len() as u32).to_le_bytes());
    b.extend(text.as_bytes());
    b.extend((tris.len() as u32).to_le_bytes());
    for t in tris {
        for v in t.p.iter().chain(&t.n).flatten() {
            b.extend(v.to_le_bytes());
        }
        b.extend(t.rgb.to_le_bytes());
        b.push(t.glow as u8);
    }
    b
}

/// The triangles, when the bytes are a whole file of this format for `text`.
pub fn decode(b: &[u8], text: &str) -> Option<Vec<Tri>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = b.split_at_checked(n)?;
        *b = tail;
        Some(head)
    }
    let u32_ = |b: &mut &[u8]| Some(u32::from_le_bytes(take(b, 4)?.try_into().ok()?));
    let mut rest = b;
    if take(&mut rest, 4)? != MAGIC || u32_(&mut rest)? != VERSION {
        return None;
    }
    let len = u32_(&mut rest)? as usize;
    if take(&mut rest, len)? != text.as_bytes() {
        return None;
    }
    let count = u32_(&mut rest)? as usize;
    if rest.len() != count.checked_mul(TRI_BYTES)? {
        return None; // truncated or trailing junk
    }
    let f = |c: &[u8], i: usize| f32::from_le_bytes(c[i * 4..i * 4 + 4].try_into().unwrap());
    Some(
        rest.as_chunks::<TRI_BYTES>()
            .0
            .iter()
            .map(|c| Tri {
                p: [0, 1, 2].map(|v| [0, 1, 2].map(|k| f(c, v * 3 + k))),
                n: [0, 1, 2].map(|v| [0, 1, 2].map(|k| f(c, 9 + v * 3 + k))),
                rgb: u32::from_le_bytes(c[72..76].try_into().unwrap()),
                glow: c[76] != 0,
            })
            .collect(),
    )
}

/// A failed write only costs the model after a restart.
pub fn save(path: &Path, text: &str, tris: &[Tri]) {
    let _ = std::fs::write(path, encode(text, tris));
}

pub fn load(path: &Path, text: &str) -> Option<Vec<Tri>> {
    decode(&std::fs::read(path).ok()?, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rarest_model_round_trips_for_its_catch_only() {
        let tris = vec![
            Tri {
                p: [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.5]],
                n: [[0.0, 1.0, 0.0], [0.0; 3], [-1.0, 0.0, 0.0]],
                rgb: 0x123456,
                glow: true,
            },
            Tri {
                p: [[0.0; 3]; 3],
                n: [[0.0; 3]; 3],
                rgb: 0xffffff,
                glow: false,
            },
        ];
        let b = encode("Cod · 1.2 kg", &tris);
        assert_eq!(decode(&b, "Cod · 1.2 kg"), Some(tris.clone()));
        assert_eq!(decode(&b, "Cod · 1.3 kg"), None); // another catch's model
        assert_eq!(decode(&b[..b.len() - 1], "Cod · 1.2 kg"), None); // truncated
        assert_eq!(decode(&b[..10], "Cod · 1.2 kg"), None);
        assert_eq!(decode(&[], ""), None);
        let dir = std::env::temp_dir().join(format!("handys-fisch-rarest-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rarest_model.bin");
        save(&path, "x", &tris);
        assert_eq!(load(&path, "x"), Some(tris));
        assert_eq!(load(&dir.join("missing.bin"), "x"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
