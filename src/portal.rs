//! Client de l'API du portail de mods Factorio (mods.factorio.com).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::version::{Dependency, Version, factorio_compatible, same_major_minor};

const API: &str = "https://mods.factorio.com";
const ASSETS: &str = "https://assets-mod.factorio.com";
/// Durée de vie du cache des détails d'un mod (releases) : les correctifs sortent vite.
/// Dossier des miniatures réduites (le suffixe invalide les anciens caches en pleine taille).
pub const THUMBS_DIR: &str = "thumbs-128";

const FULL_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct InfoJsonLite {
    #[serde(default)]
    pub factorio_version: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Release {
    pub download_url: String,
    pub file_name: String,
    #[serde(default)]
    pub info_json: InfoJsonLite,
    #[serde(default)]
    pub released_at: String,
    pub version: String,
    #[serde(default)]
    pub sha1: String,
}

impl Release {
    pub fn ver(&self) -> Version {
        self.version.parse().unwrap_or_default()
    }
    pub fn deps(&self) -> Vec<Dependency> {
        let deps = if self.info_json.dependencies.is_empty() {
            vec!["base".to_string()]
        } else {
            self.info_json.dependencies.clone()
        };
        deps.iter().filter_map(|d| Dependency::parse(d)).collect()
    }
    pub fn factorio(&self) -> String {
        self.info_json.factorio_version.clone().unwrap_or_default()
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ModListEntry {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub downloads_count: u64,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub latest_release: Option<Release>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct ModFull {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub downloads_count: u64,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub releases: Vec<Release>,
    #[serde(default)]
    pub changelog: Option<String>,
}

/// Erreur passagère d'un téléchargement (réseau, serveur, fichier corrompu) : on réessaie.
#[derive(Debug)]
struct Transient(String);

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Transient {}

fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Transient>() || c.is::<reqwest::Error>() || c.is::<std::io::Error>())
}

/// Nombre de tentatives par téléchargement.
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// Sections du changelog (format Factorio) postérieures à `since`, la plus récente en premier.
pub fn changelog_since(text: &str, since: &Version) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<(Version, Vec<&str>)> = None;
    let flush = |cur: Option<(Version, Vec<&str>)>, out: &mut Vec<String>| {
        if let Some((v, lines)) = cur {
            if v > *since {
                out.push(lines.join("\n").trim_end().to_string());
            }
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t.chars().all(|c| c == '-') && !t.is_empty() {
            continue; // séparateur entre versions
        }
        if let Some(v) = t.strip_prefix("Version:") {
            flush(current.take(), &mut out);
            current = Some((v.trim().parse().unwrap_or_default(), vec![t]));
        } else if let Some((_, lines)) = current.as_mut() {
            if !t.starts_with("Date:") {
                lines.push(line.trim_end());
            }
        }
    }
    flush(current.take(), &mut out);
    out.join("\n\n")
}

impl ModFull {
    /// Releases triées de la plus récente à la plus ancienne.
    pub fn sorted_releases(&self) -> Vec<Release> {
        let mut r = self.releases.clone();
        r.sort_by_key(|b| std::cmp::Reverse(b.ver()));
        r
    }

    /// Releases chargeables par cette version du jeu, de la plus récente à la plus ancienne.
    pub fn compatible_releases(&self, game: &str) -> Vec<Release> {
        self.sorted_releases().into_iter().filter(|r| factorio_compatible(&r.factorio(), game)).collect()
    }

    /// Meilleure release compatible avec la version du jeu et la contrainte éventuelle.
    /// Aucune release incompatible n'est proposée en repli.
    pub fn best_release(&self, game: &str, dep: Option<&Dependency>) -> Option<Release> {
        self.compatible_releases(game)
            .into_iter()
            .find(|r| dep.map(|d| d.matches(&r.ver())).unwrap_or(true))
    }

    /// Versions de Factorio pour lesquelles le mod est publié (pour les messages d'erreur).
    pub fn published_for(&self) -> String {
        let mut v: Vec<String> = self.releases.iter().map(|r| r.factorio()).filter(|f| !f.is_empty()).collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.parse::<Version>().unwrap_or_default()));
        v.dedup();
        v.join(", ")
    }
}

#[derive(Deserialize)]
struct ListResponse<T> {
    results: Vec<T>,
}

pub struct Portal {
    client: reqwest::blocking::Client,
    cache_dir: PathBuf,
    full_cache: Mutex<HashMap<String, (Instant, ModFull)>>,
    no_thumb: Mutex<HashSet<String>>,
}

pub fn is_modpack_category(cat: Option<&str>) -> bool {
    matches!(cat, Some("mod-packs") | Some("modpacks"))
}

pub fn category_label(cat: &str) -> &'static str {
    match cat {
        "content" => "Contenu",
        "overhaul" => "Refonte",
        "tweaks" => "Ajustements",
        "utilities" => "Utilitaires",
        "scenarios" => "Scénarios",
        "mod-packs" | "modpacks" => "Modpack",
        "localizations" => "Traductions",
        "internal" => "Interne",
        _ => "Sans catégorie",
    }
}

