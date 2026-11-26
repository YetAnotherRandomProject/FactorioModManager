//! Paramètres de l'application et chemins de données.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub store: PathBuf,
    pub packs: PathBuf,
    pub profile: PathBuf,
    pub cache: PathBuf,
    pub downloads: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Paths> {
        let dirs = directories::ProjectDirs::from("", "", "FactorioModManager")
            .ok_or_else(|| anyhow!("dossier de données introuvable"))?;
        let data = dirs.data_dir().to_path_buf();
        let p = Paths {
            store: data.join("store"),
            packs: data.join("packs"),
            profile: data.join("profile").join("mods"),
            cache: dirs.cache_dir().to_path_buf(),
            downloads: data.join("downloads"),
            data,
        };
        for d in [&p.data, &p.store, &p.packs, &p.profile, &p.cache, &p.downloads] {
            fs::create_dir_all(d)?;
        }
        Ok(p)
    }

    pub fn pack_settings(&self, pack_id: &str) -> PathBuf {
        self.packs.join(pack_id).join("mod-settings.dat")
    }
}

fn t() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub factorio_user_dir: Option<PathBuf>,
    #[serde(default)]
    pub factorio_exe: Option<PathBuf>,
    /// Identifiants manuels (vides = lus depuis player-data.json).
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub token: String,
    /// Version de Factorio ciblée par défaut pour les téléchargements (ex. "2.0").
    #[serde(default)]
    pub target_factorio: String,
    #[serde(default = "t")]
    pub feat_mod_settings: bool,
    #[serde(default = "t")]
    pub feat_factorio_version: bool,
    #[serde(default = "t")]
    pub feat_export: bool,
    #[serde(default = "t")]
    pub feat_saves: bool,
    #[serde(default = "t")]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub auto_sync_on_activate: bool,
    #[serde(default)]
    pub export_include_mods: bool,
    #[serde(default = "t")]
    pub copy_back_settings: bool,
    /// Intervalle (minutes) entre deux vérifications des mises à jour et de l'index du portail.
    #[serde(default = "ten")]
    pub check_interval_minutes: u32,
    /// Vérifier les nouvelles versions de l'application sur GitHub.
    #[serde(default = "t")]
    pub auto_update_app: bool,
}

fn ten() -> u32 {
    10
}

impl Default for Config {
    fn default() -> Self {
        Config {
            factorio_user_dir: None,
            factorio_exe: None,
            username: String::new(),
            token: String::new(),
            target_factorio: String::new(),
            feat_mod_settings: true,
            feat_factorio_version: true,
            feat_export: true,
            feat_saves: true,
            auto_check_updates: true,
            auto_sync_on_activate: false,
            export_include_mods: false,
            copy_back_settings: true,
            check_interval_minutes: 10,
            auto_update_app: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
    pub fn check_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.check_interval_minutes.max(1)) * 60)
    }
    pub fn mods_dir(&self) -> Option<PathBuf> {
        self.factorio_user_dir.as_ref().map(|d| d.join("mods"))
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
