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

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// An empty marker older than this is re-tried (covers art uploaded later).
const MISS_RETRY: Duration = Duration::from_secs(7 * 24 * 3600);

/// Marker suffix: the cover had no art.
const MISS_SUFFIX: &str = ".miss";
/// Marker suffix: the hero had no art.
const HERO_MISS_SUFFIX: &str = ".heromiss";

pub const SGDB_BASE: &str = "https://www.steamgriddb.com/api/v2";

/// Hand-droppable image extensions, consulted before the fetched `.img`.
const MANUAL_EXT: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];

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
#[derive(Clone, Copy)]
enum Kind {
    /// Tile art (`.img`), 2:3 portrait.
    Cover,
    /// Band background (`.hero.img`), wide.
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
    let _ = fs::create_dir_all(art_dir());
    for want in wants {
        if ensure_one(&want, api_key.as_deref()) {
            on_ready(&want.key);
        }
    }
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
fn write(want: &Want, kind: Kind, dir: &Path, fetched: Result<Vec<u8>, String>) -> bool {
    let miss = dir.join(kind.miss(&want.key));
    match fetched {
        Ok(bytes) => {
            let img = dir.join(kind.cache(&want.key));
            match fs::write(&img, &bytes) {
                Ok(()) => {
                    let _ = fs::remove_file(&miss);
                    log::info!(
                        "{}: {} ready ({} bytes, {})",
                        kind.label(),
                        want.key,
                        bytes.len(),
                        want.exe
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

/// Bytes for one kind: the keyless Steam CDN first when the exe sits in a Steam library, then
/// SteamGridDB (`None` without an API key).
fn fetch(want: &Want, kind: Kind, sgdb: Option<&mut Sgdb<'_>>) -> Result<Vec<u8>, String> {
    if let Some(appid) = steam_appid(Path::new(&want.exe)) {
        match kind {
            Kind::Cover => {
                for file in ["library_600x900.jpg", "library_600x900_2x.jpg"] {
                    if let Ok(bytes) = http_get(&steam_url(appid, file), None) {
                        return Ok(bytes); // keyless and exact: the installed Steam appid
                    }
                }
            }
            // A 404 here is just "Steam has no hero for it": fall through to SteamGridDB.
            Kind::Hero => {
                if let Ok(bytes) = http_get(&steam_url(appid, "library_hero.jpg"), None) {
                    return Ok(bytes);
                }
            }
        }
    }
    let Some(sgdb) = sgdb else {
        return Err("not a Steam install and no SteamGridDB key is set".into());
    };
    let game = sgdb.game(want)?;
    match kind {
        Kind::Cover => {
            let url = sgdb_grid_url(sgdb.key, game)?;
            http_get(&url, None).map_err(|e| format!("grid download: {e}"))
        }
        Kind::Hero => {
            let url = sgdb_hero_url(sgdb.key, game)?;
            http_get(&url, None).map_err(|e| format!("hero download: {e}"))
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
        let id = sgdb_autocomplete(self.key, &search_term(&want.name))?;
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
fn search_term(name: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
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

fn sgdb_autocomplete(key: &str, term: &str) -> Result<u32, String> {
    #[derive(serde::Deserialize)]
    struct Hit {
        id: u32,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Hit>,
    }
    let url = format!("{SGDB_BASE}/search/autocomplete/{}", urlencode(term));
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("autocomplete: {e}"))?;
    resp.data
        .first()
        .map(|h| h.id)
        .ok_or_else(|| format!("no autocomplete hit for '{term}'"))
}

fn sgdb_grid_url(key: &str, game: u32) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Grid {
        url: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Grid>,
    }
    let url = format!(
        "{SGDB_BASE}/grids/game/{game}?dimensions=600x900&mimes=image/png,image/jpeg&types=static"
    );
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("grids: {e}"))?;
    resp.data
        .into_iter()
        .next()
        .map(|g| g.url)
        .ok_or_else(|| "no 600x900 grid".into())
}

fn sgdb_hero_url(key: &str, game: u32) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Hero {
        url: String,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Hero>,
    }
    let url = format!(
        "{SGDB_BASE}/heroes/game/{game}?dimensions=1920x620&mimes=image/png,image/jpeg&types=static"
    );
    let body = http_get(&url, Some(key))?;
    let resp: Resp = serde_json::from_slice(&body).map_err(|e| format!("heroes: {e}"))?;
    resp.data
        .into_iter()
        .next()
        .map(|h| h.url)
        .ok_or_else(|| "no 1920x620 hero".into())
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
    fn hero_cache_prefers_the_hand_dropped_file() {
        let dir = std::env::temp_dir().join(format!("armoury-art-hero-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_hero_miss_and_the_user_art_block_the_retry() {
        let dir = std::env::temp_dir().join(format!("armoury-art-heromiss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let key = "fedcba9876543210";
        assert!(wanted(&dir, key, Kind::Hero));
        fs::write(dir.join(format!("{key}{HERO_MISS_SUFFIX}")), b"").unwrap();
        assert!(!wanted(&dir, key, Kind::Hero)); // a fresh miss: not retried yet
        fs::write(dir.join(format!("{key}.hero.webp")), b"x").unwrap();
        assert!(!wanted(&dir, key, Kind::Hero)); // the user's own art: never compete
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_misses_removes_only_the_markers() {
        let dir = std::env::temp_dir().join(format!("armoury-art-clear-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        let _ = fs::remove_dir_all(&dir);
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
        let root = std::env::temp_dir().join(format!("armoury-steam-app-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
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

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn steam_appid_walks_an_appmanifest() {
        let root = std::env::temp_dir().join(format!("armoury-art-test-{}", std::process::id()));
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
        let _ = fs::remove_dir_all(&root);
    }
}
