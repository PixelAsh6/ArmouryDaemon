//! Cover and hero art for the console band: SteamGridDB plus the keyless Steam CDN.
//!
//! The daemon fetches on a worker thread and writes one file per app under
//! `%APPDATA%\ArmouryDaemon\art`; the console only ever reads that folder.
//!
//! Cache layout (key = first 8 bytes of sha256 over the lowercased exe path, hex):
//!   art/<key>.img         raw downloaded cover bytes (png/jpeg; the reader sniffs the format)
//!   art/<key>.miss        empty marker: nothing matched; retried after MISS_RETRY
//!   art/<key>.hero.img    raw downloaded hero bytes (the band background)
//!   art/<key>.heromiss    empty marker: no hero matched; retried after MISS_RETRY
//!   art/<key>.<ext>       hand-dropped cover: wins over everything, blocks fetching
//!   art/<key>.hero.<ext>  hand-dropped hero: wins over everything, blocks fetching
//!   art/.version          the layout the cached art was fetched with (see ART_VERSION)
//!
//! The picker writes the user's own pick in the hand-dropped form, so a chosen photo wins
//! over everything the fetcher downloads and blocks it (see `wanted`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use armoury_core::protocol::{ArtListing, ArtPhoto, ArtSlot};

/// An empty marker older than this is re-tried (covers art uploaded later).
const MISS_RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

/// Marker suffix: the cover had no art.
const MISS_SUFFIX: &str = ".miss";
/// Marker suffix: the hero had no art.
const HERO_MISS_SUFFIX: &str = ".heromiss";

/// The layout version of the fetched art. Bump it when the art we want changes: the art
/// the old layout fetched is then dropped and fetched again, so a fix reaches the band
/// without the user clearing the cache by hand. The hand-dropped files are the user's own
/// and always stay.
const ART_VERSION: u32 = 2;

/// The file in the art folder that records the layout the cached art was fetched with.
const VERSION_FILE: &str = ".version";

pub const SGDB_BASE: &str = "https://www.steamgriddb.com/api/v2";

/// Hand-droppable image extensions, consulted before the fetched `.img`.
const MANUAL_EXT: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];

/// The Steam CDN files for the tile, best first. Steam publishes no square icon for an app
/// (`icon.jpg`, `icon_0.jpg` and `icon.png` all 404), so the tile is the 2:3 library
/// capsule — and the `_2x` file is the real 600x900 while the plain one is only 300x450.
const STEAM_COVER_FILES: [&str; 2] = ["library_600x900_2x.jpg", "library_600x900.jpg"];

/// The Steam CDN files for the band background, widest and nicest first: `library_hero` is
/// the 1920x620 hero, the capsules are the next best wide art Steam has for an app.
const STEAM_HERO_FILES: [&str; 5] = [
    "library_hero.jpg",    // 1920x620
    "capsule_616x353.jpg", // 616x353
    "header.jpg",          // 460x215
    "capsule_467x181.jpg", // 467x181
    "capsule_231x87.jpg",  // 231x87
];

/// The SteamGridDB grid sizes for the tile, best first: the 2:3 grid is the key art the
/// band crops cleanly (it loses ~9% top and bottom), a square grid is the fallback for a
/// game with no portrait art (a square loses ~18% of its width and clips the artwork).
const SGDB_COVER_DIMS: [&str; 2] = ["600x900", "1024x1024"];

/// The SteamGridDB hero sizes for the band background, best first.
const SGDB_HERO_DIMS: [&str; 2] = ["1920x620", "3840x1240"];

/// The SteamGridDB grid sizes the picker offers for the icon slot, best first: the 2:3
/// portrait grids, which are the shape this slot shows. The auto-picker also falls back
/// to a square grid (it has to take whatever there is); the picker does not, because a
/// square is not what the slot shows.
const SGDB_ICON_DIMS: [&str; 3] = ["600x900", "660x930", "342x482"];

/// How many photos one API page holds. The API caps the limit at 50 without saying so.
pub const PAGE_LIMIT: usize = 50;

/// Folder names that are never a game title: searching for them only wastes a request.
const GENERIC_DIRS: [&str; 10] = [
    "common",
    "steamapps",
    "steam",
    "games",
    "game",
    "bin",
    "apps",
    "pc",
    "program files",
    "program files (x86)",
];

/// One app the fetcher should try to cover.
pub struct Want {
    pub key: String,
    pub name: String,
    pub exe: String,
}

/// Cache key: first 8 bytes (16 hex chars) of sha256 over the lowercased exe path.
pub fn key(exe: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(exe.to_ascii_lowercase().as_bytes());
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn art_dir() -> PathBuf {
    armoury_core::config::config_dir().join("art")
}

/// The two kinds of art the console band reads; each has its own cache files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Tile art (`.img`): the 2:3 key art the band center-crops into its 196:240 tile.
    Cover,
    /// Band background (`.hero.img`): wide, the hero art of the focused app.
    Hero,
}

impl Kind {
    /// Cache file holding the fetched bytes.
    fn cache(self, key: &str) -> String {
        match self {
            Kind::Cover => format!("{key}.img"),
            Kind::Hero => format!("{key}.hero.img"),
        }
    }

    /// Empty "nothing matched" marker, re-tried after MISS_RETRY.
    fn miss(self, key: &str) -> String {
        match self {
            Kind::Cover => format!("{key}{MISS_SUFFIX}"),
            Kind::Hero => format!("{key}{HERO_MISS_SUFFIX}"),
        }
    }

    /// A hand-dropped file of this kind, with `ext`.
    fn manual(self, key: &str, ext: &str) -> String {
        match self {
            Kind::Cover => format!("{key}.{ext}"),
            Kind::Hero => format!("{key}.hero.{ext}"),
        }
    }

    /// Log prefix.
    fn label(self) -> &'static str {
        match self {
            Kind::Cover => "cover art",
            Kind::Hero => "hero art",
        }
    }
}