impl Portal {
    pub fn new(cache_dir: PathBuf) -> Result<Portal> {
        fs::create_dir_all(cache_dir.join(THUMBS_DIR))?;
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("FactorioModManager/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Portal {
            client,
            cache_dir,
            full_cache: Mutex::new(HashMap::new()),
            no_thumb: Mutex::new(HashSet::new()),
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = self.client.get(url).send().with_context(|| format!("requête {url}"))?;
        if !resp.status().is_success() {
            bail!("le portail a répondu {} pour {url}", resp.status());
        }
        Ok(resp.json()?)
    }

    /// Liste complète des mods, mise en cache sur disque tant qu'elle a moins de `max_age`.
    pub fn list_all(&self, max_age: Duration) -> Result<Vec<ModListEntry>> {
        let path = self.cache_dir.join("portal_index.json");
        if !max_age.is_zero() {
            if let Ok(meta) = fs::metadata(&path) {
                let fresh = meta
                    .modified()
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .map(|age| age < max_age)
                    .unwrap_or(false);
                if fresh {
                    if let Ok(data) = fs::read(&path) {
                        if let Ok(list) = serde_json::from_slice::<Vec<ModListEntry>>(&data) {
                            return Ok(list);
                        }
                    }
                }
            }
        }
        let resp: ListResponse<ModListEntry> =
            self.get_json(&format!("{API}/api/mods?page_size=max&hide_deprecated=true"))?;
        let list = resp.results;
        let _ = fs::write(&path, serde_json::to_vec(&list)?);
        Ok(list)
    }

    /// Détails complets d'un mod (description, toutes les releases avec dépendances).
    pub fn full(&self, name: &str) -> Result<ModFull> {
        if let Some((at, m)) = self.full_cache.lock().unwrap().get(name) {
            if at.elapsed() < FULL_TTL {
                return Ok(m.clone());
            }
        }
        let m: ModFull = self.get_json(&format!("{API}/api/mods/{}/full", urlencode(name)))?;
        self.full_cache.lock().unwrap().insert(name.to_string(), (Instant::now(), m.clone()));
        Ok(m)
    }

    /// Releases disponibles pour une liste de mods (pour la détection des mises à jour).
    pub fn releases_for(&self, names: &[String]) -> Result<HashMap<String, Vec<Release>>> {
        #[derive(Deserialize)]
        struct Short {
            name: String,
            #[serde(default)]
            releases: Vec<Release>,
        }
        let mut out = HashMap::new();
        for chunk in names.chunks(80) {
            let list = chunk.iter().map(|n| urlencode(n)).collect::<Vec<_>>().join(",");
            let resp: ListResponse<Short> =
                self.get_json(&format!("{API}/api/mods?page_size=max&namelist={list}"))?;
            for s in resp.results {
                out.insert(s.name, s.releases);
            }
        }
        Ok(out)
    }

    /// Télécharge une release dans `dest_dir`, vérifie le SHA1 et renvoie le chemin du zip.
    /// Les erreurs passagères (coupure réseau, erreur du serveur, SHA1 invalide) sont réessayées.
    pub fn download(
        &self,
        release: &Release,
        username: &str,
        token: &str,
        dest_dir: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<PathBuf> {
        let mut attempt = 1;
        loop {
            match self.download_once(release, username, token, dest_dir, &mut progress) {
                Ok(p) => return Ok(p),
                Err(e) if attempt < DOWNLOAD_ATTEMPTS && is_transient(&e) => {
                    crate::journal::log(format!(
                        "Téléchargement de {} interrompu ({e:#}) : nouvel essai ({}/{DOWNLOAD_ATTEMPTS}).",
                        release.file_name,
                        attempt + 1
                    ));
                    std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn download_once(
        &self,
        release: &Release,
        username: &str,
        token: &str,
        dest_dir: &Path,
        progress: &mut impl FnMut(u64, u64),
    ) -> Result<PathBuf> {
        if username.is_empty() || token.is_empty() {
            bail!("identifiants du portail manquants (voir Paramètres)");
        }
        fs::create_dir_all(dest_dir)?;
        let url = format!(
            "{API}{}?username={}&token={}",
            release.download_url,
            urlencode(username),
            urlencode(token)
        );
        let mut resp = self.client.get(&url).send().context("téléchargement")?;
        if resp.status().is_server_error() {
            return Err(Transient(format!("le portail a répondu {}", resp.status())).into());
        }
        if !resp.status().is_success() {
            bail!("téléchargement refusé ({}) pour {}", resp.status(), release.file_name);
        }
        let final_url = resp.url().to_string();
        if final_url.contains("/login") {
            bail!("identifiants du portail invalides");
        }
        let total = resp.content_length().unwrap_or(0);
        let tmp = dest_dir.join(format!("{}.part", release.file_name));
        let mut file = fs::File::create(&tmp)?;
        let mut hasher = Sha1::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        drop(file);
        let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !release.sha1.is_empty() && !release.sha1.eq_ignore_ascii_case(&digest) {
            let _ = fs::remove_file(&tmp);
            return Err(Transient(format!("somme SHA1 invalide pour {}", release.file_name)).into());
        }
        let dest = dest_dir.join(&release.file_name);
        fs::rename(&tmp, &dest)?;
        Ok(dest)
    }

    /// Miniature du portail, mise en cache sur disque.
    pub fn thumbnail(&self, name: &str) -> Option<PathBuf> {
        let path = self.cache_dir.join(THUMBS_DIR).join(format!("{}.png", sanitize(name)));
        if path.exists() {
            return Some(path);
        }
        if self.no_thumb.lock().unwrap().contains(name) {
            return None;
        }
        let res = (|| -> Result<PathBuf> {
            #[derive(Deserialize)]
            struct Short {
                #[serde(default)]
                thumbnail: Option<String>,
            }
            let thumb = match self.full_cache.lock().unwrap().get(name) {
                Some((_, f)) => f.thumbnail.clone(),
                None => None,
            };
            let thumb = match thumb {
                Some(t) => Some(t),
                None => self.get_json::<Short>(&format!("{API}/api/mods/{}", urlencode(name)))?.thumbnail,
            };
            let thumb = thumb.ok_or_else(|| anyhow!("pas de miniature"))?;
            if thumb.contains("/.thumb") || thumb.is_empty() {
                bail!("pas de miniature");
            }
            let url = if thumb.starts_with("http") { thumb } else { format!("{ASSETS}{thumb}") };
            let bytes = self.client.get(&url).send()?.error_for_status()?.bytes()?;
            let small = crate::store::small_png(&bytes).unwrap_or_else(|| bytes.to_vec());
            fs::write(&path, small)?;
            Ok(path.clone())
        })();
        match res {
            Ok(p) => Some(p),
            Err(_) => {
                self.no_thumb.lock().unwrap().insert(name.to_string());
                None
            }
        }
    }
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

/// Recherche locale dans l'index du portail.
pub fn search<'a>(
    list: &'a [ModListEntry],
    query: &str,
    category: Option<&str>,
    factorio: Option<&str>,
    limit: usize,
) -> Vec<&'a ModListEntry> {
    let q = query.trim().to_lowercase();
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(u64, &ModListEntry)> = list
        .iter()
        .filter(|m| match category {
            Some(c) => m.category.as_deref() == Some(c) || (is_modpack_category(Some(c)) && is_modpack_category(m.category.as_deref())),
            None => true,
        })
        .filter(|m| match factorio {
            Some(f) => m
                .latest_release
                .as_ref()
                .map(|r| same_major_minor(&r.factorio(), f))
                .unwrap_or(false),
            None => true,
        })
        .filter_map(|m| {
            if terms.is_empty() {
                return Some((m.downloads_count, m));
            }
            let name = m.name.to_lowercase();
            let title = m.title.to_lowercase();
            let summary = m.summary.to_lowercase();
            let owner = m.owner.to_lowercase();
            let mut bonus = 0u64;
            for t in &terms {
                if name == *t || title == *t {
                    bonus += 1_000_000_000;
                } else if name.contains(t) || title.contains(t) {
                    bonus += 100_000_000;
                } else if summary.contains(t) || owner.contains(t) {
                    bonus += 1;
                } else {
                    return None;
                }
            }
            Some((bonus.saturating_add(m.downloads_count), m))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(limit).map(|(_, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelog_since_keeps_newer_sections() {
        let text = "---------------------------------------------------------------------------------------------------
Version: 1.2.0
Date: 2026-11-01
  Features:
    - Nouveau truc
---------------------------------------------------------------------------------------------------
Version: 1.1.0
Date: 2026-10-01
  Bugfixes:
    - Correctif
---------------------------------------------------------------------------------------------------
Version: 1.0.0
  Info:
    - Première version
";
        let log = changelog_since(text, &"1.0.0".parse().unwrap());
        assert!(log.starts_with("Version: 1.2.0"));
        assert!(log.contains("Correctif"));
        assert!(!log.contains("Première"));
        assert!(!log.contains("Date:"));
        assert!(changelog_since(text, &"1.2.0".parse().unwrap()).is_empty());
    }

    /// Test réseau : télécharge un petit mod avec les identifiants de player-data.json.
    /// Lancer avec `cargo test -- --ignored download_real_mod`.
    #[test]
    #[ignore]
    fn download_real_mod() {
        let user_dir = crate::factorio::default_user_dir().expect("dossier Factorio");
        let (u, t) = crate::factorio::read_credentials(&user_dir).expect("identifiants");
        let tmp = std::env::temp_dir().join(format!("fmm-dl-{}", crate::modpack::new_id()));
        let portal = Portal::new(tmp.join("cache")).unwrap();
        let full = portal.full("flib").unwrap();
        let rel = full.best_release("2.0", None).unwrap();
        let path = portal.download(&rel, &u, &t, &tmp, |_, _| {}).unwrap();
        let (info, _) = crate::store::read_zip(&path, false).unwrap();
        assert_eq!(info.name, "flib");
        assert_eq!(info.version.to_string(), rel.version);
        let _ = fs::remove_dir_all(tmp);
    }
}
