use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};

use handy_sdk::util::thread::StopThread;

use crate::game::catch::asset_id;

// Images need the raw body (HttpResult::body is lossy text): (status, body), status 0 on any failure.
pub type GetBytes = Arc<dyn Fn(&str) -> (i32, Vec<u8>) + Send + Sync>;

/// The wiki API call that answers with a species page's picture, 192 px wide (a Value slot is 80 DIP: sharp to 240%).
pub fn picture_query_url(species: &str) -> String {
    let mut title = String::new();
    for ch in species.bytes() {
        if ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'.' || ch == b'_' {
            title.push(ch as char);
        } else {
            title += &format!("%{ch:02X}");
        }
    }
    format!(
        "https://fischipedia.org/w/api.php?action=query&format=json&prop=pageimages&piprop=thumbnail\
         &pithumbsize=192&redirects=1&titles={title}"
    )
}

/// Roblox's public thumbnail API for an image asset (a game icon): answers with its PNG's URL as "imageUrl".
pub fn asset_query_url(id: &str) -> String {
    format!("https://thumbnails.roblox.com/v1/assets?assetIds={id}&size=420x420&format=Png")
}

/// The picture's URL in either answer: the wiki's "source", the thumbnail API's "imageUrl". None when it has none
/// (the species has no page or picture; the thumbnail is still being made or was refused).
pub fn thumbnail_url(json: &str) -> Option<String> {
    let (at, key) = ["\"source\":\"", "\"imageUrl\":\""]
        .iter()
        .find_map(|k| json.find(k).map(|at| (at, k.len())))?;
    let url: String = json[at + key..]
        .chars()
        .take_while(|&c| c != '"')
        .filter(|&c| c != '\\')
        .collect(); // JSON escapes "/" as "\/"
    url.starts_with("https://").then_some(url)
}

/// Whether a download starts like a PNG, JPEG or GIF (what the dashboard can show), not an error page answered with
/// status 200.
pub fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG") || b.starts_with(b"\xFF\xD8\xFF") || b.starts_with(b"GIF8")
}

#[derive(Default)]
struct State {
    todo: VecDeque<String>,
    known: BTreeMap<String, PathBuf>, // species asked about -> its file (empty: none yet)
}

struct Shared {
    dir: PathBuf,
    get: GetBytes,
    m: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn file_for(&self, species: &str) -> PathBuf {
        let name: String = species
            .chars()
            .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
            .collect();
        self.dir.join(name + ".png") // the image loader reads the format from the bytes, so a JPEG under .png shows
    }

    fn run(&self, stop: &AtomicBool) {
        loop {
            let species = {
                let st = self.m.lock().unwrap();
                let mut st = self
                    .wake
                    .wait_while(st, |s| s.todo.is_empty() && !stop.load(Relaxed))
                    .unwrap();
                match st.todo.pop_front() {
                    Some(s) => s,
                    None => return, // stopped
                }
            };
            // shortcut: a failed download is not retried until the app restarts, retry on a timer if the wiki flakes often.
            // a game icon ("rbxassetid://123") from Roblox, else the species' picture from the wiki
            let query = asset_id(&species).map_or_else(|| picture_query_url(&species), asset_query_url);
            let (status, page) = (self.get)(&query);
            let url = if status == 200 {
                thumbnail_url(&String::from_utf8_lossy(&page))
            } else {
                None
            };
            let Some(url) = url else { continue };
            if stop.load(Relaxed) {
                continue;
            }
            let (status, image) = (self.get)(&url);
            // Kept forever once on disk, so nothing else gets there. Over 1 MB: cut off.
            if status != 200 || !looks_like_image(&image) || image.len() > 1000000 {
                continue;
            }
            let file = self.file_for(&species);
            let mut part = file.clone().into_os_string();
            part.push(".part");
            let part = PathBuf::from(part);
            let _ = std::fs::create_dir_all(&self.dir);
            if std::fs::write(&part, &image).is_err() {
                // a full disk, say: a half-written file must not become the picture
                let _ = std::fs::remove_file(&part);
                continue;
            }
            // whole or not at all: the dashboard never reads half a picture
            if std::fs::rename(&part, &file).is_err() {
                continue;
            }
            self.m.lock().unwrap().known.insert(species, file);
        }
    }
}

/// Fish pictures from fischipedia, downloaded once per species into `dir` and read from there after, on a thread of
/// their own so neither the dashboard nor the bot ever waits on the network.
pub struct FishPictures {
    shared: Arc<Shared>,
    worker: StopThread, // last: it starts once the rest exists
}