/// The dimensions the API is asked for, per slot, best first: the wanted size first, then
/// the fallbacks the slot accepts.
pub fn slot_dims(slot: ArtSlot) -> &'static [&'static str] {
    match slot {
        ArtSlot::Icon => &SGDB_ICON_DIMS,
        ArtSlot::Background => &SGDB_HERO_DIMS,
    }
}

/// The cache kind behind a slot.
fn slot_kind(slot: ArtSlot) -> Kind {
    match slot {
        ArtSlot::Icon => Kind::Cover,
        ArtSlot::Background => Kind::Hero,
    }
}

/// The best cached image of `kind` in `dir`: a hand-dropped file first, then the download.
fn cached_in(dir: &Path, key: &str, kind: Kind) -> Option<PathBuf> {
    for ext in MANUAL_EXT {
        let path = dir.join(kind.manual(key, ext));
        if path.is_file() {
            return Some(path);
        }
    }
    let img = dir.join(kind.cache(key));
    img.is_file().then_some(img)
}

/// The best cached cover for `key`.
pub fn cached(key: &str) -> Option<PathBuf> {
    cached_in(&art_dir(), key, Kind::Cover)
}

/// The best cached hero (the band background) for `key`.
pub fn cached_hero(key: &str) -> Option<PathBuf> {
    cached_in(&art_dir(), key, Kind::Hero)
}

// ---------------------------------------------------------------------------
// The picker: listing the photos, taking one, letting the automatic picker have it back
// ---------------------------------------------------------------------------

/// What the API has already told us about one game and one slot.
#[derive(Clone)]
struct Listing {
    /// The dimension the pages came from.
    dim: String,
    /// Every photo fetched so far, in API order.
    items: Vec<ArtPhoto>,
    /// The total the API reports for the whole listing.
    total: u32,
}

/// The listings already fetched, per game and slot. The API's terms forbid an
/// unreasonable load, so the same page is never asked for twice.
static LISTINGS: OnceLock<Mutex<HashMap<(u32, ArtSlot), Listing>>> = OnceLock::new();

/// The game ids already resolved, per app.
static GAME_IDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();

/// The SteamGridDB game id of an app, resolved once and remembered.
///
/// The lookup is a search, and the picker's pages all ask for the same game: resolving it
/// once keeps the number of requests down.
pub fn game_id(api_key: Option<&str>, want: &Want) -> Result<u32, String> {
    let Some(key) = api_key else {
        return Err("no SteamGridDB API key is set".into());
    };
    let cache = GAME_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(id) = cache.lock().unwrap().get(&want.key) {
        return Ok(*id);
    }
    let id = sgdb_resolve(key, want)?;
    cache.lock().unwrap().insert(want.key.clone(), id);
    Ok(id)
}

/// The photos of one page for an app and a slot.
///
/// The listing is cached per game and slot: the API's terms forbid an unreasonable load
/// and there is no published rate limit, so the same page is never asked for twice.
pub fn list_photos(
    api_key: Option<&str>,
    want: &Want,
    slot: ArtSlot,
    page: u32,
) -> Result<ArtListing, String> {
    let Some(key) = api_key else {
        return Err("no SteamGridDB API key is set".into());
    };
    let game = game_id(Some(key), want)?;
    let (items, total) = listing_page(key, game, slot, page)?;
    Ok(ArtListing {
        ok: true,
        slot,
        page,
        total,
        items,
        error: String::new(),
    })
}

/// The photos of one page for a game and a slot, fetched when the cache does not have them.
fn listing_page(
    api_key: &str,
    game: u32,
    slot: ArtSlot,
    page: u32,
) -> Result<(Vec<ArtPhoto>, u32), String> {
    let cache = LISTINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let cached = cache.lock().unwrap().get(&(game, slot)).cloned();
    let start = page as usize * PAGE_LIMIT;
    if let Some(listing) = &cached
        && start < listing.items.len()
    {
        // The page is already here.
        let end = (start + PAGE_LIMIT).min(listing.items.len());
        return Ok((listing.items[start..end].to_vec(), listing.total));
    }
    let dims = slot_dims(slot);
    // Which dimension to ask: the one in use, or the first one that has photos.
    let (dim, items, total) = match &cached {
        Some(listing) => {
            let dim = listing.dim.clone();
            let (items, total) = sgdb_page(api_key, game, slot, &dim, page)?;
            (dim, items, total)
        }
        None => {
            let mut picked: Option<(String, Vec<ArtPhoto>, u32)> = None;
            let mut last_err: Option<String> = None;
            for dim in dims {
                match sgdb_page(api_key, game, slot, dim, page) {
                    Ok((items, total)) => {
                        if total > 0 {
                            picked = Some((dim.to_string(), items, total));
                            break;
                        }
                        if picked.is_none() {
                            // Remember the first empty answer: a game with no photos at
                            // all is an empty list, not an error.
                            picked = Some((dim.to_string(), items, total));
                        }
                    }
                    Err(e) => {
                        // This size did not answer: the next one may.
                        last_err = Some(e);
                    }
                }
            }
            match picked {
                Some(picked) => picked,
                None => {
                    return Err(last_err.unwrap_or_else(|| "no photos for this game".into()));
                }
            }
        }
    };
    // Remember the page: the next page of the same listing is one request away.
    let mut map = cache.lock().unwrap();
    let entry = map.entry((game, slot)).or_insert_with(|| Listing {
        dim: dim.clone(),
        items: Vec::new(),
        total: 0,
    });
    entry.dim = dim;
    merge_page(entry, page, items.clone(), total);
    Ok((items, total))
}

/// Remember one page in a listing: the page replaces everything after it.
///
/// The pages before it have to be there, or the listing would have a hole. False = the
/// page was not remembered (the page before it is missing).
fn merge_page(listing: &mut Listing, page: u32, items: Vec<ArtPhoto>, total: u32) -> bool {
    let start = page as usize * PAGE_LIMIT;
    if start > listing.items.len() {
        return false;
    }
    listing.total = total;
    listing.items.truncate(start);
    listing.items.extend(items);
    true
}

