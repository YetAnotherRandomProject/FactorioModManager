//! Application d'une résolution : synchronisation du dossier mods/ de Factorio
//! ou construction d'un dossier de profil pour --mod-directory.

use anyhow::Result;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::factorio::{ModList, ModListItem, split_zip_name};
use crate::modpack::Resolution;
use crate::store::{Index, find, link_or_copy};
use crate::version::Version;

#[derive(Debug, Default, Clone)]
pub struct SyncPlan {
    pub add: Vec<(String, Version, PathBuf)>,
    pub remove: Vec<PathBuf>,
    pub keep: usize,
    pub missing: Vec<String>,
    pub mod_list: ModList,
}

impl SyncPlan {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} mod(s) à ajouter, {} à retirer, {} déjà en place.",
            self.add.len(),
            self.remove.len(),
            self.keep
        );
        if !self.add.is_empty() {
            s.push_str("\n\nAjouts :\n");
            for (n, v, _) in self.add.iter().take(30) {
                s.push_str(&format!("  + {n} {v}\n"));
            }
            if self.add.len() > 30 {
                s.push_str(&format!("  … et {} autres\n", self.add.len() - 30));
            }
        }
        if !self.remove.is_empty() {
            s.push_str("\n\nRetraits (une copie reste dans le dépôt) :\n");
            for p in self.remove.iter().take(30) {
                s.push_str(&format!("  - {}\n", p.file_name().unwrap_or_default().to_string_lossy()));
            }
            if self.remove.len() > 30 {
                s.push_str(&format!("  … et {} autres\n", self.remove.len() - 30));
            }
        }
        if !self.missing.is_empty() {
            s.push_str("\n\nAbsents du dépôt (ignorés) :\n");
            for m in &self.missing {
                s.push_str(&format!("  ! {m}\n"));
            }
        }
        s
    }
}

fn zips_in(dir: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(f) = p.file_name().and_then(|f| f.to_str()) {
                if let Some((n, v)) = split_zip_name(f) {
                    out.push((n, v, p.clone()));
                }
            }
        }
    }
    out
}

fn builtin_items(res: &Resolution, builtins: &[String]) -> Vec<ModListItem> {
    builtins
        .iter()
        .map(|b| ModListItem { name: b.clone(), enabled: !res.disabled_builtins.contains(b), version: None })
        .collect()
}

/// Plan de synchronisation pour le dossier mods/ de Factorio.
/// Seuls les zips présents dans le dépôt sont considérés comme gérés (donc retirables).
pub fn plan(mods_dir: &Path, res: &Resolution, index: &Index, builtins: &[String]) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let existing = zips_in(mods_dir);
    let mut present: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, ver, path) in &existing {
        let v: Version = ver.parse().unwrap_or_default();
        let managed = find(index, name, &v).is_some();
        let wanted = res.mods.get(name) == Some(&v);
        if wanted {
            plan.keep += 1;
        } else if managed {
            plan.remove.push(path.clone());
            continue;
        }
        present.entry(name.clone()).or_default().push(ver.clone());
    }
    // Comparaison sur la version analysée : « mod_1.0.01.zip » correspond à 1.0.1.
    let present_set: std::collections::HashSet<(String, Version)> =
        existing.iter().map(|(n, v, _)| (n.clone(), v.parse().unwrap_or_default())).collect();
    for (name, ver) in &res.mods {
        if present_set.contains(&(name.clone(), *ver)) {
            continue;
        }
        match find(index, name, ver) {
            Some(info) => plan.add.push((name.clone(), *ver, info.path.clone())),
            None => plan.missing.push(format!("{name} {ver}")),
        }
    }

    // mod-list.json : on conserve les entrées non gérées, on force les mods résolus.
    let current = ModList::read(mods_dir);
    let removed_names: Vec<String> = plan
        .remove
        .iter()
        .filter_map(|p| p.file_name().and_then(|f| f.to_str()).and_then(split_zip_name))
        .map(|(n, _)| n)
        .filter(|n| !res.mods.contains_key(n) && !present.contains_key(n))
        .collect();
    let mut list = ModList { mods: builtin_items(res, builtins) };
    for item in current.mods {
        if builtins.contains(&item.name) || res.mods.contains_key(&item.name) || removed_names.contains(&item.name) {
            continue;
        }
        if index.contains_key(&item.name) && !present.contains_key(&item.name) {
            continue; // mod géré qui n'est plus dans le dossier
        }
        list.mods.push(item);
    }
    for (name, ver) in &res.mods {
        if plan.missing.iter().any(|m| m == &format!("{name} {ver}")) {
            continue;
        }
        list.mods.push(ModListItem { name: name.clone(), enabled: true, version: None });
    }
    plan.mod_list = list;
    plan
}

