//! Mise à jour de l'application depuis les releases GitHub.
//!
//! - Windows : télécharge l'installateur NSIS (`*-setup.exe`), le lance en mode silencieux
//!   après la fermeture de l'app, puis relance l'app.
//! - Linux : remplace l'AppImage en cours d'exécution (`$APPIMAGE`) puis la relance.
//! - macOS : télécharge le `.dmg` et l'ouvre (l'utilisateur glisse l'app dans Applications).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::version::Version;

pub const REPO: &str = "YetAnotherRandomProject/FactorioModManager";

#[derive(Clone, Debug)]
pub struct AppRelease {
    pub version: Version,
    pub asset_name: String,
    pub asset_url: String,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

pub fn current_version() -> Version {
    env!("CARGO_PKG_VERSION").parse().unwrap_or_default()
}

/// Asset correspondant à la plateforme courante.
fn asset_matches(name: &str) -> bool {
    let n = name.to_lowercase();
    if cfg!(target_os = "windows") {
        n.ends_with("-setup.exe")
    } else if cfg!(target_os = "macos") {
        // Une image par architecture : « …_aarch64.dmg » ou « …_x64.dmg ».
        let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x64" };
        n.ends_with(".dmg") && n.contains(arch)
    } else {
        n.ends_with(".appimage")
    }
}

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("FactorioModManager/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()?)
}

/// Exécutable installé par l'installateur NSIS (installation par utilisateur).
#[cfg(target_os = "windows")]
fn installed_exe() -> Option<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")?;
    let exe = PathBuf::from(dir).join("Factorio Mod Manager").join("FactorioModManager.exe");
    exe.exists().then_some(exe)
}

/// Compilation locale (`cargo build`/`cargo run`) : l'installateur ne la remplacerait pas, la
/// proposer en boucle n'a pas de sens.
fn is_dev_build() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .is_some_and(|d| {
            let leaf = d.file_name().and_then(|f| f.to_str()).unwrap_or("").to_lowercase();
            let parent = d.parent().and_then(|p| p.file_name()).and_then(|f| f.to_str()).unwrap_or("").to_lowercase();
            parent == "target" && (leaf == "debug" || leaf == "release")
        })
}

/// Renvoie la dernière release si elle est plus récente que la version en cours.
pub fn check() -> Result<Option<AppRelease>> {
    if is_dev_build() {
        return Ok(None);
    }
    let resp = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // aucune release publiée
    }
    if !resp.status().is_success() {
        bail!("GitHub a répondu {}", resp.status());
    }
    let rel: GhRelease = resp.json()?;
    if rel.draft || rel.prerelease {
        return Ok(None);
    }
    let version: Version = rel
        .tag_name
        .trim_start_matches(['v', 'V'])
        .parse()
        .map_err(|e: String| anyhow!(e))?;
    if version <= current_version() {
        return Ok(None);
    }
    let Some(asset) = rel.assets.into_iter().find(|a| asset_matches(&a.name)) else {
        return Ok(None); // release sans binaire pour cette plateforme
    };
    Ok(Some(AppRelease {
        version,
        asset_name: asset.name,
        asset_url: asset.browser_download_url,
    }))
}

pub fn download(rel: &AppRelease, dir: &Path, mut progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let mut resp = client()?.get(&rel.asset_url).send()?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let dest = dir.join(&rel.asset_name);
    let tmp = dir.join(format!("{}.part", rel.asset_name));
    let mut file = fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    drop(file);
    fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Installe le fichier téléchargé. Sous Windows et Linux, l'application doit se fermer juste après.
/// Renvoie `true` si l'application doit quitter.
pub fn install(file: &Path) -> Result<bool> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // On relance la copie installée : l'exécutable en cours peut être ailleurs (copie portable),
        // et le relancer ramènerait l'ancienne version, qui proposerait de nouveau la mise à jour.
        let exe = installed_exe().map(Ok).unwrap_or_else(std::env::current_exe)?;
        let script = file.with_extension("cmd");
        // Attend la fermeture de l'app, installe en silencieux, puis relance.
        let body = format!(
            "@echo off\r\nping -n 3 127.0.0.1 >nul\r\n\"{}\" /S\r\nstart \"\" \"{}\"\r\n",
            file.display(),
            exe.display()
        );
        fs::write(&script, body)?;
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(&script)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("lancement de l'installateur")?;
        Ok(true)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let target = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("l'app n'a pas été lancée depuis une AppImage : mets-la à jour avec ton gestionnaire de paquets"))?;
        let staged = target.with_extension("new");
        fs::copy(file, &staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(&staged, &target)?;
        std::process::Command::new(&target).spawn()?;
        Ok(true)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(file).spawn()?;
        Ok(false)
    }
}