/// One page of photos from the API.
fn sgdb_page(
    api_key: &str,
    game: u32,
    slot: ArtSlot,
    dim: &str,
    page: u32,
) -> Result<(Vec<ArtPhoto>, u32), String> {
    #[derive(serde::Deserialize)]
    struct Item {
        #[serde(default)]
        id: u32,
        #[serde(default)]
        url: String,
        #[serde(default)]
        thumb: String,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
        #[serde(default)]
        style: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        #[serde(default = "yes")]
        success: bool,
        #[serde(default)]
        total: u32,
        #[serde(default)]
        data: Vec<Item>,
    }
    let url = format!(
        "{}&limit={PAGE_LIMIT}&page={page}",
        sgdb_photos_url(game, slot, dim)
    );
    let body = http_get(&url, Some(api_key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("{dim}: {e}"))?;
    if !resp.success {
        return Err(format!("the API refused the {dim} list"));
    }
    let items = resp
        .data
        .into_iter()
        .map(|i| ArtPhoto {
            id: i.id,
            url: i.url,
            thumb: i.thumb,
            width: i.width,
            height: i.height,
            style: i.style,
        })
        .collect();
    Ok((items, resp.total))
}

/// A missing `success` field is not a refusal.
fn yes() -> bool {
    true
}

/// The API url for the photos of one game, one slot and one dimension.
///
/// `types=static` and `mimes=image/png,image/jpeg` have to travel together: the API
/// answers with zero photos when `mimes` is asked for on its own.
fn sgdb_photos_url(game: u32, slot: ArtSlot, dim: &str) -> String {
    let path = match slot {
        ArtSlot::Icon => "grids",
        ArtSlot::Background => "heroes",
    };
    format!(
        "{SGDB_BASE}/{path}/game/{game}?dimensions={dim}&mimes=image/png,image/jpeg&types=static"
    )
}

/// Download one photo and store it as the user's own art for that slot.
///
/// The hand-dropped file is what the band reads first, so the pick wins over everything
/// the fetcher downloads — and it blocks the fetcher (see `wanted`).
pub fn set_photo(key: &str, slot: ArtSlot, url: &str) -> Result<(), String> {
    let bytes = http_get(url, None).map_err(|e| format!("cannot download the photo: {e}"))?;
    let dir = art_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create the art folder: {e}"))?;
    let kind = slot_kind(slot);
    let path = write_manual(&dir, key, kind, &bytes)?;
    // The fetched copy is not what shows any more, and a miss marker would keep the
    // fetcher from trying this slot again.
    let _ = fs::remove_file(dir.join(kind.cache(key)));
    let _ = fs::remove_file(dir.join(kind.miss(key)));
    log::info!(
        "{}: the user's own art is set ({} bytes)",
        kind.label(),
        fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    );
    Ok(())
}

/// Let the automatic picker choose this slot again: the user's own art goes, and so does
/// the "nothing matched" marker, so the next art pass really fetches.
pub fn use_automatic(key: &str, slot: ArtSlot) -> Result<(), String> {
    let dir = art_dir();
    use_automatic_in(&dir, key, slot)?;
    log::info!("{}: the automatic picker may choose again", slot_kind(slot).label());
    Ok(())
}

/// The same, in a given folder.
fn use_automatic_in(dir: &Path, key: &str, slot: ArtSlot) -> Result<(), String> {
    let kind = slot_kind(slot);
    clear_manual(dir, key, kind);
    let miss = dir.join(kind.miss(key));
    if miss.exists() {
        fs::remove_file(&miss).map_err(|e| format!("cannot clear the miss marker: {e}"))?;
    }
    Ok(())
}

/// Store the bytes as the user's own art: the file name says what they are.
fn write_manual(dir: &Path, key: &str, kind: Kind, bytes: &[u8]) -> Result<PathBuf, String> {
    let ext = image_ext(bytes).ok_or("the photo is neither a png nor a jpeg")?;
    // Only one file per slot may be the user's own: the old choice goes first.
    clear_manual(dir, key, kind);
    let path = dir.join(kind.manual(key, ext));
    fs::write(&path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Delete every hand-dropped file of a slot.
fn clear_manual(dir: &Path, key: &str, kind: Kind) {
    for ext in MANUAL_EXT {
        let _ = fs::remove_file(dir.join(kind.manual(key, ext)));
    }
}

/// The extension for the bytes we got: the image crate reads png and jpeg, nothing else.
fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => Some("png"),
        Ok(image::ImageFormat::Jpeg) => Some("jpg"),
        _ => None,
    }
}

/// Delete the "nothing matched" markers, so those apps are eligible for a fetch again.
/// Returns how many marker files were removed.
pub fn clear_misses() -> usize {
    let cleared = clear_misses_in(&art_dir());
    if cleared > 0 {
        log::info!("cover art: {cleared} miss marker(s) cleared");
    }
    cleared
}

/// The markers directly under `dir` — subdirectories are left alone. A missing directory is
/// simply nothing to clear.
fn clear_misses_in(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut cleared = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        if (name.ends_with(MISS_SUFFIX) || name.ends_with(HERO_MISS_SUFFIX))
            && fs::remove_file(&path).is_ok()
        {
            cleared += 1;
        }
    }
    cleared
}

/// Fetch everything still missing (blocking — call from a worker thread). `on_ready` fires
/// with each key that got new art — cover or hero, once per app — so the caller can refresh.
pub fn ensure_all(wants: Vec<Want>, api_key: Option<String>, on_ready: impl Fn(&str)) {
    let dir = art_dir();
    let _ = fs::create_dir_all(&dir);
    refresh_version(&dir);
    for want in wants {
        if ensure_one(&want, api_key.as_deref()) {
            on_ready(&want.key);
        }
    }
}

/// Make the cached art match the current layout: on a version bump the fetched art is
/// dropped so it is fetched again. Only the fetched files go — the hand-dropped art is the
/// user's own and always stays.
fn refresh_version(dir: &Path) {
    let path = dir.join(VERSION_FILE);
    let current = fs::read_to_string(&path).unwrap_or_default();
    if current.trim() == ART_VERSION.to_string() {
        return;
    }
    let dropped = drop_stale(dir);
    let _ = fs::write(&path, ART_VERSION.to_string());
    log::info!("cover art: layout v{ART_VERSION}, {dropped} stale file(s) dropped");
}

