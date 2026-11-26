//! Dépôt central : toutes les versions de tous les mods, rangées par nom.
//! Disposition : <store>/<nom>/<nom>_<version>.zip

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::portal::sanitize;
use crate::version::{Dependency, Version};

#[derive(Clone, Debug)]
pub struct ModInfo {
    pub name: String,
    pub version: Version,
    pub title: String,
    pub author: String,
    pub description: String,
    pub factorio_version: String,
    pub dependencies: Vec<String>,
    pub homepage: String,
    pub path: PathBuf,
}

impl ModInfo {
    pub fn deps(&self) -> Vec<Dependency> {
        self.dependencies.iter().filter_map(|d| Dependency::parse(d)).collect()
    }
    pub fn file_name(&self) -> String {
        format!("{}_{}.zip", self.name, self.version)
    }
}

#[derive(Deserialize)]
struct InfoJson {
    name: String,
    version: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    factorio_version: String,
    #[serde(default)]
    dependencies: Option<Vec<String>>,
    #[serde(default)]
    homepage: String,
}

/// Index : nom -> versions triées (la plus récente en premier).
pub type Index = BTreeMap<String, Vec<ModInfo>>;

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    pub icons: PathBuf,
}

pub enum AddMode {
    Move,
    LinkOrCopy,
}

/// Lit info.json (et éventuellement thumbnail.png) dans le zip d'un mod.
pub fn read_zip(path: &Path, want_thumb: bool) -> Result<(ModInfo, Option<Vec<u8>>)> {
    let file = fs::File::open(path).with_context(|| format!("ouverture de {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| format!("zip invalide: {}", path.display()))?;
    let info_name = zip
        .file_names()
        .filter(|n| n.ends_with("/info.json") && n.matches('/').count() == 1)
        .min_by_key(|n| n.len())
        .or_else(|| zip.file_names().find(|n| *n == "info.json"))
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("info.json introuvable dans {}", path.display()))?;
    let mut text = String::new();
    zip.by_name(&info_name)?.read_to_string(&mut text)?;
    let text = text.trim_start_matches('\u{feff}');
    let info: InfoJson = serde_json::from_str(text).with_context(|| format!("info.json invalide dans {}", path.display()))?;
    let thumb = if want_thumb {
        let prefix = &info_name[..info_name.len() - "info.json".len()];
        let tname = format!("{prefix}thumbnail.png");
        match zip.by_name(&tname) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).ok().map(|_| buf)
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let version = info.version.parse::<Version>().map_err(|e| anyhow!(e))?;
    Ok((
        ModInfo {
            name: info.name,
            version,
            title: info.title,
            author: info.author,
            description: info.description,
            factorio_version: info.factorio_version,
            dependencies: info.dependencies.unwrap_or_else(|| vec!["base".into()]),
            homepage: info.homepage,
            path: path.to_path_buf(),
        },
        thumb,
    ))
}

/// Taille maximale des icônes mises en cache (les miniatures d'origine peuvent dépasser 1000 px,
/// ce qui coûte cher au rendu de chaque image).
pub const ICON_SIZE: u32 = 128;

/// Réduit une image (PNG/JPEG) à `ICON_SIZE` et la ré-encode en PNG.
pub fn small_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > ICON_SIZE || img.height() > ICON_SIZE { img.thumbnail(ICON_SIZE, ICON_SIZE) } else { img };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

pub fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        fs::remove_file(dst)?;
    }
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).with_context(|| format!("copie de {} vers {}", src.display(), dst.display()))?;
    }
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf, icons: PathBuf) -> Result<Store> {
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&icons)?;
        Ok(Store { root, icons })
    }

    pub fn scan(&self) -> Index {
        let mut index: Index = BTreeMap::new();
        let Ok(dirs) = fs::read_dir(&self.root) else { return index };
        for d in dirs.flatten() {
            let Ok(files) = fs::read_dir(d.path()) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("zip") {
                    continue;
                }
                match read_zip(&p, false) {
                    Ok((info, _)) => index.entry(info.name.clone()).or_default().push(info),
                    Err(e) => eprintln!("ignoré: {e:#}"),
                }
            }
        }
        for v in index.values_mut() {
            v.sort_by(|a, b| b.version.cmp(&a.version));
        }
        index
    }

    /// Ajoute un zip au dépôt et renvoie ses infos.
    pub fn add_file(&self, src: &Path, mode: AddMode) -> Result<ModInfo> {
        let (mut info, _) = read_zip(src, false)?;
        let dir = self.root.join(&info.name);
        fs::create_dir_all(&dir)?;
        let dest = dir.join(info.file_name());
        if dest.exists() {
            if let AddMode::Move = mode {
                let _ = fs::remove_file(src);
            }
        } else {
            match mode {
                AddMode::Move => {
                    if fs::rename(src, &dest).is_err() {
                        fs::copy(src, &dest)?;
                        let _ = fs::remove_file(src);
                    }
                }
                AddMode::LinkOrCopy => link_or_copy(src, &dest)?,
            }
        }
        info.path = dest;
        Ok(info)
    }

    pub fn remove(&self, info: &ModInfo) -> Result<()> {
        fs::remove_file(&info.path)?;
        let icon = self.icon_path(&info.name, &info.version);
        let _ = fs::remove_file(icon);
        if let Some(dir) = info.path.parent() {
            let _ = fs::remove_dir(dir); // ne supprime que si vide
        }
        Ok(())
    }

    fn icon_path(&self, name: &str, v: &Version) -> PathBuf {
        self.icons.join(format!("{}_{}.png", sanitize(name), v))
    }

    /// Icône extraite du zip (thumbnail.png), mise en cache.
    pub fn icon(&self, info: &ModInfo) -> Option<PathBuf> {
        let p = self.icon_path(&info.name, &info.version);
        if p.exists() {
            return Some(p);
        }
        let none_marker = p.with_extension("none");
        if none_marker.exists() {
            return None;
        }
        match read_zip(&info.path, true) {
            Ok((_, Some(bytes))) => {
                let small = small_png(&bytes).unwrap_or(bytes);
                fs::write(&p, small).ok().map(|_| p)
            }
            _ => {
                let _ = fs::write(none_marker, b"");
                None
            }
        }
    }
}

pub fn find<'a>(index: &'a Index, name: &str, version: &Version) -> Option<&'a ModInfo> {
    index.get(name)?.iter().find(|m| &m.version == version)
}