impl FishPictures {
    pub fn new(dir: impl Into<PathBuf>, get: GetBytes) -> Self {
        let shared = Arc::new(Shared {
            dir: dir.into(),
            get,
            m: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let s = shared.clone();
        Self {
            shared,
            worker: StopThread::spawn(move |stop| s.run(stop)),
        }
    }

    /// The picture file for a species (from the wiki) or a game icon ("rbxassetid://123", from Roblox); empty while it
    /// downloads, or when there is none. Asks the network once per name.
    pub fn get(&self, species: &str) -> PathBuf {
        if species.is_empty() {
            return PathBuf::new();
        }
        let mut st = self.shared.m.lock().unwrap();
        if let Some(f) = st.known.get(species) {
            return f.clone();
        }
        let file = self.shared.file_for(species);
        let known = if Path::exists(&file) { file } else { PathBuf::new() };
        if known.as_os_str().is_empty() {
            st.todo.push_back(species.to_owned());
            self.shared.wake.notify_one();
        }
        st.known.insert(species.to_owned(), known.clone());
        known
    }
}

impl Drop for FishPictures {
    fn drop(&mut self) {
        // The stop flag is read under the lock, so the worker is either waiting (and woken here) or sees it.
        self.worker.request_stop();
        drop(self.shared.m.lock().unwrap());
        self.shared.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;
    use std::time::Duration;

    #[test]
    fn pictures_query_url_encodes_the_species() {
        assert_eq!(
            picture_query_url("Red Snapper's"),
            "https://fischipedia.org/w/api.php?action=query&format=json&prop=pageimages&piprop=thumbnail\
             &pithumbsize=192&redirects=1&titles=Red%20Snapper%27s"
        );
        assert!(picture_query_url("\u{e9}").ends_with("titles=%C3%A9")); // UTF-8 bytes
    }

    #[test]
    fn pictures_thumbnail_url_from_the_wiki_answer() {
        // as fischipedia answered for Mackerel
        assert_eq!(
            thumbnail_url(concat!(
                r#"{"query":{"pages":{"632":{"title":"Mackerel","thumbnail":{"source":"#,
                r#""https:\/\/static.wikitide.net\/fischwiki\/thumb\/7\/7e\/Mackerel.png\/192px-Mackerel.png","#,
                r#""width":192,"height":56},"pageimage":"Mackerel.png"}}}}"#
            ))
            .as_deref(),
            Some("https://static.wikitide.net/fischwiki/thumb/7/7e/Mackerel.png/192px-Mackerel.png")
        );
        assert!(thumbnail_url(r#"{"query":{"pages":{"-1":{"title":"Nofish","missing":""}}}}"#).is_none());
        // Roblox's thumbnail API, for a game icon
        assert_eq!(
            thumbnail_url(
                r#"{"data":[{"targetId":1,"state":"Completed","imageUrl":"https://tr.rbxcdn.com/abc/420/420/Image/Png/noFilter"}]}"#
            ),
            Some("https://tr.rbxcdn.com/abc/420/420/Image/Png/noFilter".into())
        );
        assert!(thumbnail_url(r#"{"data":[{"targetId":1,"state":"Pending","imageUrl":null}]}"#).is_none());
        assert_eq!(
            asset_query_url("110074220763561"),
            "https://thumbnails.roblox.com/v1/assets?assetIds=110074220763561&size=420x420&format=Png"
        );
    }

    #[test]
    fn pictures_only_real_images_are_kept() {
        assert!(
            looks_like_image(b"\x89PNG\r\n") && looks_like_image(b"\xFF\xD8\xFF\xE0") && looks_like_image(b"GIF89a")
        );
        assert!(!looks_like_image(b"") && !looks_like_image(b"<!doctype html>") && !looks_like_image(b"RIFF....WEBP"));

        let dir = std::env::temp_dir().join("fisch-pictures-bad-test");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let pics = FishPictures::new(
                &dir,
                Arc::new(|url: &str| {
                    if url.contains("api.php") {
                        return (200, br#"{"source":"https:\/\/x\/Cod.png"}"#.to_vec());
                    }
                    (200, b"<html>challenge page</html>".to_vec())
                }),
            );
            pics.get("Cod");
            std::thread::sleep(Duration::from_millis(100));
            assert!(pics.get("Cod").as_os_str().is_empty());
        }
        assert!(!dir.join("Cod.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pictures_downloads_once_then_reads_the_file() {
        let dir = std::env::temp_dir().join("fisch-pictures-test");
        let _ = std::fs::remove_dir_all(&dir);
        let calls = Arc::new(AtomicI32::new(0));
        let c = calls.clone();
        let pics = FishPictures::new(
            &dir,
            Arc::new(move |url: &str| {
                c.fetch_add(1, Relaxed);
                if url.contains("api.php") {
                    let body: &[u8] = if url.ends_with("Cod") {
                        br#"{"thumbnail":{"source":"https:\/\/x\/Cod.png"}}"#
                    } else {
                        b"{}"
                    };
                    return (200, body.to_vec());
                }
                (200, b"\x89PNG byte".to_vec())
            }),
        );
        assert!(pics.get("Cod").as_os_str().is_empty()); // downloading
        let mut file = PathBuf::new();
        for _ in 0..200 {
            if !file.as_os_str().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            file = pics.get("Cod");
        }
        assert!(file == dir.join("Cod.png") && std::fs::metadata(&file).unwrap().len() == 9);
        assert!(pics.get("Nofish").as_os_str().is_empty()); // no picture on the wiki: asked once, stays empty
        std::thread::sleep(Duration::from_millis(50));
        assert!(pics.get("Nofish").as_os_str().is_empty() && calls.load(Relaxed) == 3);
        assert!(pics.get("").as_os_str().is_empty() && calls.load(Relaxed) == 3);
        let c = calls.clone();
        let again = FishPictures::new(
            &dir,
            Arc::new(move |_: &str| {
                c.fetch_add(1, Relaxed);
                (0, Vec::new())
            }),
        );
        assert!(again.get("Cod") == dir.join("Cod.png") && calls.load(Relaxed) == 3); // already on disk: no network
        drop((pics, again));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