/// Drop the art the old layout fetched. The fetched files are the `.img` ones and the miss
/// markers; everything else in the folder is the user's own and stays.
fn drop_stale(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut dropped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        let fetched = name.ends_with(".img")
            || name.ends_with(MISS_SUFFIX)
            || name.ends_with(HERO_MISS_SUFFIX);
        if fetched && fs::remove_file(&path).is_ok() {
            dropped += 1;
        }
    }
    dropped
}

/// One app: fetch whichever of cover and hero is still missing. True = new art on disk.
fn ensure_one(want: &Want, api_key: Option<&str>) -> bool {
    let dir = art_dir();
    let cover = wanted(&dir, &want.key, Kind::Cover);
    let hero = wanted(&dir, &want.key, Kind::Hero);
    if !cover && !hero {
        return false;
    }
    // One autocomplete per app, shared by its cover and hero fetches.
    let mut sgdb = api_key.map(|key| Sgdb { key, id: None });
    let mut landed = false;
    if cover {
        landed |= write(want, Kind::Cover, &dir, fetch(want, Kind::Cover, sgdb.as_mut()));
    }
    if hero {
        landed |= write(want, Kind::Hero, &dir, fetch(want, Kind::Hero, sgdb.as_mut()));
    }
    landed
}

/// Does `key` still need this kind fetched? Skipped when the user dropped their own art, when
/// the download is cached, or while a miss is still fresh.
fn wanted(dir: &Path, key: &str, kind: Kind) -> bool {
    for ext in MANUAL_EXT {
        if dir.join(kind.manual(key, ext)).is_file() {
            return false; // the user dropped their own art: never compete with it
        }
    }
    if dir.join(kind.cache(key)).is_file() {
        return false;
    }
    if let Ok(md) = fs::metadata(dir.join(kind.miss(key))) {
        let fresh = md
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age < MISS_RETRY)
            .unwrap_or(false);
        if fresh {
            return false;
        }
    }
    true
}

/// Store one fetch result: the bytes to the cache, or the miss marker. True = new art on disk.
fn write(want: &Want, kind: Kind, dir: &Path, fetched: Result<Fetched, String>) -> bool {
    let miss = dir.join(kind.miss(&want.key));
    match fetched {
        Ok(art) => {
            let img = dir.join(kind.cache(&want.key));
            match fs::write(&img, &art.bytes) {
                Ok(()) => {
                    let _ = fs::remove_file(&miss);
                    log::info!(
                        "{}: {} ready ({} bytes, {}, {})",
                        kind.label(),
                        want.key,
                        art.bytes.len(),
                        want.exe,
                        art.source
                    );
                    true
                }
                Err(e) => {
                    log::warn!("{}: cannot write {}: {e}", kind.label(), img.display());
                    false
                }
            }
        }
        Err(why) => {
            log::info!("{}: no art for {} ({why})", kind.label(), want.exe);
            let _ = fs::write(&miss, b"");
            false
        }
    }
}

/// Bytes for one kind, and where they came from.
struct Fetched {
    bytes: Vec<u8>,
    /// The asset that answered, for the log: which file, and which game.
    source: String,
}

/// The first candidate that `try_one` accepts, with the candidate that won: the order is
/// the preference.
fn first_ok<'a>(
    candidates: &[&'a str],
    mut try_one: impl FnMut(&'a str) -> Option<Vec<u8>>,
) -> Option<(&'a str, Vec<u8>)> {
    for candidate in candidates {
        if let Some(bytes) = try_one(candidate) {
            return Some((candidate, bytes));
        }
    }
    None
}

/// Bytes for one kind: the keyless Steam CDN first when the exe sits in a Steam library, then
/// SteamGridDB (`None` without an API key).
fn fetch(want: &Want, kind: Kind, sgdb: Option<&mut Sgdb<'_>>) -> Result<Fetched, String> {
    if let Some(appid) = steam_appid(Path::new(&want.exe)) {
        let files = match kind {
            Kind::Cover => &STEAM_COVER_FILES[..],
            Kind::Hero => &STEAM_HERO_FILES[..],
        };
        // A 404 on one file is just "Steam has no such art": the next one may answer.
        if let Some((file, bytes)) =
            first_ok(files, |f| http_get(&steam_url(appid, f), None).ok())
        {
            return Ok(Fetched {
                bytes,
                source: format!("Steam {file}"),
            });
        }
    }
    let Some(sgdb) = sgdb else {
        return Err("not a Steam install and no SteamGridDB key is set".into());
    };
    let key = sgdb.key;
    let game = sgdb.game(want)?;
    match kind {
        Kind::Cover => {
            let (dim, bytes) = first_ok(&SGDB_COVER_DIMS, |dim| {
                let url = sgdb_grid_url(key, game, dim).ok()?;
                http_get(&url, None).ok()
            })
            .ok_or("no grid")?;
            Ok(Fetched {
                bytes,
                source: format!("SteamGridDB {dim} grid of game {game}"),
            })
        }
        Kind::Hero => {
            let (dim, bytes) = first_ok(&SGDB_HERO_DIMS, |dim| {
                let url = sgdb_hero_url(key, game, dim).ok()?;
                http_get(&url, None).ok()
            })
            .ok_or("no hero")?;
            Ok(Fetched {
                bytes,
                source: format!("SteamGridDB {dim} hero of game {game}"),
            })
        }
    }
}

/// Lazy SteamGridDB resolution for one app: the autocomplete (search term -> game id) runs at
/// most once and serves both the cover and the hero.
struct Sgdb<'a> {
    key: &'a str,
    id: Option<u32>,
}

impl Sgdb<'_> {
    /// The app's SteamGridDB game id.
    fn game(&mut self, want: &Want) -> Result<u32, String> {
        if let Some(id) = self.id {
            return Ok(id);
        }
        let id = sgdb_resolve(self.key, want)?;
        self.id = Some(id);
        Ok(id)
    }
}