fn backup_once(path: &Path, suffix: &str) {
    if path.exists() {
        let orig = path.with_file_name(format!(
            "{}.{suffix}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        if !orig.exists() {
            let _ = fs::copy(path, orig);
        }
    }
}

/// Applique un plan au dossier mods/ (sauvegarde de mod-list.json et mod-settings.dat).
pub fn apply(mods_dir: &Path, plan: &SyncPlan, settings: Option<&Path>) -> Result<()> {
    fs::create_dir_all(mods_dir)?;
    let list_path = mods_dir.join("mod-list.json");
    backup_once(&list_path, "fmm-original");
    if list_path.exists() {
        let _ = fs::copy(&list_path, mods_dir.join("mod-list.json.fmm-backup"));
    }
    for p in &plan.remove {
        fs::remove_file(p)?;
    }
    for (name, ver, src) in &plan.add {
        link_or_copy(src, &mods_dir.join(format!("{name}_{ver}.zip")))?;
    }
    plan.mod_list.write(mods_dir)?;
    if let Some(s) = settings.filter(|s| s.exists()) {
        let dst = mods_dir.join("mod-settings.dat");
        backup_once(&dst, "fmm-original");
        fs::copy(s, dst)?;
    }
    Ok(())
}

/// Construit le dossier de profil (entièrement géré par l'app) pour --mod-directory.
pub fn build_profile(
    profile: &Path,
    res: &Resolution,
    index: &Index,
    builtins: &[String],
    settings: Option<&Path>,
) -> Result<Vec<String>> {
    fs::create_dir_all(profile)?;
    let mut missing = Vec::new();
    for (name, ver, path) in zips_in(profile) {
        let v: Version = ver.parse().unwrap_or_default();
        if res.mods.get(&name) != Some(&v) {
            fs::remove_file(path)?;
        }
    }
    let mut list = ModList { mods: builtin_items(res, builtins) };
    for (name, ver) in &res.mods {
        let Some(info) = find(index, name, ver) else {
            missing.push(format!("{name} {ver}"));
            continue;
        };
        let dst = profile.join(info.file_name());
        if !dst.exists() {
            link_or_copy(&info.path, &dst)?;
        }
        list.mods.push(ModListItem { name: name.clone(), enabled: true, version: None });
    }
    list.write(profile)?;
    if let Some(s) = settings.filter(|s| s.exists()) {
        fs::copy(s, profile.join("mod-settings.dat"))?;
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ModInfo;

    fn info(name: &str, ver: &str, path: PathBuf) -> ModInfo {
        ModInfo {
            name: name.into(),
            version: ver.parse().unwrap(),
            title: String::new(),
            author: String::new(),
            description: String::new(),
            factorio_version: "2.0".into(),
            dependencies: vec![],
            homepage: String::new(),
            path,
        }
    }

    #[test]
    fn plan_adds_removes_and_keeps_unmanaged() {
        let tmp = std::env::temp_dir().join(format!("fmm-test-{}", crate::modpack::new_id()));
        let mods = tmp.join("mods");
        let store = tmp.join("store");
        fs::create_dir_all(&mods).unwrap();
        fs::create_dir_all(&store).unwrap();
        // géré, à garder
        fs::write(mods.join("keep_1.0.0.zip"), b"x").unwrap();
        // géré, à retirer
        fs::write(mods.join("old_1.0.0.zip"), b"x").unwrap();
        // non géré
        fs::write(mods.join("manual_3.0.0.zip"), b"x").unwrap();
        // géré, version écrite avec un zéro non significatif
        fs::write(mods.join("dated_2026.11.01.zip"), b"x").unwrap();
        fs::write(store.join("new_2.0.0.zip"), b"x").unwrap();
        let mut index = Index::new();
        index.insert("keep".into(), vec![info("keep", "1.0.0", mods.join("keep_1.0.0.zip"))]);
        index.insert("old".into(), vec![info("old", "1.0.0", mods.join("old_1.0.0.zip"))]);
        index.insert("dated".into(), vec![info("dated", "2026.11.1", mods.join("dated_2026.11.01.zip"))]);
        index.insert("new".into(), vec![info("new", "2.0.0", store.join("new_2.0.0.zip"))]);
        ModList {
            mods: vec![
                ModListItem { name: "base".into(), enabled: true, version: None },
                ModListItem { name: "old".into(), enabled: true, version: None },
                ModListItem { name: "manual".into(), enabled: false, version: None },
            ],
        }
        .write(&mods)
        .unwrap();

        let mut res = Resolution::default();
        res.mods.insert("keep".into(), Version(1, 0, 0));
        res.mods.insert("new".into(), Version(2, 0, 0));
        res.mods.insert("dated".into(), Version(2026, 11, 1));
        let builtins = vec!["base".to_string(), "space-age".to_string()];
        let p = plan(&mods, &res, &index, &builtins);
        assert_eq!(p.keep, 2);
        assert_eq!(p.add.len(), 1);
        assert_eq!(p.remove, vec![mods.join("old_1.0.0.zip")]);
        apply(&mods, &p, None).unwrap();
        assert!(mods.join("new_2.0.0.zip").exists());
        assert!(!mods.join("old_1.0.0.zip").exists());
        assert!(mods.join("manual_3.0.0.zip").exists());
        let list = ModList::read(&mods);
        assert!(list.is_enabled("new") && list.is_enabled("keep") && list.is_enabled("space-age"));
        assert!(!list.mods.iter().any(|m| m.name == "old"));
        assert!(list.mods.iter().any(|m| m.name == "manual" && !m.enabled));
        assert!(mods.join("mod-list.json.fmm-original").exists());
        let _ = fs::remove_dir_all(tmp);
    }
}
