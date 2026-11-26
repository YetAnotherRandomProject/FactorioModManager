//! Modpacks, état persistant et résolution de l'ensemble de mods à charger.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::write_atomic;
use crate::store::Index;
use crate::version::{Version, is_builtin};

fn t() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PackEntry {
    pub name: String,
    pub version: String,
    #[serde(default = "t")]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Modpack {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Mod du portail (catégorie "mod-packs") dont ce modpack est dérivé.
    #[serde(default)]
    pub base_mod: Option<String>,
    #[serde(default)]
    pub factorio_version: Option<String>,
    #[serde(default)]
    pub mods: Vec<PackEntry>,
    /// Mods intégrés (DLC) désactivés par ce modpack.
    #[serde(default)]
    pub disabled_builtins: Vec<String>,
    #[serde(default)]
    pub saves: Vec<PathBuf>,
}

impl Modpack {
    pub fn new(name: &str) -> Modpack {
        Modpack { id: new_id(), name: name.to_string(), ..Default::default() }
    }

    /// Ajoute ou met à jour un mod (une seule version par modpack).
    pub fn set_mod(&mut self, name: &str, version: &Version) {
        if let Some(e) = self.mods.iter_mut().find(|e| e.name == name) {
            e.version = version.to_string();
        } else {
            self.mods.push(PackEntry { name: name.to_string(), version: version.to_string(), enabled: true });
            self.mods.sort_by_key(|e| e.name.to_lowercase());
        }
    }

    pub fn entry(&self, name: &str) -> Option<&PackEntry> {
        self.mods.iter().find(|e| e.name == name)
    }
}

pub fn new_id() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{n:x}")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    #[serde(default)]
    pub packs: Vec<Modpack>,
    /// Identifiants des modpacks actifs, dans l'ordre d'activation.
    #[serde(default)]
    pub active: Vec<String>,
    /// Choix mémorisés pour les conflits de versions : nom du mod -> version.
    #[serde(default)]
    pub conflict_choices: BTreeMap<String, String>,
}

impl State {
    pub fn load(path: &Path) -> State {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
    pub fn pack(&self, id: &str) -> Option<&Modpack> {
        self.packs.iter().find(|p| p.id == id)
    }
    pub fn pack_mut(&mut self, id: &str) -> Option<&mut Modpack> {
        self.packs.iter_mut().find(|p| p.id == id)
    }
    pub fn is_active(&self, id: &str) -> bool {
        self.active.iter().any(|a| a == id)
    }
    pub fn active_packs(&self) -> Vec<&Modpack> {
        self.active.iter().filter_map(|id| self.pack(id)).collect()
    }
    /// Noms des modpacks qui utilisent (nom, version).
    pub fn packs_using(&self, name: &str, version: Option<&str>) -> Vec<String> {
        self.packs
            .iter()
            .filter(|p| p.mods.iter().any(|e| e.name == name && version.map(|v| v == e.version).unwrap_or(true)))
            .map(|p| p.name.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ConflictOption {
    pub version: String,
    pub packs: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Conflict {
    pub name: String,
    pub options: Vec<ConflictOption>,
}

#[derive(Clone, Debug, Default)]
pub struct Resolution {
    pub mods: BTreeMap<String, Version>,
    pub conflicts: Vec<Conflict>,
    pub disabled_builtins: BTreeSet<String>,
}

/// Calcule l'union des modpacks actifs. `choices` tranche les conflits déjà arbitrés.
pub fn resolve(state: &State, choices: &BTreeMap<String, String>) -> Resolution {
    let mut wanted: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut res = Resolution::default();
    for pack in state.active_packs() {
        for e in pack.mods.iter().filter(|e| e.enabled) {
            wanted
                .entry(e.name.clone())
                .or_default()
                .entry(e.version.clone())
                .or_default()
                .push(pack.name.clone());
        }
        res.disabled_builtins.extend(pack.disabled_builtins.iter().cloned());
    }
    for (name, versions) in wanted {
        let pick = if versions.len() == 1 {
            versions.keys().next().cloned()
        } else if let Some(c) = choices.get(&name).filter(|c| versions.contains_key(*c)) {
            Some(c.clone())
        } else {
            let mut opts: Vec<ConflictOption> = versions
                .iter()
                .map(|(v, p)| ConflictOption { version: v.clone(), packs: p.clone() })
                .collect();
            opts.sort_by_key(|o| std::cmp::Reverse(o.version.parse::<Version>().unwrap_or_default()));
            let first = opts[0].version.clone();
            res.conflicts.push(Conflict { name: name.clone(), options: opts });
            Some(first)
        };
        if let Some(v) = pick.and_then(|v| v.parse().ok()) {
            res.mods.insert(name, v);
        }
    }
    res
}

/// Problèmes de dépendances dans une résolution (mods absents du dépôt, dépendances manquantes...).
pub fn check(res: &Resolution, index: &Index, builtins: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (name, ver) in &res.mods {
        let Some(info) = crate::store::find(index, name, ver) else {
            out.push(format!("{name} {ver} n'est pas dans le dépôt (à télécharger)"));
            continue;
        };
        for dep in info.deps() {
            if is_builtin(&dep.name, builtins) {
                if dep.kind.is_required() && res.disabled_builtins.contains(&dep.name) {
                    out.push(format!("{name} requiert {} qui est désactivé", dep.name));
                }
                continue;
            }
            let present = res.mods.get(&dep.name);
            if dep.kind.is_required() {
                match present {
                    None => out.push(format!("{name} requiert {} {}", dep.name, dep.constraint_str())),
                    Some(v) if !dep.matches(v) => out.push(format!(
                        "{name} requiert {} {} (version chargée : {v})",
                        dep.name,
                        dep.constraint_str()
                    )),
                    _ => {}
                }
            } else if dep.kind == crate::version::DepKind::Incompatible && present.is_some() {
                out.push(format!("{name} est incompatible avec {}", dep.name));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(name: &str, mods: &[(&str, &str)]) -> Modpack {
        let mut p = Modpack::new(name);
        p.id = name.to_string();
        for (n, v) in mods {
            p.set_mod(n, &v.parse().unwrap());
        }
        p
    }

    #[test]
    fn union_and_conflicts() {
        let mut s = State::default();
        s.packs.push(pack("A", &[("flib", "0.15.0"), ("foo", "1.0.0")]));
        s.packs.push(pack("B", &[("flib", "0.16.0"), ("bar", "2.0.0")]));
        s.packs.push(pack("C", &[("baz", "1.0.0")]));
        s.active = vec!["A".into(), "B".into()];

        let r = resolve(&s, &BTreeMap::new());
        assert_eq!(r.mods.len(), 3);
        assert!(!r.mods.contains_key("baz"));
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].name, "flib");
        assert_eq!(r.conflicts[0].options[0].version, "0.16.0");

        let mut choices = BTreeMap::new();
        choices.insert("flib".to_string(), "0.15.0".to_string());
        let r = resolve(&s, &choices);
        assert!(r.conflicts.is_empty());
        assert_eq!(r.mods["flib"], Version(0, 15, 0));
    }

    #[test]
    fn disabled_entries_are_ignored() {
        let mut s = State::default();
        let mut a = pack("A", &[("foo", "1.0.0")]);
        a.mods[0].enabled = false;
        s.packs.push(a);
        s.active = vec!["A".into()];
        assert!(resolve(&s, &BTreeMap::new()).mods.is_empty());
    }
}