/// The keyless Steam CDN file of an installed app.
fn steam_url(appid: u32, file: &str) -> String {
    format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{file}")
}

/// The Steam appid of an installed game, via the `appmanifest_*.acf` beside its exe.
pub fn steam_appid(exe: &Path) -> Option<u32> {
    steam_app(exe).map(|app| app.appid)
}

/// A Steam install an exe belongs to.
pub struct SteamApp {
    /// The Steam appid (the number in `appmanifest_<appid>.acf`).
    pub appid: u32,
    /// The install root: `<steamapps>\common\<installdir>`.
    pub install_dir: PathBuf,
    /// The Steam directory: the parent of `steamapps`.
    pub steam_dir: PathBuf,
}

impl SteamApp {
    /// The Steam client of this install.
    pub fn steam_exe(&self) -> PathBuf {
        self.steam_dir.join("steam.exe")
    }
}

/// The Steam app an exe belongs to: the folder right after `common` in the exe's path,
/// matched against the `installdir` of the library's `appmanifest_*.acf` files. `None` when
/// the exe is not in a Steam library, or when its app is not installed.
pub fn steam_app(exe: &Path) -> Option<SteamApp> {
    let chain: Vec<&Path> = exe.ancestors().collect();
    let common = chain
        .iter()
        .position(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("common")))?;
    let install_dir = chain.get(common.checked_sub(1)?)?.to_path_buf();
    let installdir = install_dir.file_name()?.to_string_lossy().to_string();
    let steamapps = chain.get(common)?.parent()?;
    for entry in fs::read_dir(steamapps).ok()?.flatten() {
        let file = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let (mut appid, mut dir) = (None, String::new());
        for line in text.lines() {
            if let Some(v) = quoted_value(line, "appid") {
                appid = v.parse().ok();
            }
            if let Some(v) = quoted_value(line, "installdir") {
                dir = v.to_string();
            }
        }
        if let Some(appid) = appid {
            if dir.eq_ignore_ascii_case(&installdir) {
                return Some(SteamApp {
                    appid,
                    install_dir,
                    steam_dir: steamapps.parent()?.to_path_buf(),
                });
            }
        }
    }
    None
}

/// The second quoted token of a Valve keyvalues line (`\t"appid"\t\t"12345"`).
fn quoted_value<'a>(line: &'a str, wanted: &str) -> Option<&'a str> {
    let mut parts = line.split('"');
    let lead = parts.next()?;
    let key = parts.next()?;
    parts.next()?; // the gap between the two quoted tokens
    let value = parts.next()?;
    (lead.trim().is_empty() && key.eq_ignore_ascii_case(wanted)).then_some(value)
}

/// A friendlier SteamGridDB query than the raw exe stem: "AlanWake2" -> "Alan Wake 2".
/// A tracked app's name is its exe file name, so the ".exe" goes too.
fn search_term(name: &str) -> String {
    let name = name.trim();
    let stem = if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    };
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in stem.chars() {
        if let Some(p) = prev {
            let camel = c.is_ascii_uppercase() && !p.is_ascii_uppercase();
            let digit = c.is_ascii_digit() && !p.is_ascii_digit();
            if (camel || digit) && !out.ends_with(' ') {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out.trim().to_string()
}

/// The SteamGridDB search terms for an app, best first: the app's own name, then the folders
/// the exe sits in. A pinned "DS2" really is "DEATH STRANDING 2", and the folder knows it.
fn search_terms(want: &Want) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |term: String| {
        let term = term.trim().to_string();
        if term.is_empty() || out.iter().any(|t| t.eq_ignore_ascii_case(&term)) {
            return;
        }
        out.push(term);
    };
    add(search_term(&want.name));
    for dir in Path::new(&want.exe).ancestors().skip(1).take(2) {
        let Some(name) = dir.file_name() else { continue };
        let name = name.to_string_lossy();
        if GENERIC_DIRS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        add(search_term(&clean_dir(&name)));
    }
    out
}

/// A folder name as a title: the dots and underscores a release folder carries are noise.
fn clean_dir(name: &str) -> String {
    name.chars()
        .map(|c| if c == '.' || c == '_' { ' ' } else { c })
        .collect()
}

/// Does the game's name really start with the term? "DS 2" must not match "Eiken Kako
/// Mondai Shuuroku: Eiken DS 2 Deluxe": the term has to be the start of a name, not a word
/// buried in the middle of one.
fn name_starts_with(name: &str, term: &str) -> bool {
    let name = squash(name);
    let term = squash(term);
    !term.is_empty() && name.starts_with(term.as_str())
}

/// A name reduced to its letters and digits, so punctuation and spacing do not matter.
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// A SteamGridDB search hit: the game id and the name it is listed under.
#[derive(serde::Deserialize)]
struct Hit {
    id: u32,
    #[serde(default)]
    name: String,
}

/// The SteamGridDB game id for an app.
///
/// The search term is the first thing that can go wrong: a pinned "DS2" is really "DEATH
/// STRANDING 2", and "DS 2" lands on "Eiken Kako Mondai Shuuroku: Eiken DS 2 Deluxe" — a
/// completely different Japanese game, whose single grid is the box art the band showed. So
/// a term only counts when a hit's name really starts with it, and the folders the exe sits
/// in get a say too.
fn sgdb_resolve(key: &str, want: &Want) -> Result<u32, String> {
    let mut fallback: Option<(u32, String)> = None;
    let mut last_err: Option<String> = None;
    for term in search_terms(want) {
        let hits = match sgdb_search(key, &term) {
            Ok(hits) => hits,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        for hit in &hits {
            if name_starts_with(&hit.name, &term) {
                log::info!("SteamGridDB: {} is game {} ({})", want.exe, hit.id, hit.name);
                return Ok(hit.id);
            }
        }
        if fallback.is_none() {
            fallback = hits.first().map(|h| (h.id, h.name.clone()));
        }
    }
    if let Some((id, name)) = fallback {
        log::info!("SteamGridDB: {} is game {} ({})", want.exe, id, name);
        return Ok(id);
    }
    Err(last_err.unwrap_or_else(|| {
        format!("no SteamGridDB hit for '{}'", search_term(&want.name))
    }))
}

/// The SteamGridDB games a search term matches.
fn sgdb_search(key: &str, term: &str) -> Result<Vec<Hit>, String> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Hit>,
    }
    let url = format!("{SGDB_BASE}/search/autocomplete/{}", urlencode(term));
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("autocomplete: {e}"))?;
    Ok(resp.data)
}

