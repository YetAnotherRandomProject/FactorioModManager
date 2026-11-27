//! Interaction avec l'installation de Factorio : chemins, identifiants, mod-list.json, lancement.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crate::version::BUILTIN_MODS;

/// Dossier des données utilisateur de Factorio (contient mods/, saves/, player-data.json).
pub fn default_user_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![base.config_dir().join("Factorio")]
    } else if cfg!(target_os = "macos") {
        vec![base.data_dir().join("factorio")]
    } else {
        vec![
            base.home_dir().join(".factorio"),
            base.home_dir().join(".var/app/com.valvesoftware.Steam/.factorio"),
        ]
    };
    candidates.iter().find(|p| p.exists()).cloned().or_else(|| candidates.into_iter().next())
}

/// Essaie de trouver l'exécutable : d'abord via le dernier log du jeu, sinon les chemins habituels.
pub fn detect_exe(user_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = user_dir {
        for log in ["factorio-current.log", "factorio-previous.log"] {
            if let Ok(text) = fs::read_to_string(dir.join(log)) {
                for line in text.lines().take(20) {
                    if let Some(idx) = line.find("Program arguments:") {
                        let args = &line[idx + "Program arguments:".len()..];
                        if let Some(start) = args.find('"') {
                            if let Some(end) = args[start + 1..].find('"') {
                                let p = PathBuf::from(&args[start + 1..start + 1 + end]);
                                if p.exists() {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let base = directories::BaseDirs::new();
    let home = base.as_ref().map(|b| b.home_dir().to_path_buf()).unwrap_or_default();
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![
            r"C:\Program Files (x86)\Steam\steamapps\common\Factorio\bin\x64\factorio.exe".into(),
            r"C:\Program Files\Steam\steamapps\common\Factorio\bin\x64\factorio.exe".into(),
            r"C:\Program Files\Factorio\bin\x64\factorio.exe".into(),
            r"D:\SteamLibrary\steamapps\common\Factorio\bin\x64\factorio.exe".into(),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            "/Applications/factorio.app/Contents/MacOS/factorio".into(),
            home.join("Library/Application Support/Steam/steamapps/common/Factorio/factorio.app/Contents/MacOS/factorio"),
        ]
    } else {
        vec![
            home.join(".steam/steam/steamapps/common/Factorio/bin/x64/factorio"),
            home.join(".local/share/Steam/steamapps/common/Factorio/bin/x64/factorio"),
            home.join("factorio/bin/x64/factorio"),
            "/opt/factorio/bin/x64/factorio".into(),
        ]
    };
    candidates.into_iter().find(|p| p.exists())
}

/// Dossier data/ de l'installation (contient base/, space-age/...).
pub fn data_dir(exe: &Path) -> Option<PathBuf> {
    let mut cur = exe.parent();
    for _ in 0..4 {
        let dir = cur?;
        for cand in [dir.join("data"), dir.join("Contents").join("data")] {
            if cand.join("base").join("info.json").exists() {
                return Some(cand);
            }
        }
        cur = dir.parent();
    }
    None
}

#[derive(Deserialize)]
struct BaseInfo {
    version: String,
}

/// Version installée du jeu (data/base/info.json, sinon le log).
pub fn detect_version(exe: Option<&Path>, user_dir: Option<&Path>) -> Option<String> {
    if let Some(data) = exe.and_then(data_dir) {
        if let Ok(text) = fs::read_to_string(data.join("base").join("info.json")) {
            if let Ok(info) = serde_json::from_str::<BaseInfo>(&text) {
                return Some(info.version);
            }
        }
    }
    let text = fs::read_to_string(user_dir?.join("factorio-current.log")).ok()?;
    let line = text.lines().next()?;
    let idx = line.find("Factorio ")?;
    let ver = line[idx + 9..].split_whitespace().next()?;
    Some(ver.to_string())
}

/// Mods intégrés présents dans l'installation (base + DLC), hors "core".
pub fn builtin_mods(exe: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(data) = exe.and_then(data_dir) {
        if let Ok(rd) = fs::read_dir(&data) {
            for e in rd.flatten() {
                let p = e.path();
                if p.join("info.json").exists() {
                    if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                        if n != "core" {
                            out.push(n.to_string());
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = BUILTIN_MODS.iter().filter(|n| **n != "core").map(|s| s.to_string()).collect();
    }
    out.sort_by_key(|n| (n != "base", n.clone()));
    out
}

#[derive(Deserialize)]
struct PlayerData {
    #[serde(rename = "service-username", default)]
    username: Option<String>,
    #[serde(rename = "service-token", default)]
    token: Option<String>,
}

/// Identifiants du portail depuis player-data.json.
pub fn read_credentials(user_dir: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(user_dir.join("player-data.json")).ok()?;
    let pd: PlayerData = serde_json::from_str(&text).ok()?;
    match (pd.username, pd.token) {
        (Some(u), Some(t)) if !u.is_empty() && !t.is_empty() => Some((u, t)),
        _ => None,
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModListItem {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ModList {
    pub mods: Vec<ModListItem>,
}

impl ModList {
    pub fn read(mods_dir: &Path) -> ModList {
        fs::read_to_string(mods_dir.join("mod-list.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn write(&self, mods_dir: &Path) -> Result<()> {
        fs::create_dir_all(mods_dir)?;
        let text = serde_json::to_string_pretty(self)?;
        let tmp = mods_dir.join("mod-list.json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, mods_dir.join("mod-list.json")).context("écriture de mod-list.json")?;
        Ok(())
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.mods.iter().any(|m| m.name == name && m.enabled)
    }
}

/// Lance le jeu, éventuellement avec un dossier de mods dédié et une sauvegarde.
pub fn launch(exe: &Path, mod_dir: Option<&Path>, save: Option<&Path>, extra: &[&str]) -> Result<Child> {
    let mut cmd = Command::new(exe);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    if let Some(m) = mod_dir {
        cmd.arg("--mod-directory").arg(m);
    }
    if let Some(s) = save {
        cmd.arg("--load-game").arg(s);
    }
    cmd.args(extra);
    cmd.spawn().with_context(|| format!("impossible de lancer {}", exe.display()))
}

/// Découpe "nom_1.2.3.zip" en ("nom", "1.2.3").
pub fn split_zip_name(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".zip")?;
    let (name, ver) = stem.rsplit_once('_')?;
    ver.parse::<crate::version::Version>().ok()?;
    Some((name.to_string(), ver.to_string()))
}