/// The SteamGridDB grid URL for a game at one size.
fn sgdb_grid_url(key: &str, game: u32, dim: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Grid {
        url: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Grid>,
    }
    let url = sgdb_photos_url(game, ArtSlot::Icon, dim);
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("grids: {e}"))?;
    resp.data
        .into_iter()
        .next()
        .map(|g| g.url)
        .ok_or_else(|| format!("no {dim} grid"))
}

/// The SteamGridDB hero URL for a game at one size.
fn sgdb_hero_url(key: &str, game: u32, dim: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Hero {
        url: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Hero>,
    }
    let url = sgdb_photos_url(game, ArtSlot::Background, dim);
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("heroes: {e}"))?;
    resp.data
        .into_iter()
        .next()
        .map(|h| h.url)
        .ok_or_else(|| format!("no {dim} hero"))
}

/// GET with the `catalog.rs` HTTP shape: explicit timeout, body as bytes.
fn http_get(url: &str, bearer: Option<&str>) -> Result<Vec<u8>, String> {
    let mut req = ureq::get(url);
    if let Some(key) = bearer {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let mut resp = req
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .call()
        .map_err(|e| format!("GET {url} ({e})"))?;
    resp.body_mut()
        .read_to_vec()
        .map_err(|e| format!("read {url} ({e})"))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    #[test]
    fn key_is_stable_and_case_folded() {
        assert_eq!(key("C:\\X\\Foo.exe"), key("c:\\x\\foo.exe"));
        assert_ne!(key("a.exe"), key("b.exe"));
        assert_eq!(key("a.exe").len(), 16);
    }

    #[test]
    fn search_term_splits_camel_and_digits() {
        assert_eq!(search_term("AlanWake2"), "Alan Wake 2");
        assert_eq!(search_term("RainbowSix"), "Rainbow Six");
        assert_eq!(search_term("Notepad"), "Notepad");
        assert_eq!(search_term("GTAV"), "GTAV"); // all caps: no fake splits
        assert_eq!(search_term("HalfLife2"), "Half Life 2");
    }

    #[test]
    fn search_term_drops_the_exe_suffix() {
        // a tracked app is named after its exe file
        assert_eq!(search_term("RainbowSix.exe"), "Rainbow Six");
        assert_eq!(search_term("RainbowSix.EXE"), "Rainbow Six");
        assert_eq!(search_term("exe"), "exe"); // too short to be a suffix
    }

    #[test]
    fn the_search_terms_are_the_name_and_the_folders_the_exe_sits_in() {
        // a pinned "DS2" is really "DEATH STRANDING 2": the folder knows it, the name does not
        let want = Want {
            key: "0123456789abcdef".into(),
            name: "DS2".into(),
            exe: r"C:\Games\DEATH STRANDING 2\DS2.exe".into(),
        };
        assert_eq!(search_terms(&want), vec!["DS 2", "DEATH STRANDING 2"]);

        // "Games" and "bin" are never a game title: they are not worth a search
        let nested = Want {
            key: "0123456789abcdef".into(),
            name: "game".into(),
            exe: r"C:\Games\Some Game\bin\game.exe".into(),
        };
        assert_eq!(search_terms(&nested), vec!["game", "Some Game"]);

        // the same folder twice is one search
        let kodi = Want {
            key: "0123456789abcdef".into(),
            name: "kodi".into(),
            exe: r"C:\Apps\Kodi\kodi.exe".into(),
        };
        assert_eq!(search_terms(&kodi), vec!["kodi"]);
    }

    #[test]
    fn a_term_only_matches_a_name_that_really_starts_with_it() {
        // the hit the band used to land on: a different Japanese game
        let eiken = "Eiken Kako Mondai Shuuroku: Eiken DS 2 Deluxe";
        assert!(!name_starts_with(eiken, "DS 2"));
        assert!(!name_starts_with(eiken, "DS2"));
        // the right game
        assert!(name_starts_with(
            "Death Stranding 2: On The Beach",
            "DEATH STRANDING 2"
        ));
        assert!(name_starts_with("Rainbow Six Siege", "Rainbow Six"));
        // punctuation and case do not matter
        assert!(name_starts_with("Kodi", "kodi"));
        assert!(!name_starts_with("Kodi", ""));
    }

    #[test]
    fn the_steam_assets_are_tried_in_the_right_order() {
        // the tile is the 2:3 key art, and the 2x file is the real 600x900 while the plain
        // one is only 300x450
        assert_eq!(
            STEAM_COVER_FILES,
            ["library_600x900_2x.jpg", "library_600x900.jpg"]
        );
        // the background is the widest art first, so it is never black while Steam has any
        assert_eq!(
            STEAM_HERO_FILES,
            [
                "library_hero.jpg",
                "capsule_616x353.jpg",
                "header.jpg",
                "capsule_467x181.jpg",
                "capsule_231x87.jpg",
            ]
        );
    }

    #[test]
    fn the_first_candidate_that_answers_wins() {
        // the order is the preference: the first file that downloads is the one taken
        let mut asked: Vec<&str> = Vec::new();
        let got = first_ok(&STEAM_HERO_FILES, |file| {
            asked.push(file);
            match file {
                "library_hero.jpg" => None, // Steam has no hero for this app
                "capsule_616x353.jpg" => Some(b"wide".to_vec()),
                _ => Some(b"narrow".to_vec()),
            }
        });
        assert_eq!(got.map(|(f, b)| (f, b)), Some(("capsule_616x353.jpg", b"wide".to_vec())));
        assert_eq!(asked, ["library_hero.jpg", "capsule_616x353.jpg"]);

        // nothing answers: no art, and the band falls back to the flat background
        assert!(first_ok(&STEAM_HERO_FILES, |_| None).is_none());
    }

    #[test]
    fn a_version_bump_drops_the_fetched_art_and_keeps_the_users_own() {
        let dir = Scratch::dir("art-version");
        let key = "aa11223344556677";
        let fetched = dir.join(format!("{key}.img"));
        let fetched_hero = dir.join(format!("{key}.hero.img"));
        let miss = dir.join(format!("{key}{MISS_SUFFIX}"));
        let hero_miss = dir.join(format!("{key}{HERO_MISS_SUFFIX}"));
        let mine = dir.join(format!("{key}.png"));
        let mine_hero = dir.join(format!("{key}.hero.png"));
        for f in [&fetched, &fetched_hero, &miss, &hero_miss, &mine, &mine_hero] {
            fs::write(f, b"x").unwrap();
        }

        assert_eq!(drop_stale(&dir), 4); // the fetched art and its miss markers
        assert!(!fetched.exists() && !fetched_hero.exists());
        assert!(!miss.exists() && !hero_miss.exists());
        assert!(mine.exists()); // the hand-dropped art is the user's own: it stays
        assert!(mine_hero.exists());
    }

    #[test]
    fn hero_cache_prefers_the_hand_dropped_file() {
        let dir = Scratch::dir("art-hero");
        let key = "0123456789abcdef";
        assert_eq!(cached_in(&dir, key, Kind::Hero), None);
        fs::write(dir.join(format!("{key}.hero.img")), b"x").unwrap();
        let fetched = dir.join(format!("{key}.hero.img"));
        assert_eq!(cached_in(&dir, key, Kind::Hero), Some(fetched));
        fs::write(dir.join(format!("{key}.hero.png")), b"x").unwrap();
        assert_eq!(
            cached_in(&dir, key, Kind::Hero),
            Some(dir.join(format!("{key}.hero.png")))
        );
        assert_eq!(cached_in(&dir, key, Kind::Cover), None); // the cover slot is its own
    }

    #[test]
    fn a_fresh_hero_miss_and_the_user_art_block_the_retry() {
        let dir = Scratch::dir("art-heromiss");
        let key = "fedcba9876543210";
        assert!(wanted(&dir, key, Kind::Hero));
        fs::write(dir.join(format!("{key}{HERO_MISS_SUFFIX}")), b"").unwrap();
        assert!(!wanted(&dir, key, Kind::Hero)); // a fresh miss: not retried yet
        fs::write(dir.join(format!("{key}.hero.webp")), b"x").unwrap();
        assert!(!wanted(&dir, key, Kind::Hero)); // the user's own art: never compete
    }

    #[test]
    fn clear_misses_removes_only_the_markers() {
        let dir = Scratch::dir("art-clear");
        let key = "abcdef0123456789";
        let cover_miss = dir.join(format!("{key}{MISS_SUFFIX}"));
        let hero_miss = dir.join(format!("{key}{HERO_MISS_SUFFIX}"));
        let img = dir.join(format!("{key}.img"));
        let other = dir.join("notes.txt");
        fs::write(&cover_miss, b"").unwrap();
        fs::write(&hero_miss, b"").unwrap();
        fs::write(&img, b"x").unwrap();
        fs::write(&other, b"x").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let nested = sub.join(format!("{key}{MISS_SUFFIX}"));
        fs::write(&nested, b"").unwrap();

        assert_eq!(clear_misses_in(&dir), 2);
        assert!(!cover_miss.exists());
        assert!(!hero_miss.exists());
        assert!(img.exists()); // the fetched art stays
        assert!(other.exists()); // an unrelated file stays
        assert!(nested.exists()); // subdirectories are not touched
        assert_eq!(clear_misses_in(&dir), 0); // nothing left to clear
    }

    #[test]
    fn each_slot_asks_for_its_own_sizes() {
        // the icon slot is the 2:3 portrait grid, with the smaller portraits as fallbacks
        assert_eq!(
            slot_dims(ArtSlot::Icon),
            ["600x900", "660x930", "342x482"]
        );
        // the background slot is the wide hero
        assert_eq!(slot_dims(ArtSlot::Background), ["1920x620", "3840x1240"]);
        // the slots keep their own cache files
        assert_eq!(slot_kind(ArtSlot::Icon), Kind::Cover);
        assert_eq!(slot_kind(ArtSlot::Background), Kind::Hero);
    }

    #[test]
    fn the_photo_list_asks_for_the_png_and_the_jpeg_together() {
        // `mimes` on its own answers with zero photos, so it travels with `types`.
        let url = sgdb_photos_url(1234, ArtSlot::Icon, "600x900");
        assert!(url.contains("types=static"));
        assert!(url.contains("mimes=image/png,image/jpeg"));
        assert!(url.contains("dimensions=600x900"));
        assert!(url.contains("/grids/game/1234"));
        // the background slot asks the heroes endpoint
        assert!(sgdb_photos_url(1, ArtSlot::Background, "1920x620").contains("/heroes/game/1"));
    }

    #[test]
    fn a_photo_page_lands_in_the_listing_cache() {
        let photo = |id: u32| ArtPhoto {
            id,
            url: format!("https://cdn.example/{id}.png"),
            thumb: format!("https://cdn.example/{id}t.png"),
            width: 600,
            height: 900,
            style: "Alternate".into(),
        };
        let mut listing = Listing {
            dim: "600x900".into(),
            items: Vec::new(),
            total: 0,
        };
        // the first page fills the listing
        assert!(merge_page(
            &mut listing,
            0,
            (0..PAGE_LIMIT as u32).map(photo).collect(),
            120,
        ));
        assert_eq!(listing.items.len(), PAGE_LIMIT);
        assert_eq!(listing.total, 120);
        // the second page is added after the first
        assert!(merge_page(
            &mut listing,
            1,
            (PAGE_LIMIT as u32..PAGE_LIMIT as u32 + 50).map(photo).collect(),
            120,
        ));
        assert_eq!(listing.items.len(), PAGE_LIMIT * 2);
        // a page that is fetched again replaces what came after it
        assert!(merge_page(&mut listing, 1, vec![photo(99)], 120));
        assert_eq!(listing.items.len(), PAGE_LIMIT + 1);
        assert_eq!(listing.items[PAGE_LIMIT].id, 99);
        // a page whose predecessor is missing is not remembered: the listing would have
        // a hole in it
        let mut hole = Listing {
            dim: "600x900".into(),
            items: Vec::new(),
            total: 0,
        };
        assert!(!merge_page(&mut hole, 2, vec![photo(1)], 120));
        assert!(hole.items.is_empty());
    }

    #[test]
    fn the_users_own_photo_goes_in_the_hand_dropped_file() {
        let dir = Scratch::dir("art-pick");
        let key = "aabbccddeeff0011";

        let png = image::DynamicImage::new_rgb8(2, 3);
        let mut bytes = Vec::new();
        png.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let path = write_manual(&dir, key, Kind::Cover, &bytes).unwrap();
        assert_eq!(path, dir.join(format!("{key}.png")));
        assert!(path.is_file());

        // the hero slot keeps its own file
        let path = write_manual(&dir, key, Kind::Hero, &bytes).unwrap();
        assert_eq!(path, dir.join(format!("{key}.hero.png")));
        assert!(path.is_file());

        // a jpeg is stored as a jpg, whatever the picture really is
        let mut jpg = Vec::new();
        png.write_to(&mut std::io::Cursor::new(&mut jpg), image::ImageFormat::Jpeg)
            .unwrap();
        let path = write_manual(&dir, key, Kind::Cover, &jpg).unwrap();
        assert_eq!(path, dir.join(format!("{key}.jpg")));
        // the old choice is gone: only one file per slot is the user's own
        assert!(!dir.join(format!("{key}.png")).exists());

        // bytes that are not a picture are refused
        assert!(write_manual(&dir, key, Kind::Cover, b"not a picture").is_err());
    }

    #[test]
    fn automatic_again_takes_the_users_own_art_away() {
        let dir = Scratch::dir("art-auto");
        let key = "1122334455667788";
        let mine = dir.join(format!("{key}.png"));
        let mine_hero = dir.join(format!("{key}.hero.png"));
        let miss = dir.join(format!("{key}{MISS_SUFFIX}"));
        let hero_miss = dir.join(format!("{key}{HERO_MISS_SUFFIX}"));
        let fetched = dir.join(format!("{key}.img"));
        for f in [&mine, &mine_hero, &miss, &hero_miss, &fetched] {
            fs::write(f, b"x").unwrap();
        }

        // the icon slot goes back to automatic
        use_automatic_in(&dir, key, ArtSlot::Icon).unwrap();
        assert!(!mine.exists());
        assert!(!miss.exists());
        // the other slot is untouched
        assert!(mine_hero.exists());
        assert!(hero_miss.exists());
        assert!(fetched.exists());

        // and so does the background slot
        use_automatic_in(&dir, key, ArtSlot::Background).unwrap();
        assert!(!mine_hero.exists());
        assert!(!hero_miss.exists());

        // a second time is not an error
        use_automatic_in(&dir, key, ArtSlot::Icon).unwrap();
    }

    #[test]
    fn quoted_value_reads_acf_lines() {
        assert_eq!(quoted_value("\t\"appid\"\t\t\"12345\"", "appid"), Some("12345"));
        assert_eq!(
            quoted_value("\t\"installdir\"\t\"Rainbow Six Siege\"", "installdir"),
            Some("Rainbow Six Siege")
        );
        assert_eq!(quoted_value("\t\"appid\"\t\t\"12345\"", "name"), None);
        assert_eq!(quoted_value("not a keyvalues line", "appid"), None);
    }

    #[test]
    fn a_steam_game_resolves_its_appid_and_its_steam_client() {
        let root = Scratch::dir("steam-app");
        let game = root.join("Steam/steamapps/common/Some Game/sub");
        fs::create_dir_all(&game).unwrap();
        fs::write(
            root.join("Steam/steamapps/appmanifest_12345.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"12345\"\n\t\"installdir\"\t\t\"Some Game\"\n}\n",
        )
        .unwrap();

        // the folder right after `common` is the installdir the manifest names
        let app = steam_app(&game.join("game.exe")).expect("the app resolves");
        assert_eq!(app.appid, 12345);
        assert_eq!(app.install_dir, root.join("Steam/steamapps/common/Some Game"));
        assert_eq!(app.steam_dir, root.join("Steam"));
        assert_eq!(app.steam_exe(), root.join("Steam/steam.exe"));
        assert_eq!(steam_appid(&game.join("game.exe")), Some(12345));

        // a path that is not under a `steamapps\common\...` folder is not a Steam game
        assert!(steam_app(&root.join("elsewhere/game.exe")).is_none());
        assert_eq!(steam_appid(&root.join("elsewhere/game.exe")), None);

        // a Steam folder whose installdir no manifest names is not an installed app
        let other = root.join("Steam/steamapps/common/Other Game");
        fs::create_dir_all(&other).unwrap();
        assert!(steam_app(&other.join("game.exe")).is_none());
    }

    #[test]
    fn steam_appid_walks_an_appmanifest() {
        let root = Scratch::dir("art-test");
        let game_dir = root.join("library/steamapps/common/Rainbow Six Siege");
        fs::create_dir_all(&game_dir).unwrap();
        fs::write(
            root.join("library/steamapps/appmanifest_359550.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"359550\"\n\t\"installdir\"\t\t\"Rainbow Six Siege\"\n}\n",
        )
        .unwrap();
        let exe = game_dir.join("RainbowSix.exe");
        assert_eq!(steam_appid(&exe), Some(359550));
        assert_eq!(steam_appid(&root.join("elsewhere/game.exe")), None);
    }
}
