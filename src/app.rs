//! Contrôleur : relie l'interface Slint au cœur (dépôt, portail, modpacks, synchro).

use anyhow::{Context, Result, anyhow, bail};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::check::{self, CheckDirs};
use crate::config::{Config, Paths};
use crate::factorio::{self, ModList};
use crate::instance;
use crate::journal;
use crate::modpack::{self, Conflict, Modpack, PackEntry, Resolution, State};
use crate::portal::{self, ModListEntry, Portal, Release};
use crate::store::{self, AddMode, Index, Store};
use crate::sync::{self, SyncPlan};
use crate::transfer;
use crate::updater::{self, AppRelease};
use crate::version::{DepKind, Dependency, Version, factorio_compatible, is_builtin, same_major_minor};
use crate::{
    AddRow, AppWindow, FailRow, IssueRow, BuiltinRow, ConflictRow, DepRow, ModRow, PackModRow, PackRow, SaveRow, SearchRow, UpdOptRow,
    UpdRow, VersionRow,
};

type Shared = Arc<Mutex<Core>>;

const CATEGORIES: &[(&str, Option<&str>)] = &[
    ("Toutes catégories", None),
    ("Modpacks", Some("mod-packs")),
    ("Contenu", Some("content")),
    ("Refonte", Some("overhaul")),
    ("Ajustements", Some("tweaks")),
    ("Utilitaires", Some("utilities")),
    ("Scénarios", Some("scenarios")),
    ("Traductions", Some("localizations")),
    ("Interne", Some("internal")),
    ("Sans catégorie", Some("no-category")),
];
const VERSIONS: &[&str] = &["Toutes versions", "2.1", "2.0", "1.1", "1.0", "0.18", "0.17"];

// ---------------------------------------------------------------------------
// État du cœur
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DlItem {
    name: String,
    kind: DepKind,
    constraint: Option<Dependency>,
    release: Option<Release>,
    installed: Option<Version>,
    checked: bool,
    checkable: bool,
    status: String,
}

#[derive(Clone, Debug)]
struct DlSession {
    name: String,
    title: String,
    category: Option<String>,
    releases: Vec<Release>,
    version_index: usize,
    items: Vec<DlItem>,
    /// Proposer « Créer un modpack à partir de ce mod » (jamais depuis un modpack existant :
    /// on met alors simplement ce modpack à jour).
    offer_pack: bool,
}

/// Mod à mettre à jour (fenêtre « Mises à jour »).
#[derive(Clone, Debug)]
struct UpdItem {
    name: String,
    title: String,
    from: Version,
    to: Version,
    /// Release à télécharger ; `None` si la version est déjà dans le dépôt.
    release: Option<Release>,
    changelog: String,
    checked: bool,
}

/// Dépendance apparue avec une mise à jour.
#[derive(Clone, Debug)]
struct UpdOpt {
    name: String,
    /// Mods mis à jour qui l'ont ajoutée.
    for_mods: Vec<String>,
    required: bool,
    release: Option<Release>,
    installed: Option<Version>,
    checked: bool,
    checkable: bool,
    status: String,
}

#[derive(Clone, Debug, Default)]
struct UpdSession {
    items: Vec<UpdItem>,
    opts: Vec<UpdOpt>,
    /// Mise à jour d'un seul modpack (sinon : tous les modpacks qui suivent les mises à jour).
    pack: Option<String>,
}

enum After {
    Nothing,
    Sync,
    Launch(Option<PathBuf>),
}

enum Confirm {
    Sync(SyncPlan, Option<PathBuf>),
    DeletePack(String),
    DeleteVersion(String, Version),
    /// Suppression groupée du dépôt (mods non utilisés par un modpack).
    BulkDelete(Vec<String>),
    /// Nettoyage : anciennes versions qu'aucun modpack n'utilise.
    CleanRepo(Vec<store::ModInfo>),
}

/// Vérification du chargement des mods en cours (instance isolée de Factorio).
struct CheckRun {
    id: u64,
    fp: String,
    cancel: Arc<AtomicBool>,
    /// Lancement demandé une fois la vérification réussie (avec la sauvegarde éventuelle).
    launch_after: Option<Option<PathBuf>>,
    bisecting: bool,
    /// Demandée par l'utilisateur : la fenêtre d'erreur s'ouvre en cas d'échec.
    manual: bool,
}

/// Dernière erreur de chargement ou d'exécution relevée.
#[derive(Clone)]
struct LastFailure {
    fp: String,
    failure: check::Failure,
    /// Erreur survenue en partie : le chargement, lui, a fonctionné.
    runtime: bool,
    /// Conclusion de la recherche du mod en conflit.
    note: String,
    save: Option<PathBuf>,
}

/// Mod en cours de téléchargement, affiché grisé avec sa progression dans les listes.
#[derive(Clone, Debug)]
struct Pending {
    version: String,
    title: String,
    progress: f32,
}

pub struct Core {
    paths: Paths,
    config: Config,
    state: State,
    store: Store,
    index: Index,
    portal: Arc<Portal>,
    portal_list: Option<Arc<Vec<ModListEntry>>>,
    portal_list_at: Option<Instant>,
    last_check: Option<Instant>,
    checking: bool,
    app_update: Option<AppRelease>,
    updates: BTreeMap<String, Release>,
    builtins: Vec<String>,
    game_version: Option<String>,
    icons: HashMap<String, PathBuf>,
    session_choices: BTreeMap<String, String>,
    selected_mod: Option<String>,
    selected_pack: Option<String>,
    dl: Option<DlSession>,
    dl_gen: u64,
    conflicts: Vec<Conflict>,
    conflict_sel: Vec<usize>,
    after_conflict: After,
    confirm: Option<Confirm>,
    /// Mods cochés dans la bibliothèque pour une action groupée.
    lib_selection: BTreeSet<String>,
    /// Téléchargements en cours, par nom de mod.
    pending: BTreeMap<String, Pending>,
    check_dirs: CheckDirs,
    check: Option<CheckRun>,
    check_gen: u64,
    failure: Option<LastFailure>,
    game_running: bool,
    /// Fichiers introuvables relevés par la dernière vérification de chaque ensemble.
    pending_missing: HashMap<String, BTreeSet<String>>,
    upd: Option<UpdSession>,
}

impl Core {
    fn config_path(&self) -> PathBuf {
        self.paths.data.join("config.json")
    }
    fn state_path(&self) -> PathBuf {
        self.paths.data.join("state.json")
    }
    fn save_state(&self) {
        if let Err(e) = self.state.save(&self.state_path()) {
            eprintln!("sauvegarde de l'état: {e:#}");
        }
    }
    fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path()) {
            eprintln!("sauvegarde des paramètres: {e:#}");
        }
    }
    fn credentials(&self) -> Option<(String, String)> {
        if !self.config.username.is_empty() && !self.config.token.is_empty() {
            return Some((self.config.username.clone(), self.config.token.clone()));
        }
        factorio::read_credentials(self.config.factorio_user_dir.as_ref()?)
    }
    /// Version de Factorio à laquelle les mods doivent correspondre : celle du jeu installé.
    /// Si le jeu n'est pas détecté : version des paramètres, puis celle du modpack.
    fn target_factorio(&self, pack: Option<&Modpack>) -> String {
        if let Some(v) = self.game_version.as_ref().and_then(|v| v.parse::<Version>().ok()) {
            return v.major_minor();
        }
        if !self.config.target_factorio.is_empty() {
            return self.config.target_factorio.clone();
        }
        pack.and_then(|p| p.factorio_version.clone()).unwrap_or_default()
    }
    fn choices(&self) -> BTreeMap<String, String> {
        let mut c = self.state.conflict_choices.clone();
        c.extend(self.session_choices.clone());
        c
    }
    fn resolution(&self) -> Resolution {
        modpack::resolve(&self.state, &self.choices())
    }
    fn settings_source(&self) -> Option<(String, PathBuf)> {
        if !self.config.feat_mod_settings {
            return None;
        }
        self.state
            .active
            .iter()
            .map(|id| (id.clone(), self.paths.pack_settings(id)))
            .find(|(_, p)| p.exists())
    }
    /// Réglages des mods utilisés au lancement : ceux du modpack, sinon ceux restés dans le profil.
    fn effective_settings(&self) -> Option<PathBuf> {
        self.settings_source()
            .map(|(_, p)| p)
            .or_else(|| Some(self.paths.profile.join("mod-settings.dat")))
            .filter(|p| p.exists())
    }
    fn launch_fingerprint(&self, res: &Resolution) -> String {
        check::fingerprint(res, self.game_version.as_deref().unwrap_or(""), self.effective_settings().as_deref())
    }
    /// Mods pouvant être mis en cause (hors jeu de base et DLC) et leurs titres.
    fn known_and_titles(&self, res: &Resolution) -> (BTreeSet<String>, BTreeMap<String, String>) {
        (res.mods.keys().cloned().collect(), check::titles(&self.index, res))
    }
    fn refresh_factorio(&mut self) {
        let exe = self.config.factorio_exe.clone();
        self.builtins = factorio::builtin_mods(exe.as_deref());
        self.game_version = factorio::detect_version(exe.as_deref(), self.config.factorio_user_dir.as_deref());
    }
}

// ---------------------------------------------------------------------------
// Utilitaires UI
// ---------------------------------------------------------------------------

static BUSY: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static IMAGES: RefCell<HashMap<PathBuf, Option<Image>>> = RefCell::new(HashMap::new());
}

fn load_image(path: Option<&PathBuf>) -> Option<Image> {
    let path = path?;
    IMAGES.with(|cache| {
        cache
            .borrow_mut()
            .entry(path.clone())
            .or_insert_with(|| Image::load_from_path(path).ok())
            .clone()
    })
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn strings(v: Vec<String>) -> ModelRc<SharedString> {
    model(v.into_iter().map(SharedString::from).collect())
}

/// Titre affichable : certains info.json contiennent des titres de gabarit (« [MOD DISPLAY NAME] »).
fn display_title(title: &str, name: &str) -> String {
    let t = one_line(title);
    if t.is_empty() || (t.starts_with('[') && t.ends_with(']')) { name.to_string() } else { t }
}

/// Texte sur une seule ligne : les retours à la ligne et espaces multiples (fréquents dans les
/// résumés du portail) deviennent un espace, pour ne pas déborder des lignes de hauteur fixe.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn letter(s: &str) -> SharedString {
    s.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into()
}

fn short(s: &str, max: usize) -> String {
    let line = one_line(s);
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Taille lisible (« 12,3 Mo »).
fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.2} Go", mb / 1024.0).replace('.', ",")
    } else if mb >= 1.0 {
        format!("{mb:.1} Mo").replace('.', ",")
    } else {
        format!("{} Ko", bytes.div_ceil(1024))
    }
}

fn local_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%d/%m/%Y").to_string())
        .unwrap_or_default()
}

fn now_label() -> String {
    chrono::Local::now().format("%d/%m/%Y %H:%M").to_string()
}

fn file_size(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn post_status(w: &Weak<AppWindow>, msg: impl Into<String>) {
    let msg = msg.into();
    let _ = w.upgrade_in_event_loop(move |ui| ui.set_status(msg.into()));
}

fn post_progress(w: &Weak<AppWindow>, p: f32) {
    let _ = w.upgrade_in_event_loop(move |ui| ui.set_progress(p));
}

fn post_refresh(w: &Weak<AppWindow>, c: &Shared) {
    let c = c.clone();
    let _ = w.upgrade_in_event_loop(move |ui| {
        let k = c.lock().unwrap();
        refresh_all(&ui, &k);
    });
}

/// Lance une tâche en arrière-plan ; son résultat s'affiche dans la barre d'état.
fn task<F>(ui: &AppWindow, core: &Shared, label: &str, f: F)
where
    F: FnOnce(&Shared, &Weak<AppWindow>) -> Result<String> + Send + 'static,
{
    BUSY.fetch_add(1, Ordering::SeqCst);
    ui.set_busy(true);
    ui.set_status(label.into());
    let c = core.clone();
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let msg = match f(&c, &w) {
            Ok(m) => m,
            Err(e) => format!("Erreur : {e:#}"),
        };
        let c2 = c.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            let left = BUSY.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
            ui.set_busy(left > 0);
            ui.set_progress(-1.0);
            if !msg.is_empty() {
                ui.set_status(msg.into());
            }
            let k = c2.lock().unwrap();
            refresh_all(&ui, &k);
        });
    });
}

// ---------------------------------------------------------------------------
// Rafraîchissement des vues
// ---------------------------------------------------------------------------

fn refresh_all(ui: &AppWindow, k: &Core) {
    refresh_library(ui, k);
    refresh_detail(ui, k);
    refresh_packs(ui, k);
    refresh_pack_detail(ui, k, false);
}

fn portal_summary<'a>(k: &'a Core, name: &str) -> Option<&'a ModListEntry> {
    k.portal_list.as_ref()?.iter().find(|m| m.name == name)
}

/// Pertinence d'un mod pour la recherche (plus petit = mieux classé) et indication de l'endroit
/// où la recherche a été trouvée, `None` s'il ne correspond pas.
/// Priorité : ID, puis nom affiché, puis modpacks, puis auteur. Chaque mot de la requête doit être
/// trouvé quelque part ; le rang est celui du mot le moins bien placé.
fn library_match(filter: &str, id: &str, title: &str, packs: &[String], author: &str) -> Option<(u8, String)> {
    let lid = id.to_lowercase();
    let ltitle = title.to_lowercase();
    let lauthor = author.to_lowercase();
    let lpacks: Vec<String> = packs.iter().map(|p| p.to_lowercase()).collect();
    let mut worst = (0u8, String::new());
    for word in filter.split_whitespace() {
        // L'ID n'est pas affiché dans les listes : on le montre s'il est la raison du résultat.
        let id_hint = || if ltitle.contains(word) { String::new() } else { format!("ID : {id}") };
        let (rank, hint) = if lid == word {
            (0, id_hint())
        } else if lid.starts_with(word) {
            (1, id_hint())
        } else if lid.contains(word) {
            (2, id_hint())
        } else if ltitle.starts_with(word) {
            (3, String::new())
        } else if ltitle.contains(word) {
            (4, String::new())
        } else if lpacks.iter().any(|p| p.contains(word)) {
            let found: Vec<&str> =
                packs.iter().zip(&lpacks).filter(|(_, l)| l.contains(word)).map(|(p, _)| p.as_str()).collect();
            (5, format!("Trouvé dans le modpack : {}", found.join(", ")))
        } else if lauthor.contains(word) {
            (6, format!("Auteur : {author}"))
        } else {
            return None;
        };
        if rank > worst.0 || (rank == worst.0 && worst.1.is_empty()) {
            worst = (rank, hint);
        }
    }
    Some(worst)
}

fn library_rank(filter: &str, id: &str, title: &str, packs: &[String], author: &str) -> Option<u8> {
    library_match(filter, id, title, packs, author).map(|(r, _)| r)
}

fn refresh_library(ui: &AppWindow, k: &Core) {
    let filter = ui.get_library_filter().trim().to_lowercase();
    let mode = ui.get_library_mode();
    let sort = ui.get_library_sort();
    let game = k.game_version.as_ref().and_then(|v| v.parse::<Version>().ok()).map(|v| v.major_minor()).unwrap_or_default();
    let res = k.resolution();
    let mut rows = Vec::new();
    for (name, versions) in &k.index {
        let latest = &versions[0];
        let packs = k.state.packs_using(name, None);
        let update = k.updates.get(name).map(|r| r.version.clone()).unwrap_or_default();
        let active = res.mods.contains_key(name);
        let incompatible = !game.is_empty() && !factorio_compatible(&latest.factorio_version, &game);
        let keep = match mode {
            1 => active,
            2 => packs.is_empty(),
            3 => !update.is_empty(),
            4 => incompatible,
            _ => true,
        };
        if !keep {
            continue;
        }
        let title = display_title(&latest.title, name);
        let Some((rank, hint)) = library_match(&filter, name, &title, &packs, &latest.author) else {
            continue;
        };
        // Clé de tri (la plus grande en premier) et information affichée sur la ligne.
        let (key, sort_info) = match sort {
            1 => {
                let t = versions
                    .iter()
                    .filter_map(|v| fs::metadata(&v.path).and_then(|m| m.modified()).ok())
                    .max()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (t, if t > 0 { format!("reçu le {}", local_date(t)) } else { String::new() })
            }
            2 => {
                let size: u64 = versions.iter().map(|v| file_size(&v.path)).sum();
                (size as i64, human_size(size))
            }
            3 => {
                let t = portal_summary(k, name)
                    .and_then(|m| m.latest_release.as_ref())
                    .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.released_at).ok())
                    .map(|d| d.timestamp())
                    .unwrap_or(0);
                (t, if t > 0 { format!("MàJ le {}", local_date(t)) } else { String::new() })
            }
            _ => (0, String::new()),
        };
        let icon = load_image(k.icons.get(name));
        rows.push((rank, key, ModRow {
            name: name.into(),
            letter: letter(&title),
            title: title.into(),
            summary: short(&latest.description, 160).into(),
            has_icon: icon.is_some(),
            icon: icon.unwrap_or_default(),
            version: latest.version.to_string().into(),
            versions_count: versions.len() as i32,
            update: update.into(),
            packs: packs.join(", ").into(),
            hint: hint.into(),
            sort_info: sort_info.into(),
            active,
            incompatible,
            checked: k.lib_selection.contains(name),
            downloading: k.pending.contains_key(name),
            progress: k.pending.get(name).map(|p| p.progress).unwrap_or(0.0),
        }));
    }
    // Mods pas encore dans le dépôt : affichés grisés pendant leur téléchargement.
    for (name, p) in k.pending.iter().filter(|(n, _)| !k.index.contains_key(*n)) {
        let packs = k.state.packs_using(name, None);
        let Some((rank, hint)) = library_match(&filter, name, &p.title, &packs, "") else {
            continue;
        };
        if mode != 0 {
            continue;
        }
        rows.push((rank, 0, ModRow {
            name: name.into(),
            letter: letter(&p.title),
            title: p.title.clone().into(),
            summary: "Téléchargement en cours…".into(),
            has_icon: false,
            icon: Image::default(),
            version: p.version.clone().into(),
            versions_count: 0,
            update: "".into(),
            packs: packs.join(", ").into(),
            hint: hint.into(),
            sort_info: "".into(),
            active: false,
            incompatible: false,
            checked: false,
            downloading: true,
            progress: p.progress,
        }));
    }
    rows.sort_by_cached_key(|(rank, key, r)| (*rank, std::cmp::Reverse(*key), r.title.to_lowercase()));
    let rows: Vec<ModRow> = rows.into_iter().map(|(_, _, r)| r).collect();
    ui.set_library_total(k.index.len() as i32);
    ui.set_updates_count(k.updates.len() as i32);
    ui.set_lib_selected_count(k.lib_selection.len() as i32);
    ui.set_library(model(rows));
    ui.set_selected_mod(k.selected_mod.clone().unwrap_or_default().into());
}

fn refresh_detail(ui: &AppWindow, k: &Core) {
    let Some(versions) = k.selected_mod.as_ref().and_then(|n| k.index.get(n)) else {
        ui.set_detail_visible(false);
        return;
    };
    let latest = &versions[0];
    let name = &latest.name;
    let title = display_title(&latest.title, name);
    ui.set_detail_visible(true);
    ui.set_detail_name(name.into());
    ui.set_detail_letter(letter(&title));
    ui.set_detail_title(title.into());
    ui.set_detail_author(latest.author.clone().into());
    ui.set_detail_factorio(latest.factorio_version.clone().into());
    let summary = portal_summary(k, name)
        .map(|m| m.summary.clone())
        .filter(|s| s.trim() != latest.description.trim())
        .unwrap_or_default();
    ui.set_detail_summary(summary.into());
    ui.set_detail_description(latest.description.clone().into());
    ui.set_detail_homepage(latest.homepage.clone().into());
    ui.set_detail_update(k.updates.get(name).map(|r| format!("version {}", r.version)).unwrap_or_default().into());
    let icon = load_image(k.icons.get(name));
    ui.set_detail_has_icon(icon.is_some());
    ui.set_detail_icon(icon.unwrap_or_default());
    let rows: Vec<VersionRow> = versions
        .iter()
        .map(|m| {
            let packs = k.state.packs_using(name, Some(&m.version.to_string()));
            VersionRow {
                version: m.version.to_string().into(),
                factorio: m.factorio_version.clone().into(),
                used: !packs.is_empty(),
                packs: packs.join(", ").into(),
            }
        })
        .collect();
    ui.set_detail_versions(model(rows));
    ui.set_detail_version_names(strings(versions.iter().map(|m| m.version.to_string()).collect()));
    let mut deps: Vec<(i32, String)> = latest
        .deps()
        .iter()
        .map(|d| (d.kind.group(), format!("{} : {} {}", d.kind.label(), d.name, d.constraint_str())))
        .collect();
    deps.sort();
    ui.set_detail_deps(strings(deps.into_iter().map(|(_, s)| s).collect()));
    if ui.get_add_version_index() >= versions.len() as i32 {
        ui.set_add_version_index(0);
    }
}

fn refresh_packs(ui: &AppWindow, k: &Core) {
    let rows: Vec<PackRow> = k
        .state
        .packs
        .iter()
        .map(|p| PackRow {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            description: p.description.clone().into(),
            active: k.state.is_active(&p.id),
            mods_count: p.mods.len() as i32,
            base_mod: p.base_mod.clone().unwrap_or_default().into(),
            factorio: p.factorio_version.clone().unwrap_or_default().into(),
            has_settings: k.paths.pack_settings(&p.id).exists(),
            saves_count: p.saves.len() as i32,
        })
        .collect();
    ui.set_packs(model(rows));
    ui.set_pack_names(strings(k.state.packs.iter().map(|p| p.name.clone()).collect()));
    if ui.get_add_pack_index() >= k.state.packs.len() as i32 {
        ui.set_add_pack_index(0);
    }
    let res = k.resolution();
    let n = k.state.active.len();
    let mut summary = format!("{n} modpack(s) actif(s)\n{} mod(s) chargé(s)", res.mods.len());
    if !res.conflicts.is_empty() {
        summary.push_str(&format!("\n⚠ {} conflit(s) de version", res.conflicts.len()));
    }
    ui.set_active_summary(summary.into());
    ui.set_app_update(k.app_update.as_ref().map(|r| r.version.to_string()).unwrap_or_default().into());
}

fn pack_mod_versions(index: &Index, e: &PackEntry) -> Vec<String> {
    let mut v: Vec<String> =
        index.get(&e.name).map(|l| l.iter().map(|m| m.version.to_string()).collect()).unwrap_or_default();
    if !v.contains(&e.version) {
        v.push(e.version.clone());
    }
    v
}

fn refresh_pack_detail(ui: &AppWindow, k: &Core, reset_meta: bool) {
    let current = k.selected_pack.clone().unwrap_or_default();
    let reset_meta = reset_meta || ui.get_selected_pack().as_str() != current;
    ui.set_selected_pack(current.into());
    let Some(pack) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)) else {
        ui.set_pack_visible(false);
        return;
    };
    ui.set_pack_visible(true);
    if reset_meta {
        ui.set_pack_name(pack.name.clone().into());
        ui.set_pack_description(pack.description.clone().into());
        ui.set_pack_factorio(pack.factorio_version.clone().unwrap_or_default().into());
    }
    ui.set_pack_base_mod(pack.base_mod.clone().unwrap_or_default().into());

    let target = pack.factorio_version.clone().unwrap_or_default();
    let mut warning = String::new();
    if let (false, Some(game)) = (target.is_empty(), &k.game_version) {
        if !same_major_minor(&target, game) {
            warning = format!("Ce modpack cible Factorio {target}, la version installée est {game}.");
        }
    }
    ui.set_pack_factorio_warning(warning.into());

    let filter = ui.get_pack_mod_filter().trim().to_lowercase();
    let mut rows: Vec<(u8, PackModRow)> = pack
        .mods
        .iter()
        .enumerate()
        .filter_map(|(pos, e)| {
            let ver: Version = e.version.parse().unwrap_or_default();
            let info = store::find(&k.index, &e.name, &ver);
            let title = display_title(info.map(|i| i.title.as_str()).unwrap_or(""), &e.name);
            let packs = k.state.packs_using(&e.name, None);
            let author = info.map(|i| i.author.as_str()).unwrap_or("");
            let rank = library_rank(&filter, &e.name, &title, &packs, author)?;
            let versions = pack_mod_versions(&k.index, e);
            let idx = versions.iter().position(|v| v == &e.version).unwrap_or(0);
            let icon = load_image(k.icons.get(&e.name));
            Some((rank, PackModRow {
                // Position dans le modpack (la liste affichée peut être filtrée).
                index: pos as i32,
                name: e.name.clone().into(),
                letter: letter(&title),
                title: title.into(),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
                versions: strings(versions),
                version_index: idx as i32,
                enabled: e.enabled,
                pinned: e.pinned,
                missing: info.is_none() && !k.pending.contains_key(&e.name),
                downloading: k.pending.contains_key(&e.name),
                progress: k.pending.get(&e.name).map(|p| p.progress).unwrap_or(0.0),
            }))
        })
        .collect();
    // Tri stable : sans filtre, l'ordre du modpack est conservé.
    rows.sort_by_key(|(rank, _)| *rank);
    let rows: Vec<PackModRow> = rows.into_iter().map(|(_, r)| r).collect();
    ui.set_pack_mods_total(pack.mods.len() as i32);
    ui.set_pack_mods(model(rows));
    ui.set_pack_undo_label(
        pack.history.last().map(|h| format!("Revenir à l'état d'avant le {}", h.at)).unwrap_or_default().into(),
    );

    let builtins: Vec<BuiltinRow> = k
        .builtins
        .iter()
        .map(|b| BuiltinRow { name: b.clone().into(), enabled: !pack.disabled_builtins.contains(b) })
        .collect();
    ui.set_pack_builtins(model(builtins));

    let saves: Vec<SaveRow> = pack
        .saves
        .iter()
        .map(|p| SaveRow {
            name: p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default().into(),
            path: p.to_string_lossy().to_string().into(),
        })
        .collect();
    ui.set_pack_saves(model(saves));

    let settings = k.paths.pack_settings(&pack.id);
    let has = settings.exists();
    ui.set_pack_has_settings(has);
    let info = if has {
        let size = fs::metadata(&settings).map(|m| m.len()).unwrap_or(0);
        format!("Réglages enregistrés ({size} octets). Ils seront appliqués à l'activation et au lancement.")
    } else {
        "Aucun réglage enregistré : Factorio utilisera les réglages déjà présents.".to_string()
    };
    ui.set_pack_settings_info(info.into());

    let issues = modpack::pack_issues(pack, &k.index, &k.builtins, &k.target_factorio(None), &k.state.found_incompatibilities);
    ui.set_pack_auto_fixable(issues.iter().filter(|i| i.is_automatic()).count() as i32);
    let rows: Vec<IssueRow> = issues
        .into_iter()
        .map(|i| IssueRow {
            text: i.text.into(),
            a: i.disable.first().cloned().unwrap_or_default().into(),
            b: i.disable.get(1).cloned().unwrap_or_default().into(),
        })
        .collect();
    ui.set_pack_issues(model(rows));
}

fn refresh_dl(ui: &AppWindow, k: &Core) {
    let Some(dl) = &k.dl else {
        ui.set_dl_visible(false);
        return;
    };
    ui.set_dl_title(dl.title.clone().into());
    ui.set_dl_name(dl.name.clone().into());
    ui.set_dl_versions(strings(
        dl.releases.iter().map(|r| format!("{}  (Factorio {})", r.version, r.factorio())).collect(),
    ));
    ui.set_dl_version_index(dl.version_index as i32);
    let rows: Vec<DepRow> = dl
        .items
        .iter()
        .map(|i| DepRow {
            name: i.name.clone().into(),
            kind: i.kind.label().into(),
            group: i.kind.group(),
            constraint: i.constraint.as_ref().map(|d| d.constraint_str()).unwrap_or_default().into(),
            status: i.status.clone().into(),
            checked: i.checked,
            checkable: i.checkable,
        })
        .collect();
    let target = k.target_factorio(None);
    let rel = dl.releases.get(dl.version_index);
    let mut info = format!(
        "{} dépendance(s) obligatoire(s), {} recommandée(s) ou optionnelle(s).",
        dl.items.iter().filter(|i| i.kind.is_required()).count(),
        dl.items.iter().filter(|i| i.kind.is_optional_like()).count()
    );
    if let Some(r) = rel {
        if !target.is_empty() && !same_major_minor(&r.factorio(), &target) {
            info.push_str(&format!("  ⚠ Cette version est prévue pour Factorio {} (cible : {target}).", r.factorio()));
        }
    }
    ui.set_dl_info(info.into());
    ui.set_dl_deps(model(rows));
    let mut targets = vec!["Aucun (dépôt uniquement)".to_string()];
    targets.extend(k.state.packs.iter().map(|p| p.name.clone()));
    ui.set_dl_targets(strings(targets));
    ui.set_dl_is_modpack(dl.offer_pack && portal::is_modpack_category(dl.category.as_deref()));
}

fn refresh_settings(ui: &AppWindow, k: &Core) {
    let c = &k.config;
    ui.set_s_user_dir(c.factorio_user_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default().into());
    ui.set_s_exe(c.factorio_exe.as_ref().map(|p| p.display().to_string()).unwrap_or_default().into());
    ui.set_s_username(c.username.clone().into());
    ui.set_s_token(c.token.clone().into());
    ui.set_s_target(c.target_factorio.clone().into());
    ui.set_s_feat_settings(c.feat_mod_settings);
    ui.set_s_feat_version(c.feat_factorio_version);
    ui.set_s_feat_export(c.feat_export);
    ui.set_s_feat_saves(c.feat_saves);
    ui.set_s_auto_updates(c.auto_check_updates);
    ui.set_s_auto_sync(c.auto_sync_on_activate);
    ui.set_s_export_mods(c.export_include_mods);
    ui.set_s_copy_back(c.copy_back_settings);
    ui.set_s_interval(c.check_interval_minutes.to_string().into());
    ui.set_s_auto_app(c.auto_update_app);
    ui.set_s_check_launch(c.check_before_launch);
    ui.set_s_sprite_cache(c.cache_sprite_atlas);
    ui.set_app_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    ui.set_s_data_dir(k.paths.data.display().to_string().into());
    let detected = match &k.game_version {
        Some(v) => format!("Factorio {v} détecté • intégrés : {}", k.builtins.join(", ")),
        None => "Version du jeu non détectée".to_string(),
    };
    ui.set_s_detected(detected.into());
    let creds = if !c.username.is_empty() && !c.token.is_empty() {
        format!("Identifiants manuels : {}", c.username)
    } else {
        match c.factorio_user_dir.as_ref().and_then(|d| factorio::read_credentials(d)) {
            Some((u, _)) => format!("Connecté en tant que « {u} » (lu depuis player-data.json)."),
            None => "Aucun identifiant trouvé dans player-data.json : connecte-toi au portail dans le jeu, ou saisis-les ci-dessous.".into(),
        }
    };
    ui.set_s_credentials(creds.into());
}

// ---------------------------------------------------------------------------
// Opérations de fond
// ---------------------------------------------------------------------------

/// `version` est-elle la plus récente du dépôt pour ce mod ?
fn is_latest(index: &Index, name: &str, version: &Version) -> bool {
    index.get(name).and_then(|v| v.first()).map(|m| m.version <= *version).unwrap_or(true)
}

/// Fait passer les modpacks à la dernière version du dépôt des mods `names`, sauf là où une
/// version a été choisie à la main ou si cette version ne convient pas à la version de Factorio
/// visée par le modpack. Renvoie le nombre de mods mis à jour dans les modpacks.
fn follow_latest(k: &mut Core, names: &HashSet<String>) -> usize {
    let targets: Vec<String> = k.state.packs.iter().map(|p| k.target_factorio(Some(p))).collect();
    let mut n = 0;
    for (pack, target) in k.state.packs.iter_mut().zip(targets) {
        for e in pack.mods.iter_mut().filter(|e| !e.pinned && names.contains(&e.name)) {
            let Some(latest) = k.index.get(&e.name).and_then(|v| v.first()) else { continue };
            let current: Version = e.version.parse().unwrap_or_default();
            if latest.version > current && (target.is_empty() || factorio_compatible(&latest.factorio_version, &target)) {
                e.version = latest.version.to_string();
                n += 1;
            }
        }
    }
    n
}

/// Après une mise à jour : ajoute aux modpacks les dépendances obligatoires apparues (ou met à jour
/// celles dont la version ne convient plus), ainsi que les dépendances optionnelles choisies
/// (`optional` : mod mis à jour -> dépendances à ajouter). Seuls les modpacks qui utilisent la
/// version mise à jour sont concernés. Renvoie (ajoutées, mises à jour).
fn ensure_pack_deps(
    k: &mut Core,
    updated_to: &HashMap<String, Version>,
    optional: &HashMap<String, Vec<String>>,
    scope: Option<&str>,
) -> (usize, usize) {
    let index = k.index.clone();
    let builtins = k.builtins.clone();
    let targets: Vec<String> = k.state.packs.iter().map(|p| k.target_factorio(Some(p))).collect();
    let required = |info: &store::ModInfo| info.deps().into_iter().filter(|d| d.kind.is_required()).collect::<Vec<_>>();
    let (mut added, mut updated) = (0, 0);
    for (pack, target) in k.state.packs.iter_mut().zip(targets) {
        if scope.is_some_and(|id| id != pack.id) {
            continue;
        }
        let mut stack: Vec<Dependency> = Vec::new();
        // Seuls les mods passés à la nouvelle version (pas ceux restés sur une version fixée).
        for e in pack.mods.iter().filter(|e| e.enabled) {
            let v: Version = e.version.parse().unwrap_or_default();
            if updated_to.get(&e.name) != Some(&v) {
                continue;
            }
            let Some(info) = store::find(&index, &e.name, &v) else { continue };
            stack.extend(required(info));
            for o in optional.get(&e.name).into_iter().flatten() {
                stack.extend(info.deps().into_iter().filter(|d| &d.name == o));
            }
        }
        let mut seen = HashSet::new();
        while let Some(dep) = stack.pop() {
            if is_builtin(&dep.name, &builtins) || !seen.insert(dep.name.clone()) {
                continue;
            }
            match pack.mods.iter_mut().find(|e| e.name == dep.name) {
                Some(e) => {
                    let cur: Version = e.version.parse().unwrap_or_default();
                    if dep.matches(&cur) || e.pinned {
                        if let Some(i) = store::find(&index, &dep.name, &cur) {
                            stack.extend(required(i));
                        }
                    } else if let Some(m) = installed_compatible(&index, &dep, &target) {
                        e.version = m.version.to_string();
                        updated += 1;
                        stack.extend(required(m));
                    }
                }
                None => {
                    if let Some(m) = installed_compatible(&index, &dep, &target) {
                        pack.set_mod(&m.name, &m.version);
                        added += 1;
                        stack.extend(required(m));
                    }
                }
            }
        }
    }
    (added, updated)
}

/// Mods de chaque modpack, pour mémoriser l'état d'avant une mise à jour.
fn packs_snapshot(k: &Core) -> Vec<(String, Vec<PackEntry>)> {
    k.state.packs.iter().map(|p| (p.id.clone(), p.mods.clone())).collect()
}

/// Ajoute à l'historique des modpacks modifiés depuis `before` leur état précédent.
fn record_history(k: &mut Core, before: Vec<(String, Vec<PackEntry>)>) -> usize {
    let at = now_label();
    let mut n = 0;
    for (id, mods) in before {
        if let Some(p) = k.state.pack_mut(&id) {
            if p.mods != mods {
                p.push_history(mods, at.clone());
                n += 1;
            }
        }
    }
    n
}

/// Ajoute une version d'un mod au modpack, avec ses dépendances obligatoires déjà présentes
/// dans le dépôt. Renvoie le nombre de dépendances ajoutées.
fn add_mod_to_pack(k: &mut Core, pack_id: &str, info: &store::ModInfo) -> usize {
    let builtins = k.builtins.clone();
    let index = k.index.clone();
    let Some(pack) = k.state.pack_mut(pack_id) else { return 0 };
    pack.set_mod(&info.name, &info.version);
    pack.set_pinned(&info.name, !is_latest(&index, &info.name, &info.version));
    let mut added = 0;
    let mut stack = info.deps();
    let mut seen = HashSet::new();
    while let Some(dep) = stack.pop() {
        if !dep.kind.is_required() || is_builtin(&dep.name, &builtins) || !seen.insert(dep.name.clone()) {
            continue;
        }
        if pack.entry(&dep.name).is_some() {
            continue;
        }
        if let Some(m) = index.get(&dep.name).and_then(|v| v.iter().find(|m| dep.matches(&m.version))) {
            pack.set_mod(&m.name, &m.version);
            stack.extend(m.deps());
            added += 1;
        }
    }
    added
}

/// Résultats de la recherche « ajouter un mod » de la vue modpack : dépôt d'abord, puis portail.
fn pack_add_rows(k: &Core, query: &str) -> Vec<AddRow> {
    let q = query.trim().to_lowercase();
    if q.chars().count() < 2 {
        return vec![];
    }
    let pack = k.selected_pack.as_ref().and_then(|id| k.state.pack(id));
    let in_pack = |n: &str| pack.map(|p| p.entry(n).is_some()).unwrap_or(false);
    let mut local: Vec<(u8, String, AddRow)> = Vec::new();
    for (name, versions) in &k.index {
        if in_pack(name) {
            continue;
        }
        let title = display_title(&versions[0].title, name);
        let packs = k.state.packs_using(name, None);
        let Some(rank) = library_rank(&q, name, &title, &packs, &versions[0].author) else {
            continue;
        };
        local.push((
            rank,
            title.to_lowercase(),
            AddRow {
                name: name.into(),
                title: title.into(),
                info: format!("Dans le dépôt  •  {}", versions[0].version).into(),
                installed: true,
            },
        ));
    }
    local.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut rows: Vec<AddRow> = local.into_iter().take(8).map(|(_, _, r)| r).collect();
    if let Some(list) = &k.portal_list {
        let target = k.target_factorio(pack);
        let filter = (!target.is_empty()).then_some(target.as_str());
        for m in portal::search(list, &q, None, filter, 30) {
            if rows.len() >= 16 {
                break;
            }
            if k.index.contains_key(&m.name) || in_pack(&m.name) {
                continue;
            }
            let ver = m.latest_release.as_ref().map(|r| r.version.clone()).unwrap_or_default();
            rows.push(AddRow {
                name: m.name.clone().into(),
                title: display_title(&m.title, &m.name).into(),
                info: format!("Portail  •  par {}  •  {ver}", one_line(&m.owner)).into(),
                installed: false,
            });
        }
    }
    rows
}

fn rescan(c: &Shared) {
    let store = c.lock().unwrap().store.clone();
    let index = store.scan();
    let mut icons = HashMap::new();
    for (name, versions) in &index {
        if let Some(p) = versions.iter().find_map(|v| store.icon(v)) {
            icons.insert(name.clone(), p);
        }
    }
    let mut k = c.lock().unwrap();
    for (n, p) in k.icons.drain() {
        icons.entry(n).or_insert(p);
    }
    k.icons = icons;
    k.index = index;
    if let Some(sel) = k.selected_mod.clone() {
        if !k.index.contains_key(&sel) {
            k.selected_mod = None;
        }
    }
}

/// Miniatures du portail pour les mods sans thumbnail.png.
fn fetch_missing_icons(c: &Shared) {
    let (portal, names) = {
        let k = c.lock().unwrap();
        let names: Vec<String> = k.index.keys().filter(|n| !k.icons.contains_key(*n)).cloned().collect();
        (k.portal.clone(), names)
    };
    let found: Vec<(String, PathBuf)> = parallel_map(&names, 6, |n| portal.thumbnail(n).map(|p| (n.clone(), p)));
    let mut k = c.lock().unwrap();
    for (n, p) in found {
        k.icons.insert(n, p);
    }
}

fn parallel_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> Option<R> + Sync) -> Vec<R> {
    if items.is_empty() {
        return vec![];
    }
    let chunk = items.len().div_ceil(threads.max(1));
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                s.spawn(move || part.iter().filter_map(f).collect::<Vec<R>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

fn check_updates(c: &Shared) -> Result<String> {
    let (portal, names, target, local) = {
        let k = c.lock().unwrap();
        let names: Vec<String> = k.index.keys().filter(|n| !is_builtin(n, &k.builtins)).cloned().collect();
        let local: HashMap<String, Version> = k.index.iter().map(|(n, v)| (n.clone(), v[0].version)).collect();
        (k.portal.clone(), names, k.target_factorio(None), local)
    };
    let releases = portal.releases_for(&names)?;
    let mut updates = BTreeMap::new();
    for (name, rels) in releases {
        let Some(cur) = local.get(&name) else { continue };
        // Seules les versions chargeables par le jeu installé sont proposées.
        let best = rels.into_iter().filter(|r| factorio_compatible(&r.factorio(), &target)).max_by_key(|r| r.ver());
        if let Some(r) = best.filter(|r| r.ver() > *cur) {
            updates.insert(name, r);
        }
    }
    let n = updates.len();
    c.lock().unwrap().updates = updates;
    Ok(if n == 0 { "Tous les mods sont à jour.".into() } else { format!("{n} mise(s) à jour disponible(s).") })
}

/// Index du portail en mémoire ; `force` le retélécharge, sinon le cache disque est utilisé
/// tant qu'il a moins que l'intervalle de vérification.
fn ensure_portal_list(c: &Shared, force: bool) -> Result<Arc<Vec<ModListEntry>>> {
    let (portal, cached, max_age) = {
        let k = c.lock().unwrap();
        (k.portal.clone(), k.portal_list.clone(), k.config.check_interval())
    };
    if let (Some(l), false) = (cached, force) {
        return Ok(l);
    }
    let list = Arc::new(portal.list_all(if force { Duration::ZERO } else { max_age })?);
    let mut k = c.lock().unwrap();
    k.portal_list = Some(list.clone());
    k.portal_list_at = Some(Instant::now());
    Ok(list)
}

fn portal_list_stale(k: &Core) -> bool {
    k.portal_list_at.map(|t| t.elapsed() > k.config.check_interval()).unwrap_or(true)
}

/// Vérification périodique : mises à jour des mods, index du portail, nouvelle version de l'app.
fn periodic_check(c: &Shared, w: &Weak<AppWindow>) -> Result<String> {
    let (mods_auto, app_auto, stale, before, has_mods) = {
        let mut k = c.lock().unwrap();
        k.checking = true;
        k.last_check = Some(Instant::now());
        (
            k.config.auto_check_updates,
            k.config.auto_update_app,
            portal_list_stale(&k),
            k.updates.len(),
            !k.index.is_empty(),
        )
    };
    let mut msg = String::new();
    if mods_auto && has_mods {
        post_status(w, "Vérification des mises à jour des mods…");
        msg = check_updates(c).unwrap_or_else(|e| format!("Mises à jour : {e:#}"));
    }
    if stale {
        if let Err(e) = ensure_portal_list(c, true) {
            eprintln!("index du portail: {e:#}");
        }
    }
    if app_auto {
        match updater::check() {
            Ok(Some(rel)) => {
                msg = format!("Nouvelle version de l'application disponible : {}", rel.version);
                c.lock().unwrap().app_update = Some(rel);
            }
            Ok(None) => {}
            Err(e) => eprintln!("mise à jour de l'app: {e:#}"),
        }
    }
    let mut k = c.lock().unwrap();
    k.checking = false;
    if k.updates.len() > before {
        msg = format!("{} nouvelle(s) mise(s) à jour de mods disponible(s).", k.updates.len() - before);
    }
    Ok(msg)
}

/// Fermeture transitive des dépendances obligatoires d'une release.
fn required_closure(
    portal: &Portal,
    index: &Index,
    builtins: &[String],
    target: &str,
    root: &str,
    deps: Vec<Dependency>,
    seen: &mut HashSet<String>,
) -> Vec<DlItem> {
    let mut items = Vec::new();
    let mut frontier: Vec<(Dependency, String)> =
        deps.into_iter().filter(|d| d.kind.is_required()).map(|d| (d, root.to_string())).collect();
    while !frontier.is_empty() {
        let to_fetch: Vec<String> = frontier
            .iter()
            .filter(|(d, _)| !is_builtin(&d.name, builtins) && !seen.contains(&d.name))
            .filter(|(d, _)| installed_compatible(index, d, target).is_none())
            .map(|(d, _)| d.name.clone())
            .collect();
        parallel_map(&to_fetch, 8, |n| portal.full(n).ok().map(|_| ()));
        let mut next = Vec::new();
        for (dep, parent) in frontier {
            if is_builtin(&dep.name, builtins) || !seen.insert(dep.name.clone()) {
                continue;
            }
            let installed = installed_compatible(index, &dep, target);
            let mut item = DlItem {
                name: dep.name.clone(),
                kind: dep.kind,
                constraint: Some(dep.clone()),
                release: None,
                installed: None,
                checked: true,
                checkable: true,
                status: String::new(),
            };
            if let Some(info) = installed {
                item.installed = Some(info.version);
                item.status = format!("Déjà dans le dépôt ({}) • requis par {parent}", info.version);
                next.extend(info.deps().into_iter().filter(|d| d.kind.is_required()).map(|d| (d, dep.name.clone())));
            } else {
                match portal.full(&dep.name) {
                    Ok(full) => match full.best_release(target, Some(&dep)) {
                        Some(r) => {
                            item.status =
                                format!("À télécharger : {} (Factorio {}) • requis par {parent}", r.version, r.factorio());
                            next.extend(r.deps().into_iter().filter(|d| d.kind.is_required()).map(|d| (d, dep.name.clone())));
                            item.release = Some(r);
                        }
                        None => {
                            item.checked = false;
                            item.checkable = false;
                            item.status = format!(
                                "⚠ Aucune version pour Factorio {target} (publié pour : {}) • requis par {parent}",
                                full.published_for()
                            );
                        }
                    },
                    Err(e) => {
                        item.checked = false;
                        item.checkable = false;
                        item.status = format!("⚠ Introuvable sur le portail ({e}) • requis par {parent}");
                    }
                }
            }
            items.push(item);
        }
        frontier = next;
    }
    items
}

/// Version du dépôt qui satisfait la contrainte ET peut être chargée par le jeu.
fn installed_compatible<'a>(index: &'a Index, dep: &Dependency, game: &str) -> Option<&'a store::ModInfo> {
    index
        .get(&dep.name)?
        .iter()
        .find(|m| dep.matches(&m.version) && factorio_compatible(&m.factorio_version, game))
}

fn build_items(portal: &Portal, index: &Index, builtins: &[String], target: &str, name: &str, rel: &Release) -> Vec<DlItem> {
    let mut seen = HashSet::from([name.to_string()]);
    let deps = rel.deps();
    let mut items = required_closure(portal, index, builtins, target, name, deps.clone(), &mut seen);
    let optional: Vec<&Dependency> = deps
        .iter()
        .filter(|d| d.kind.is_optional_like() && !is_builtin(&d.name, builtins) && !seen.contains(&d.name))
        .collect();
    let to_fetch: Vec<String> = optional
        .iter()
        .filter(|d| installed_compatible(index, d, target).is_none())
        .map(|d| d.name.clone())
        .collect();
    parallel_map(&to_fetch, 8, |n| portal.full(n).ok().map(|_| ()));
    for dep in optional {
        if !seen.insert(dep.name.clone()) {
            continue;
        }
        let mut item = DlItem {
            name: dep.name.clone(),
            kind: dep.kind,
            constraint: Some(dep.clone()),
            release: None,
            installed: None,
            checked: false,
            checkable: true,
            status: String::new(),
        };
        if let Some(i) = installed_compatible(index, dep, target) {
            item.installed = Some(i.version);
            item.status = format!("Déjà dans le dépôt ({})", i.version);
        } else {
            match portal.full(&dep.name) {
                Ok(full) => match full.best_release(target, Some(dep)) {
                    Some(r) => {
                        item.status = format!(
                            "À télécharger : {} (Factorio {}), avec ses dépendances obligatoires",
                            r.version,
                            r.factorio()
                        );
                        item.release = Some(r);
                    }
                    None => {
                        item.checkable = false;
                        item.status =
                            format!("Aucune version pour Factorio {target} (publié pour : {})", full.published_for());
                    }
                },
                Err(_) => {
                    item.checkable = false;
                    item.status = "Introuvable sur le portail".into();
                }
            }
        }
        items.push(item);
    }
    // Les mods incompatibles (« ! ») ne sont pas proposés : ce ne sont pas des dépendances à ajouter.
    // Les conflits éventuels restent signalés dans les problèmes détectés du modpack.
    items.sort_by_key(|i| (i.kind.group(), i.name.to_lowercase()));
    items
}

fn load_dl_items(c: &Shared, w: &Weak<AppWindow>, version_index: usize) {
    let (portal, index, builtins, target, dl) = {
        let k = c.lock().unwrap();
        let target = k.target_factorio(k.selected_pack.as_ref().and_then(|id| k.state.pack(id)));
        (k.portal.clone(), k.index.clone(), k.builtins.clone(), target, k.dl.clone())
    };
    let Some(dl) = dl else { return };
    let Some(rel) = dl.releases.get(version_index).cloned() else { return };
    let items = build_items(&portal, &index, &builtins, &target, &dl.name, &rel);
    let c2 = c.clone();
    let _ = w.upgrade_in_event_loop(move |ui| {
        let mut k = c2.lock().unwrap();
        if let Some(d) = k.dl.as_mut().filter(|d| d.name == dl.name) {
            d.items = items;
            d.version_index = version_index;
        }
        ui.set_dl_loading(false);
        ui.set_dl_req_all(true);
        ui.set_dl_rec_all(false);
        ui.set_dl_opt_all(false);
        ui.set_dl_hidden_all(false);
        refresh_dl(&ui, &k);
    });
}

/// Modpack proposé par défaut dans la fenêtre de téléchargement / dépendances.
enum DlTarget {
    /// Le modpack sélectionné dans la vue Modpacks (recherche, ajout depuis un pack).
    SelectedPack,
    /// Aucun : uniquement le dépôt (fiche de la bibliothèque).
    NoPack,
    /// Un modpack précis (ouverture depuis une ligne du modpack).
    Pack(String),
}

/// Ouvre la sélection des dépendances d'un mod : dans une fenêtre (`overlay`) ou intégrée à la fiche
/// de la bibliothèque.
fn open_download(ui: &AppWindow, core: &Shared, name: String, preset: Option<String>, target: DlTarget, overlay: bool) {
    let offer_pack = !matches!(target, DlTarget::Pack(_));
    let generation = {
        let mut k = core.lock().unwrap();
        k.dl_gen += 1;
        k.dl = None;
        let id = match target {
            DlTarget::SelectedPack => k.selected_pack.clone(),
            DlTarget::NoPack => None,
            DlTarget::Pack(id) => Some(id),
        };
        let idx = id.and_then(|id| k.state.packs.iter().position(|p| p.id == id));
        ui.set_dl_target_index(idx.map(|i| i as i32 + 1).unwrap_or(0));
        ui.set_dl_installed(k.index.contains_key(&name));
        k.dl_gen
    };
    ui.set_dl_name(name.clone().into());
    ui.set_dl_visible(overlay);
    ui.set_dl_loading(true);
    ui.set_dl_title(name.clone().into());
    ui.set_dl_info("Chargement des informations du portail…".into());
    ui.set_dl_versions(strings(vec![]));
    ui.set_dl_deps(model(vec![]));
    ui.set_dl_is_modpack(false);
    ui.set_dl_make_pack(false);
    let c = core.clone();
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let (portal, target) = {
            let k = c.lock().unwrap();
            (k.portal.clone(), k.target_factorio(k.selected_pack.as_ref().and_then(|id| k.state.pack(id))))
        };
        let full = match portal.full(&name) {
            Ok(f) => f,
            Err(e) => {
                let msg = format!("Erreur : {e:#}");
                let _ = w.upgrade_in_event_loop(move |ui| {
                    ui.set_dl_loading(false);
                    ui.set_dl_info(msg.into());
                });
                return;
            }
        };
        let releases = full.compatible_releases(&target);
        if releases.is_empty() {
            let msg = format!(
                "Aucune version de « {} » n'est compatible avec Factorio {target} (publié pour : {}).",
                display_title(&full.title, &full.name),
                full.published_for()
            );
            let _ = w.upgrade_in_event_loop(move |ui| {
                ui.set_dl_loading(false);
                ui.set_dl_versions(strings(vec![]));
                ui.set_dl_deps(model(vec![]));
                ui.set_dl_info(msg.into());
            });
            return;
        }
        let idx = preset
            .and_then(|v| releases.iter().position(|r| r.version == v))
            .or_else(|| {
                let best = full.best_release(&target, None)?;
                releases.iter().position(|r| r.version == best.version)
            })
            .unwrap_or(0);
        let is_pack = portal::is_modpack_category(full.category.as_deref());
        let title = display_title(&full.title, &full.name);
        {
            let mut k = c.lock().unwrap();
            if k.dl_gen != generation {
                return; // un autre mod a été sélectionné entre-temps
            }
            k.dl = Some(DlSession {
                name: full.name.clone(),
                title: title.clone(),
                category: full.category.clone(),
                releases,
                version_index: idx,
                items: vec![],
                offer_pack,
            });
        }
        let c2 = c.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            let k = c2.lock().unwrap();
            refresh_dl(&ui, &k);
            ui.set_dl_loading(true);
            ui.set_dl_make_pack(is_pack && offer_pack);
            ui.set_dl_pack_name(title.into());
        });
        load_dl_items(&c, &w, idx);
    });
}

/// Télécharge les mods choisis et met à jour le modpack ciblé.
fn confirm_download(c: &Shared, w: &Weak<AppWindow>, target_pack: Option<String>, new_pack: Option<String>) -> Result<String> {
    let (portal, index, builtins, target, dl, creds, downloads, store) = {
        let k = c.lock().unwrap();
        let pack = target_pack.as_ref().and_then(|id| k.state.pack(id));
        (
            k.portal.clone(),
            k.index.clone(),
            k.builtins.clone(),
            k.target_factorio(pack),
            k.dl.clone().ok_or_else(|| anyhow!("aucun téléchargement en cours"))?,
            k.credentials(),
            k.paths.downloads.clone(),
            k.store.clone(),
        )
    };
    let main = dl.releases.get(dl.version_index).cloned().ok_or_else(|| anyhow!("version introuvable"))?;

    // Liste finale : (nom, version, release à télécharger éventuelle)
    let mut wanted: Vec<(String, Version, Option<Release>)> = Vec::new();
    let main_installed = store::find(&index, &dl.name, &main.ver()).is_some();
    wanted.push((dl.name.clone(), main.ver(), if main_installed { None } else { Some(main.clone()) }));
    let mut seen: HashSet<String> = HashSet::from([dl.name.clone()]);
    // Mods sans version compatible avec le jeu installé : non téléchargés, signalés à la fin.
    let mut skipped: Vec<String> = Vec::new();
    if !factorio_compatible(&main.factorio(), &target) {
        bail!("{} {} est prévu pour Factorio {}, pas {target}", dl.name, main.version, main.factorio());
    }
    for item in dl.items.iter().filter(|i| i.checked && i.kind.is_required()) {
        seen.insert(item.name.clone());
        if let Some(v) = item.installed {
            wanted.push((item.name.clone(), v, None));
        } else if let Some(r) = &item.release {
            wanted.push((item.name.clone(), r.ver(), Some(r.clone())));
        }
    }
    for item in dl.items.iter().filter(|i| i.checked && !i.kind.is_required() && i.kind != DepKind::Incompatible) {
        if !seen.insert(item.name.clone()) {
            continue;
        }
        if let Some(v) = item.installed {
            wanted.push((item.name.clone(), v, None));
            continue;
        }
        let rel = match &item.release {
            Some(r) => Some(r.clone()),
            None => portal.full(&item.name).ok().and_then(|f| f.best_release(&target, item.constraint.as_ref())),
        };
        let Some(rel) = rel else {
            skipped.push(item.name.clone());
            continue;
        };
        let extra = required_closure(&portal, &index, &builtins, &target, &item.name, rel.deps(), &mut seen);
        wanted.push((item.name.clone(), rel.ver(), Some(rel)));
        for e in extra {
            if let Some(v) = e.installed {
                wanted.push((e.name, v, None));
            } else if let Some(r) = e.release {
                wanted.push((e.name, r.ver(), Some(r)));
            } else {
                skipped.push(e.name);
            }
        }
    }

    let jobs: Vec<(String, Release)> =
        wanted.iter().filter_map(|(n, _, r)| r.clone().map(|r| (n.clone(), r))).collect();
    let creds = if jobs.is_empty() {
        None
    } else {
        Some(creds.ok_or_else(|| {
            anyhow!("identifiants du portail introuvables : connecte-toi au portail dans Factorio ou renseigne-les dans Paramètres")
        })?)
    };

    // 1. Le modpack (nouveau ou existant) reçoit tout de suite la liste complète, et les mods à
    //    télécharger apparaissent grisés dans les listes.
    let mut msg = String::new();
    {
        let mut k = c.lock().unwrap();
        let pack_id = if let Some(name) = new_pack {
            let mut p = Modpack::new(&name);
            p.base_mod = Some(dl.name.clone());
            if k.config.feat_factorio_version {
                let f = main.factorio();
                p.factorio_version = if f.is_empty() { None } else { Some(f) };
            }
            let id = p.id.clone();
            k.state.packs.push(p);
            msg.push_str(&format!(", modpack « {name} » créé"));
            Some(id)
        } else {
            target_pack
        };
        if let Some(id) = pack_id {
            if let Some(p) = k.state.pack_mut(&id) {
                for (n, v, _) in &wanted {
                    p.set_mod(n, v);
                }
                // Version plus ancienne que la meilleure disponible : choix manuel, figé.
                let best = dl.releases.iter().filter(|r| factorio_compatible(&r.factorio(), &target)).map(|r| r.ver()).max();
                p.set_pinned(&dl.name, best.is_some_and(|b| main.ver() < b));
                if msg.is_empty() {
                    msg.push_str(&format!(", ajouté(s) à « {} »", p.name));
                }
            }
            k.selected_pack = Some(id);
        }
        for (name, rel) in &jobs {
            let title = if *name == dl.name { dl.title.clone() } else { name.clone() };
            k.pending.insert(name.clone(), Pending { version: rel.version.clone(), title, progress: 0.0 });
        }
        k.dl = None;
        k.save_state();
    }
    post_refresh(w, c);

    // 2. Téléchargements en parallèle ; chaque mod rejoint le dépôt dès qu'il est prêt.
    let failed = download_jobs(c, w, &portal, &store, &downloads, creds, jobs.clone());

    let mut k = c.lock().unwrap();
    let ok = jobs.len() - failed.len();
    let mut out = format!("{ok} mod(s) téléchargé(s){msg}");
    if !failed.is_empty() {
        out.push_str(&format!(" — échecs : {}", failed.join(", ")));
    }
    if !skipped.is_empty() {
        out.push_str(&format!(" ({} ignoré(s), sans version pour Factorio {target} : {})", skipped.len(), skipped.join(", ")));
    }
    let core = &mut *k;
    let index = &core.index;
    core.updates.retain(|name, r| index.get(name).map(|v| r.ver() > v[0].version).unwrap_or(true));
    let names: HashSet<String> =
        jobs.iter().map(|(n, _)| n.clone()).filter(|n| !failed.iter().any(|f| f.starts_with(&format!("{n} (")))).collect();
    let before = packs_snapshot(&k);
    let followed = follow_latest(&mut k, &names);
    let latest: HashMap<String, Version> =
        names.iter().filter_map(|n| k.index.get(n).map(|v| (n.clone(), v[0].version))).collect();
    let (added, updated) = ensure_pack_deps(&mut k, &latest, &HashMap::new(), None);
    record_history(&mut k, before);
    if followed > 0 {
        out.push_str(&format!(", {followed} mise(s) à jour dans les modpacks"));
    }
    if added + updated > 0 {
        out.push_str(&format!(" (dépendances : {added} ajoutée(s), {updated} mise(s) à jour)"));
    }
    k.save_state();
    Ok(out + ".")
}

/// Nombre de téléchargements simultanés.
const PARALLEL_DOWNLOADS: usize = 4;

/// Télécharge les releases en parallèle, ajoute chaque zip au dépôt et met à jour les listes au fur
/// et à mesure (progression par mod). Renvoie les mods en échec.
fn download_jobs(
    c: &Shared,
    w: &Weak<AppWindow>,
    portal: &Portal,
    store: &Store,
    downloads: &Path,
    creds: Option<(String, String)>,
    jobs: Vec<(String, Release)>,
) -> Vec<String> {
    let Some((user, token)) = creds else { return vec![] };
    let total = jobs.len();
    let queue = Mutex::new(jobs.into_iter());
    let done = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..PARALLEL_DOWNLOADS.min(total) {
            s.spawn(|| loop {
                let Some((name, rel)) = queue.lock().unwrap().next() else { break };
                let mut last = Instant::now();
                let result = portal
                    .download(&rel, &user, &token, downloads, |got, size| {
                        if size > 0 && last.elapsed() >= Duration::from_millis(150) {
                            last = Instant::now();
                            post_mod_progress(w, c, &name, got as f32 / size as f32);
                        }
                    })
                    .and_then(|path| store.add_file(&path, AddMode::Move));
                let icon = result.as_ref().ok().and_then(|info| store.icon(info));
                {
                    let mut k = c.lock().unwrap();
                    k.pending.remove(&name);
                    match result {
                        Ok(info) => {
                            journal::log(format!("Téléchargé : {} {}", info.name, info.version));
                            if let Some(p) = icon {
                                k.icons.insert(info.name.clone(), p);
                            }
                            let list = k.index.entry(info.name.clone()).or_default();
                            list.retain(|m| m.version != info.version);
                            list.push(info);
                            list.sort_by(|a, b| b.version.cmp(&a.version));
                        }
                        Err(e) => {
                            journal::log(format!("Échec du téléchargement de {name} {} : {e:#}", rel.version));
                            failed.lock().unwrap().push(format!("{name} ({e})"));
                        }
                    }
                }
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                post_status(w, format!("Téléchargement : {n}/{total} terminé(s)"));
                post_progress(w, n as f32 / total as f32);
                post_refresh(w, c);
            });
        }
    });
    failed.into_inner().unwrap()
}

/// Met à jour la barre de progression d'un mod dans les listes, sans reconstruire les modèles.
fn post_mod_progress(w: &Weak<AppWindow>, c: &Shared, name: &str, progress: f32) {
    let (c, name) = (c.clone(), name.to_string());
    let _ = w.upgrade_in_event_loop(move |ui| {
        if let Some(p) = c.lock().unwrap().pending.get_mut(&name) {
            p.progress = progress;
        }
        let lib = ui.get_library();
        for i in 0..lib.row_count() {
            if let Some(mut r) = lib.row_data(i).filter(|r| r.name.as_str() == name) {
                r.progress = progress;
                lib.set_row_data(i, r);
            }
        }
        let mods = ui.get_pack_mods();
        for i in 0..mods.row_count() {
            if let Some(mut r) = mods.row_data(i).filter(|r| r.name.as_str() == name) {
                r.progress = progress;
                mods.set_row_data(i, r);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Mises à jour (tous les mods, ou un modpack) avec changelog et nouvelles dépendances
// ---------------------------------------------------------------------------

fn refresh_upd(ui: &AppWindow, k: &Core) {
    let Some(u) = &k.upd else {
        ui.set_upd_visible(false);
        return;
    };
    let rows: Vec<UpdRow> = u
        .items
        .iter()
        .map(|i| UpdRow {
            name: i.name.clone().into(),
            title: i.title.clone().into(),
            from: i.from.to_string().into(),
            to: i.to.to_string().into(),
            source: if i.release.is_some() { "à télécharger" } else { "déjà dans le dépôt" }.into(),
            changelog: i.changelog.clone().into(),
            checked: i.checked,
        })
        .collect();
    let opts: Vec<UpdOptRow> = u
        .opts
        .iter()
        .map(|o| UpdOptRow {
            name: o.name.clone().into(),
            info: format!(
                "{} • ajoutée par {}",
                o.status,
                o.for_mods.join(", ")
            )
            .into(),
            required: o.required,
            checked: o.checked,
            checkable: o.checkable,
        })
        .collect();
    let n = u.items.iter().filter(|i| i.checked).count();
    ui.set_upd_rows(model(rows));
    ui.set_upd_opts(model(opts));
    ui.set_upd_count(n as i32);
}

/// Ouvre la fenêtre des mises à jour : tous les mods (`pack` = None) ou les mods d'un modpack.
fn open_updates(ui: &AppWindow, core: &Shared, only: Option<Vec<String>>, pack: Option<String>) {
    let (items, portal) = {
        let k = core.lock().unwrap();
        let mut items: Vec<UpdItem> = Vec::new();
        match &pack {
            None => {
                for (name, rel) in &k.updates {
                    if only.as_ref().is_some_and(|o| !o.contains(name)) {
                        continue;
                    }
                    let Some(local) = k.index.get(name) else { continue };
                    items.push(UpdItem {
                        name: name.clone(),
                        title: display_title(&local[0].title, name),
                        from: local[0].version,
                        to: rel.ver(),
                        release: Some(rel.clone()),
                        changelog: String::new(),
                        checked: true,
                    });
                }
            }
            Some(id) => {
                let Some(p) = k.state.pack(id) else { return };
                let target = k.target_factorio(Some(p));
                for e in p.mods.iter().filter(|e| !e.pinned) {
                    let cur: Version = e.version.parse().unwrap_or_default();
                    let repo = k
                        .index
                        .get(&e.name)
                        .and_then(|v| v.iter().find(|m| factorio_compatible(&m.factorio_version, &target)));
                    let portal_rel = k.updates.get(&e.name).filter(|r| factorio_compatible(&r.factorio(), &target));
                    let (to, release) = match (repo, portal_rel) {
                        (_, Some(r)) if repo.is_none_or(|m| r.ver() > m.version) => (r.ver(), Some(r.clone())),
                        (Some(m), _) => (m.version, None),
                        _ => continue,
                    };
                    if to <= cur {
                        continue;
                    }
                    let title = k.index.get(&e.name).map(|v| display_title(&v[0].title, &e.name)).unwrap_or(e.name.clone());
                    items.push(UpdItem {
                        name: e.name.clone(),
                        title,
                        from: cur,
                        to,
                        release,
                        changelog: String::new(),
                        checked: true,
                    });
                }
            }
        }
        (items, k.portal.clone())
    };
    let pack_name = pack.as_ref().and_then(|id| core.lock().unwrap().state.pack(id).map(|p| p.name.clone()));
    if items.is_empty() {
        ui.set_status(match &pack_name {
            Some(n) => format!("« {n} » est déjà à jour (les versions fixées à la main ne sont pas proposées)."),
            None => "Aucune mise à jour à installer : lance d'abord une vérification.".to_string(),
        }
        .into());
        return;
    }
    ui.set_upd_title(
        match &pack_name {
            Some(n) => format!("Mettre à jour « {n} »"),
            None => format!("Mises à jour des mods ({})", items.len()),
        }
        .into(),
    );
    ui.set_upd_loading(true);
    ui.set_upd_visible(true);
    {
        let mut k = core.lock().unwrap();
        k.upd = Some(UpdSession { items: items.clone(), opts: vec![], pack: pack.clone() });
        refresh_upd(ui, &k);
    }
    let (c, w) = (core.clone(), ui.as_weak());
    std::thread::spawn(move || {
        let (index, builtins, target) = {
            let k = c.lock().unwrap();
            let p = pack.as_ref().and_then(|id| k.state.pack(id));
            (k.index.clone(), k.builtins.clone(), k.target_factorio(p))
        };
        let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
        let fulls: HashMap<String, portal::ModFull> =
            parallel_map(&names, 8, |n| portal.full(n).ok().map(|f| (n.clone(), f))).into_iter().collect();
        let mut items = items;
        let mut opts: Vec<UpdOpt> = Vec::new();
        for it in items.iter_mut() {
            let full = fulls.get(&it.name);
            it.changelog = match full.and_then(|f| f.changelog.as_deref()) {
                Some(text) => {
                    let log = portal::changelog_since(text, &it.from);
                    if log.is_empty() { "Le changelog ne décrit pas ces versions.".into() } else { short_block(&log, 3000) }
                }
                None if full.is_some() => "Pas de changelog sur le portail.".into(),
                None => "Changelog indisponible (portail injoignable).".into(),
            };
            // Dépendances de la nouvelle version absentes de l'ancienne.
            let new_deps: Vec<Dependency> = match &it.release {
                Some(r) => r.deps(),
                None => store::find(&index, &it.name, &it.to).map(|m| m.deps()).unwrap_or_default(),
            };
            let old: HashSet<String> = store::find(&index, &it.name, &it.from)
                .map(|m| m.deps().into_iter().map(|d| d.name).collect())
                .unwrap_or_default();
            for d in new_deps {
                if is_builtin(&d.name, &builtins) || old.contains(&d.name) || d.kind == DepKind::Incompatible {
                    continue;
                }
                if !d.kind.is_required() && !d.kind.is_optional_like() {
                    continue;
                }
                if let Some(o) = opts.iter_mut().find(|o| o.name == d.name) {
                    o.for_mods.push(it.title.clone());
                    o.required |= d.kind.is_required();
                    continue;
                }
                let mut o = UpdOpt {
                    name: d.name.clone(),
                    for_mods: vec![it.title.clone()],
                    required: d.kind.is_required(),
                    release: None,
                    installed: None,
                    checked: d.kind.is_required(),
                    checkable: !d.kind.is_required(),
                    status: String::new(),
                };
                if let Some(m) = installed_compatible(&index, &d, &target) {
                    o.installed = Some(m.version);
                    o.status = format!("{} {}, déjà dans le dépôt", d.kind.label(), m.version);
                } else {
                    match portal.full(&d.name).ok().and_then(|f| f.best_release(&target, Some(&d))) {
                        Some(r) => {
                            o.status = format!("{}, à télécharger : {}", d.kind.label(), r.version);
                            o.release = Some(r);
                        }
                        None => {
                            o.checked = false;
                            o.checkable = false;
                            o.status = format!("⚠ {} sans version pour Factorio {target}", d.kind.label());
                        }
                    }
                }
                opts.push(o);
            }
        }
        opts.sort_by_key(|o| (!o.required, o.name.to_lowercase()));
        let _ = w.upgrade_in_event_loop(move |ui| {
            let mut k = c.lock().unwrap();
            if let Some(u) = k.upd.as_mut() {
                // Conserve les cases décochées entre-temps.
                for it in items.iter_mut() {
                    if let Some(old) = u.items.iter().find(|o| o.name == it.name) {
                        it.checked = old.checked;
                    }
                }
                u.items = items;
                u.opts = opts;
            }
            ui.set_upd_loading(false);
            refresh_upd(&ui, &k);
        });
    });
}

/// Texte tronqué à `max` caractères (fin de ligne conservée).
fn short_block(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}\n…", s.chars().take(max).collect::<String>())
}

/// Installe les mises à jour cochées : téléchargements (avec les nouvelles dépendances), puis
/// passage des modpacks aux nouvelles versions. L'état précédent de chaque modpack modifié est
/// mémorisé pour pouvoir y revenir.
fn run_updates(c: &Shared, w: &Weak<AppWindow>, session: UpdSession) -> Result<String> {
    let (portal, index, builtins, target, creds, downloads, store) = {
        let k = c.lock().unwrap();
        let p = session.pack.as_ref().and_then(|id| k.state.pack(id));
        (
            k.portal.clone(),
            k.index.clone(),
            k.builtins.clone(),
            k.target_factorio(p),
            k.credentials(),
            k.paths.downloads.clone(),
            k.store.clone(),
        )
    };
    let items: Vec<&UpdItem> = session.items.iter().filter(|i| i.checked).collect();
    if items.is_empty() {
        return Ok("Aucune mise à jour sélectionnée.".into());
    }
    let mut seen: HashSet<String> = items.iter().map(|i| i.name.clone()).collect();
    let mut jobs: Vec<(String, Release)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut add_closure = |name: &str, deps: Vec<Dependency>, jobs: &mut Vec<(String, Release)>, seen: &mut HashSet<String>| {
        for e in required_closure(&portal, &index, &builtins, &target, name, deps, seen) {
            match (e.release, e.installed) {
                (Some(r), _) => jobs.push((e.name, r)),
                (None, Some(_)) => {}
                (None, None) => skipped.push(e.name),
            }
        }
    };
    for it in &items {
        let deps = match &it.release {
            Some(r) => {
                jobs.push((it.name.clone(), r.clone()));
                r.deps()
            }
            None => store::find(&index, &it.name, &it.to).map(|m| m.deps()).unwrap_or_default(),
        };
        add_closure(&it.name, deps, &mut jobs, &mut seen);
    }
    let mut optional: HashMap<String, Vec<String>> = HashMap::new();
    for o in session.opts.iter().filter(|o| o.checked && !o.required) {
        for it in items.iter().filter(|i| o.for_mods.contains(&i.title)) {
            optional.entry(it.name.clone()).or_default().push(o.name.clone());
        }
        if o.installed.is_none() && seen.insert(o.name.clone()) {
            if let Some(r) = &o.release {
                jobs.push((o.name.clone(), r.clone()));
                add_closure(&o.name, r.deps(), &mut jobs, &mut seen);
            }
        }
    }
    let creds = if jobs.is_empty() {
        None
    } else {
        Some(creds.ok_or_else(|| {
            anyhow!("identifiants du portail introuvables : connecte-toi au portail dans Factorio ou renseigne-les dans Paramètres")
        })?)
    };
    {
        let mut k = c.lock().unwrap();
        for (name, rel) in &jobs {
            let title = k.index.get(name).map(|v| display_title(&v[0].title, name)).unwrap_or(name.clone());
            k.pending.insert(name.clone(), Pending { version: rel.version.clone(), title, progress: 0.0 });
        }
    }
    post_refresh(w, c);
    let failed = download_jobs(c, w, &portal, &store, &downloads, creds, jobs.clone());

    let mut k = c.lock().unwrap();
    let failed_names: HashSet<String> =
        jobs.iter().map(|(n, _)| n.clone()).filter(|n| failed.iter().any(|f| f.starts_with(&format!("{n} (")))).collect();
    let names: HashSet<String> = items.iter().map(|i| i.name.clone()).filter(|n| !failed_names.contains(n)).collect();
    let before = packs_snapshot(&k);
    let moved = match &session.pack {
        None => follow_latest(&mut k, &names),
        Some(id) => {
            let index = k.index.clone();
            let mut n = 0;
            if let Some(p) = k.state.pack_mut(id) {
                for it in items.iter().filter(|i| names.contains(&i.name)) {
                    if store::find(&index, &it.name, &it.to).is_some() {
                        if let Some(e) = p.mods.iter_mut().find(|e| e.name == it.name) {
                            e.version = it.to.to_string();
                            n += 1;
                        }
                    }
                }
            }
            n
        }
    };
    let updated_to: HashMap<String, Version> =
        items.iter().filter(|i| names.contains(&i.name)).map(|i| (i.name.clone(), i.to)).collect();
    let (added, updated) = ensure_pack_deps(&mut k, &updated_to, &optional, session.pack.as_deref());
    let changed_packs = record_history(&mut k, before);
    let core = &mut *k;
    let index = &core.index;
    core.updates.retain(|name, r| index.get(name).map(|v| r.ver() > v[0].version).unwrap_or(true));
    k.save_state();
    let mut out = format!("{} mod(s) mis à jour", names.len());
    if moved > 0 {
        out.push_str(&format!(", {moved} changement(s) de version dans {changed_packs} modpack(s)"));
    }
    if added + updated > 0 {
        out.push_str(&format!(", dépendances : {added} ajoutée(s), {updated} mise(s) à jour"));
    }
    if !failed.is_empty() {
        out.push_str(&format!(" — échecs : {}", failed.join(", ")));
    }
    if !skipped.is_empty() {
        out.push_str(&format!(" — sans version pour Factorio {target} : {}", skipped.join(", ")));
    }
    journal::log(&out);
    Ok(out + ".")
}

/// Anciennes versions qu'aucun modpack n'utilise (ni son historique, ni un choix de conflit).
/// La version la plus récente de chaque mod est toujours conservée.
fn cleanable_versions(k: &Core) -> Vec<store::ModInfo> {
    let mut used: HashSet<(String, String)> = HashSet::new();
    for p in &k.state.packs {
        for e in p.mods.iter().chain(p.history.iter().flat_map(|h| h.mods.iter())) {
            used.insert((e.name.clone(), e.version.parse::<Version>().map(|v| v.to_string()).unwrap_or(e.version.clone())));
        }
    }
    for (n, v) in k.state.conflict_choices.iter().chain(k.session_choices.iter()) {
        used.insert((n.clone(), v.clone()));
    }
    k.index
        .values()
        .flat_map(|versions| versions.iter().skip(1))
        .filter(|m| !used.contains(&(m.name.clone(), m.version.to_string())))
        .cloned()
        .collect()
}

/// Fichier déposé sur la fenêtre : zip de mod (copié dans le dépôt) ou modpack `.fmpack`.
fn handle_drop(ui: &AppWindow, core: &Shared, path: PathBuf) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let name = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    match ext.as_str() {
        "zip" => task(ui, core, &format!("Ajout de {name} au dépôt…"), move |c, _| {
            let store = c.lock().unwrap().store.clone();
            let info = store.add_file(&path, AddMode::LinkOrCopy)?;
            {
                let mut k = c.lock().unwrap();
                if let Some(p) = store.icon(&info) {
                    k.icons.insert(info.name.clone(), p);
                }
                let list = k.index.entry(info.name.clone()).or_default();
                list.retain(|m| m.version != info.version);
                list.push(info.clone());
                list.sort_by(|a, b| b.version.cmp(&a.version));
            }
            fetch_missing_icons(c);
            Ok(format!("{} {} ajouté au dépôt.", info.name, info.version))
        }),
        "fmpack" => task(ui, core, &format!("Import du modpack {name}…"), move |c, w| import_pack(c, w, &path)),
        _ => ui.set_status(format!("{name} ignoré : dépose un zip de mod ou un fichier .fmpack.").into()),
    }
}

fn import_existing(c: &Shared, w: &Weak<AppWindow>) -> Result<String> {
    let (mods_dir, store, builtins) = {
        let k = c.lock().unwrap();
        (k.config.mods_dir().ok_or_else(|| anyhow!("dossier Factorio non configuré"))?, k.store.clone(), k.builtins.clone())
    };
    let zips: Vec<PathBuf> = fs::read_dir(&mods_dir)
        .with_context(|| format!("lecture de {}", mods_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();
    let n = zips.len();
    let mut imported: Vec<store::ModInfo> = Vec::new();
    let mut errors = 0;
    for (i, z) in zips.iter().enumerate() {
        post_progress(w, i as f32 / n.max(1) as f32);
        match store.add_file(z, AddMode::LinkOrCopy) {
            Ok(info) => imported.push(info),
            Err(e) => {
                errors += 1;
                eprintln!("import: {e:#}");
            }
        }
    }
    let list = ModList::read(&mods_dir);
    let mut pack = Modpack::new("Mods actuels (import)");
    pack.description = "Mods activés dans Factorio au moment de l'import".into();
    for b in &builtins {
        if list.mods.iter().any(|m| &m.name == b && !m.enabled) {
            pack.disabled_builtins.push(b.clone());
        }
    }
    let mut best: BTreeMap<String, Version> = BTreeMap::new();
    for info in &imported {
        let enabled = list.mods.is_empty() || list.is_enabled(&info.name);
        if !enabled {
            continue;
        }
        let pinned = list
            .mods
            .iter()
            .find(|m| m.name == info.name)
            .and_then(|m| m.version.as_ref())
            .and_then(|v| v.parse::<Version>().ok());
        let e = best.entry(info.name.clone()).or_insert(info.version);
        match pinned {
            Some(p) => *e = p,
            None if info.version > *e => *e = info.version,
            _ => {}
        }
    }
    for (name, v) in &best {
        pack.set_mod(name, v);
    }
    rescan(c);
    let mut k = c.lock().unwrap();
    let pack_name = pack.name.clone();
    let id = pack.id.clone();
    k.state.packs.push(pack);
    if k.state.active.is_empty() {
        k.state.active.push(id.clone());
    }
    k.selected_pack = Some(id);
    k.save_state();
    Ok(format!(
        "{} zip(s) importé(s) dans le dépôt{}, modpack « {pack_name} » créé avec {} mod(s) actif(s).",
        imported.len(),
        if errors > 0 { format!(" ({errors} erreur(s))") } else { String::new() },
        best.len()
    ))
}

// ---------------------------------------------------------------------------
// Activation, conflits, synchro, lancement
// ---------------------------------------------------------------------------

fn show_conflicts(ui: &AppWindow, k: &mut Core, conflicts: Vec<Conflict>, after: After) {
    let rows: Vec<ConflictRow> = conflicts
        .iter()
        .map(|c| ConflictRow {
            name: c.name.clone().into(),
            labels: strings(c.options.iter().map(|o| format!("{}  —  {}", o.version, o.packs.join(", "))).collect()),
            selected: 0,
        })
        .collect();
    k.conflict_sel = vec![0; conflicts.len()];
    k.conflicts = conflicts;
    k.after_conflict = after;
    ui.set_conflicts(model(rows));
    ui.set_conflict_visible(true);
}

/// Vérifie les conflits puis exécute l'action demandée.
fn proceed(ui: &AppWindow, core: &Shared, after: After) {
    let mut k = core.lock().unwrap();
    let res = k.resolution();
    if !res.conflicts.is_empty() {
        show_conflicts(ui, &mut k, res.conflicts, after);
        return;
    }
    drop(k);
    match after {
        After::Nothing => {}
        After::Sync => start_sync(ui, core),
        After::Launch(save) => start_launch(ui, core, save),
    }
}

fn start_sync(ui: &AppWindow, core: &Shared) {
    let mut k = core.lock().unwrap();
    let Some(mods_dir) = k.config.mods_dir() else {
        ui.set_status("Configure le dossier de Factorio dans Paramètres.".into());
        return;
    };
    let res = k.resolution();
    let plan = sync::plan(&mods_dir, &res, &k.index, &k.builtins);
    let settings = k.settings_source().map(|(_, p)| p);
    let mut text = plan.summary();
    if let Some((id, _)) = k.settings_source() {
        let name = k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default();
        text.push_str(&format!("\nRéglages des mods : ceux du modpack « {name} ».\n"));
    }
    text.push_str(&format!("\nDossier : {}", mods_dir.display()));
    ui.set_confirm_title("Synchroniser le dossier mods/ de Factorio ?".into());
    ui.set_confirm_text(text.into());
    ui.set_confirm_ok("Synchroniser".into());
    ui.set_confirm_visible(true);
    k.confirm = Some(Confirm::Sync(plan, settings));
}

/// Lancement demandé : vérifie d'abord le chargement des mods (sauf si cet ensemble a déjà réussi).
fn start_launch(ui: &AppWindow, core: &Shared, save: Option<PathBuf>) {
    let mut k = core.lock().unwrap();
    if !k.config.check_before_launch || k.config.factorio_exe.is_none() {
        drop(k);
        return do_launch(ui, core, save, None);
    }
    let res = k.resolution();
    let fp = k.launch_fingerprint(&res);
    if k.check_dirs.is_known_ok(&fp) {
        drop(k);
        return do_launch(ui, core, save, Some(fp));
    }
    if let Some(f) = k.failure.as_mut().filter(|f| f.fp == fp && !f.runtime) {
        f.save = save;
        refresh_failure(ui, &k);
        ui.set_fail_visible(true);
        return;
    }
    if let Some(run) = k.check.as_mut() {
        if run.fp == fp && !run.bisecting {
            run.launch_after = Some(save);
            ui.set_status("Vérification des mods en cours : le jeu sera lancé dès qu'elle aura réussi.".into());
            return;
        }
        run.cancel.store(true, Ordering::SeqCst);
        k.check = None;
    }
    drop(k);
    start_check(ui, core, Some(save), true);
}

fn post_check_progress(w: &Weak<AppWindow>, p: f32, label: String) {
    let _ = w.upgrade_in_event_loop(move |ui| {
        ui.set_check_progress(p);
        ui.set_check_label(label.into());
    });
}

fn enabled_count(res: &Resolution, builtins: &[String]) -> usize {
    res.mods.len() + builtins.iter().filter(|b| !res.disabled_builtins.contains(*b)).count()
}

/// Vérifie l'ensemble de mods actif dans l'instance isolée. `launch_after` : lancer ensuite si OK.
fn start_check(ui: &AppWindow, core: &Shared, launch_after: Option<Option<PathBuf>>, manual: bool) {
    let mut k = core.lock().unwrap();
    let Some(exe) = k.config.factorio_exe.clone() else { return };
    let res = k.resolution();
    let fp = k.launch_fingerprint(&res);
    let settings = k.effective_settings();
    let (known, titles) = k.known_and_titles(&res);
    k.check_gen += 1;
    let id = k.check_gen;
    let cancel = Arc::new(AtomicBool::new(false));
    let manual = manual || launch_after.is_some();
    k.check = Some(CheckRun { id, fp, cancel: cancel.clone(), launch_after, bisecting: false, manual });
    let (index, builtins, dirs) = (k.index.clone(), k.builtins.clone(), k.check_dirs.clone());
    drop(k);
    ui.set_check_running(true);
    ui.set_check_progress(0.0);
    ui.set_check_label("Préparation…".into());
    if manual {
        ui.set_status("Vérification du chargement des mods (sans les sprites)…".into());
    }
    let (c, w) = (core.clone(), ui.as_weak());
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = (|| -> Result<Option<check::Outcome>> {
            dirs.prepare()?;
            sync::build_profile(&dirs.mods(), &res, &index, &builtins, settings.as_deref())?;
            if settings.is_none() {
                let _ = fs::remove_file(dirs.mods().join("mod-settings.dat"));
            }
            let out = check::run(&exe, &dirs, enabled_count(&res, &builtins), &known, &titles, &cancel, |p, label| {
                post_check_progress(&w, p, label.to_string())
            })?;
            Ok(out.map(|o| check::judge(&dirs, &known, o)))
        })();
        let secs = started.elapsed().as_secs();
        let _ = w.upgrade_in_event_loop(move |ui| finish_check(&ui, &c, id, outcome, secs));
    });
}

fn finish_check(ui: &AppWindow, core: &Shared, id: u64, outcome: Result<Option<check::Outcome>>, secs: u64) {
    let mut k = core.lock().unwrap();
    if k.check.as_ref().map(|r| r.id) != Some(id) {
        return;
    }
    let run = k.check.take().unwrap();
    ui.set_check_running(false);
    match outcome {
        Ok(None) => {}
        Ok(Some(check::Outcome::Ok(missing))) => {
            k.check_dirs.remember_ok(&run.fp);
            // Confirmés comme sans conséquence si le vrai jeu se charge avec cet ensemble.
            k.pending_missing.insert(run.fp.clone(), check::missing_paths(&missing));
            if k.failure.as_ref().is_some_and(|f| !f.runtime) {
                k.failure = None;
            }
            refresh_check(ui, &mut k);
            if let Some(save) = run.launch_after {
                drop(k);
                do_launch(ui, core, save, Some(run.fp));
            } else {
                ui.set_status(format!("Mods vérifiés en {secs} s : aucun problème, le prochain lancement sera direct.").into());
            }
        }
        Ok(Some(check::Outcome::Failed(failure))) => {
            let launch = run.launch_after.is_some();
            k.failure = Some(LastFailure {
                fp: run.fp,
                failure,
                runtime: false,
                note: String::new(),
                save: run.launch_after.flatten(),
            });
            refresh_failure(ui, &k);
            refresh_check(ui, &mut k);
            if launch || run.manual {
                ui.set_fail_visible(true);
                ui.set_status(format!("Lancement annulé : le chargement des mods échoue (détecté en {secs} s).").into());
            } else {
                ui.set_status(
                    format!("⚠ Le chargement des mods échoue (détecté en {secs} s) : voir « Erreur de chargement ».").into(),
                );
            }
        }
        Err(e) => {
            ui.set_status(format!("Vérification impossible : {e:#}").into());
            if let Some(save) = run.launch_after {
                drop(k);
                do_launch(ui, core, save, None);
            }
        }
    }
}

/// Recherche par dichotomie du mod qui fait échouer le premier mod cité dans l'erreur.
fn start_bisect(ui: &AppWindow, core: &Shared) {
    let mut k = core.lock().unwrap();
    let Some(f) = k.failure.clone() else { return };
    let Some(culprit) = f.failure.culprits.first().cloned() else { return };
    let Some(exe) = k.config.factorio_exe.clone() else { return };
    let res = k.resolution();
    if k.launch_fingerprint(&res) != f.fp {
        ui.set_status("La liste de mods a changé depuis l'erreur : relance d'abord la vérification.".into());
        return;
    }
    if let Some(run) = k.check.take() {
        run.cancel.store(true, Ordering::SeqCst);
    }
    k.check_gen += 1;
    let id = k.check_gen;
    let cancel = Arc::new(AtomicBool::new(false));
    k.check = Some(CheckRun { id, fp: f.fp.clone(), cancel: cancel.clone(), launch_after: None, bisecting: true, manual: true });
    let settings = k.effective_settings();
    let (known, titles) = k.known_and_titles(&res);
    let (index, builtins, dirs) = (k.index.clone(), k.builtins.clone(), k.check_dirs.clone());
    drop(k);
    ui.set_fail_visible(false);
    ui.set_check_running(true);
    ui.set_check_progress(0.0);
    ui.set_check_label("Recherche du conflit…".into());
    ui.set_status(format!("Recherche du mod en conflit avec {culprit} (plusieurs vérifications successives)…").into());
    let (c, w) = (core.clone(), ui.as_weak());
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = check::bisect(&culprit, &res, &index, &builtins, |set, step, planned| {
            let sub = check::subset(&res, set);
            dirs.prepare()?;
            sync::build_profile(&dirs.mods(), &sub, &index, &builtins, settings.as_deref())?;
            let prefix = format!("Essai {step}/~{planned} ({} mods)", sub.mods.len());
            post_check_progress(&w, 0.0, prefix.clone());
            let out = check::run(&exe, &dirs, enabled_count(&sub, &builtins), &known, &titles, &cancel, |p, label| {
                post_check_progress(&w, p, format!("{prefix} — {label}"))
            })?;
            Ok(out.map(|o| match check::judge(&dirs, &known, o) {
                check::Outcome::Ok(_) => false,
                check::Outcome::Failed(g) => g.culprits.contains(&culprit),
            }))
        });
        let secs = started.elapsed().as_secs();
        let _ = w.upgrade_in_event_loop(move |ui| {
            let mut k = c.lock().unwrap();
            if k.check.as_ref().map(|r| r.id) != Some(id) {
                return;
            }
            k.check = None;
            ui.set_check_running(false);
            let note = match result {
                Ok(None) => return,
                Ok(Some(check::Bisect::Partner(p))) => {
                    let known = k
                        .state
                        .found_incompatibilities
                        .iter()
                        .any(|(a, b)| (a == &culprit && b == &p) || (a == &p && b == &culprit));
                    if !known {
                        k.state.found_incompatibilities.push((culprit.clone(), p.clone()));
                        k.save_state();
                    }
                    if let Some(f) = k.failure.as_mut() {
                        f.failure.culprits = vec![culprit.clone(), p.clone()];
                    }
                    format!(
                        "Trouvé en {secs} s : {culprit} est incompatible avec {p}. L'erreur se reproduit avec ces deux mods \
                         seuls (et leurs dépendances). Incompatibilité mémorisée : elle apparaît dans les problèmes du modpack."
                    )
                }
                Ok(Some(check::Bisect::Alone)) => format!(
                    "{culprit} échoue même sans les autres mods : le problème vient de ce mod (ou de sa version), pas d'un conflit."
                ),
                Ok(Some(check::Bisect::Unknown)) => {
                    "Impossible d'isoler un mod partenaire : l'erreur dépend sans doute de plusieurs mods à la fois.".to_string()
                }
                Err(e) => format!("Recherche interrompue : {e:#}"),
            };
            if let Some(f) = k.failure.as_mut() {
                f.note = note.clone();
            }
            refresh_failure(&ui, &k);
            refresh_all(&ui, &k);
            ui.set_fail_visible(true);
            ui.set_status(note.into());
        });
    });
}

/// Mods chargés qui ne pourraient plus l'être sans `name` : ceux qui en dépendent
/// obligatoirement, en cascade.
fn dependents(k: &Core, res: &Resolution, name: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![name.to_string()];
    while let Some(n) = stack.pop() {
        for (m, v) in &res.mods {
            if m == name || out.contains(m) {
                continue;
            }
            let Some(info) = store::find(&k.index, m, v) else { continue };
            if info.deps().iter().any(|d| d.kind.is_required() && d.name == n) {
                out.insert(m.clone());
                stack.push(m.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// Désactive un mod, et ceux qui en dépendent obligatoirement, dans tous les modpacks actifs.
/// Renvoie les mods désactivés.
fn disable_everywhere(k: &mut Core, name: &str) -> Vec<String> {
    let res = k.resolution();
    let mut names = vec![name.to_string()];
    names.extend(dependents(k, &res, name));
    let active = k.state.active.clone();
    for id in active {
        if let Some(p) = k.state.pack_mut(&id) {
            for e in p.mods.iter_mut().filter(|e| e.enabled && names.contains(&e.name)) {
                e.enabled = false;
            }
        }
    }
    k.save_state();
    names
}

/// Conséquence affichée à côté du bouton « Désactiver » (et si elle mérite un avertissement).
fn disable_effect(k: &Core, res: &Resolution, name: &str) -> (String, bool) {
    let deps = dependents(k, res, name);
    match deps.len() {
        0 => ("Aucun autre mod n'en dépend.".to_string(), false),
        n => {
            let shown: Vec<&str> = deps.iter().take(6).map(String::as_str).collect();
            let more = if n > 6 { format!(" et {} autre(s)", n - 6) } else { String::new() };
            (format!("Désactivera aussi {n} mod(s) qui en dépendent : {}{more}.", shown.join(", ")), true)
        }
    }
}

fn refresh_failure(ui: &AppWindow, k: &Core) {
    let Some(f) = &k.failure else {
        ui.set_fail_visible(false);
        return;
    };
    let res = k.resolution();
    let rows: Vec<FailRow> = f
        .failure
        .culprits
        .iter()
        .take(8)
        .map(|name| {
            let title = res
                .mods
                .get(name)
                .and_then(|v| store::find(&k.index, name, v))
                .map(|i| display_title(&i.title, name))
                .unwrap_or_else(|| name.clone());
            let enabled = res.mods.contains_key(name);
            let (effect, effect_warn) = if enabled { disable_effect(k, &res, name) } else { (String::new(), false) };
            FailRow { name: name.clone().into(), title: title.into(), enabled, effect: effect.into(), effect_warn }
        })
        .collect();
    ui.set_fail_title(
        if f.runtime { "Factorio a signalé une erreur pendant la partie" } else { "Le chargement des mods échoue" }.into(),
    );
    ui.set_fail_text(f.failure.text.clone().into());
    ui.set_fail_note(f.note.clone().into());
    ui.set_fail_can_bisect(!f.runtime && !f.failure.culprits.is_empty() && f.note.is_empty());
    ui.set_fail_rows(model(rows));
}

/// Indicateurs de la barre latérale : mods déjà vérifiés, erreur en attente.
fn refresh_check(ui: &AppWindow, k: &mut Core) {
    let res = k.resolution();
    let fp = k.launch_fingerprint(&res);
    ui.set_check_ok(k.config.check_before_launch && k.check_dirs.is_known_ok(&fp));
    let failed = k.failure.as_ref().is_some_and(|f| f.runtime || f.fp == fp);
    if !failed && k.failure.is_some() && !ui.get_fail_visible() {
        // La liste a changé : l'ancienne erreur ne s'applique plus.
        k.failure = None;
    }
    ui.set_has_failure(failed);
}

/// Minuteur : met à jour les indicateurs de la barre latérale. Aucune vérification n'est
/// lancée d'elle-même : seulement via « Vérifier les mods actifs » ou au lancement du jeu.
fn status_tick(ui: &AppWindow, core: &Shared) {
    let mut k = core.lock().unwrap();
    refresh_check(ui, &mut k);
}

/// Lance réellement le jeu, puis analyse son log à la fermeture.
fn do_launch(ui: &AppWindow, core: &Shared, save: Option<PathBuf>, fp: Option<String>) {
    {
        let mut k = core.lock().unwrap();
        // Une vérification en arrière-plan ne doit pas concurrencer le jeu.
        if let Some(run) = k.check.take() {
            run.cancel.store(true, Ordering::SeqCst);
            ui.set_check_running(false);
        }
        k.game_running = true;
    }
    ui.set_fail_visible(false);
    task(ui, core, "Préparation du profil de lancement…", move |c, w| {
        let (exe, profile, res, index, builtins, settings, copy_back, single, user_dir, sprite_cache, known, titles, fp) = {
            let k = c.lock().unwrap();
            let exe = k.config.factorio_exe.clone().ok_or_else(|| anyhow!("exécutable de Factorio non configuré"))?;
            let settings = k.settings_source();
            let single = if k.state.active.len() == 1 { k.state.active.first().cloned() } else { None };
            let res = k.resolution();
            let (known, titles) = k.known_and_titles(&res);
            let fp = fp.unwrap_or_else(|| k.launch_fingerprint(&res));
            (
                exe,
                k.paths.profile.clone(),
                res,
                k.index.clone(),
                k.builtins.clone(),
                settings,
                k.config.copy_back_settings && k.config.feat_mod_settings,
                single,
                k.config.factorio_user_dir.clone(),
                k.config.cache_sprite_atlas,
                known,
                titles,
                fp,
            )
        };
        let result = (|| -> Result<(std::process::Child, Vec<String>)> {
            let missing = sync::build_profile(&profile, &res, &index, &builtins, settings.as_ref().map(|(_, p)| p.as_path()))?;
            let extra: &[&str] = if sprite_cache { &["--cache-sprite-atlas"] } else { &[] };
            Ok((factorio::launch(&exe, Some(&profile), save.as_deref(), extra)?, missing))
        })();
        let (mut child, missing) = match result {
            Ok(r) => r,
            Err(e) => {
                c.lock().unwrap().game_running = false;
                return Err(e);
            }
        };
        let mut msg = format!("Factorio lancé avec {} mod(s).", res.mods.len());
        if !missing.is_empty() {
            msg.push_str(&format!(" Absents du dépôt : {}", missing.join(", ")));
        }
        post_status(w, msg.clone());
        let target = settings.map(|(id, _)| id).or(single);
        let c2 = c.clone();
        let w2 = w.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            // Analyse du log de la partie : erreur de chargement ou erreur d'un mod en jeu.
            let log = user_dir
                .as_ref()
                .and_then(|d| fs::read_to_string(d.join("factorio-current.log")).ok())
                .unwrap_or_default();
            let inited = check::reached_init(&log);
            let failure = check::parse_log(&log, &known, &titles);
            let load_failed = failure.as_ref().is_some_and(|f| !f.runtime);
            {
                let mut k = c2.lock().unwrap();
                k.game_running = false;
                if load_failed {
                    // La vérification n'avait pas vu cette erreur : on ne garde ni la validation
                    // ni les fichiers introuvables que le jeu vient de refuser.
                    k.check_dirs.forget_ok(&fp);
                    k.check_dirs.remove_tolerated(&check::atlas_missing(&log));
                } else if inited {
                    k.check_dirs.remember_ok(&fp);
                    if let Some(missing) = k.pending_missing.remove(&fp) {
                        k.check_dirs.add_tolerated(&missing);
                    }
                }
            }
            if let Some(failure) = failure {
                let c3 = c2.clone();
                let _ = w2.upgrade_in_event_loop(move |ui| {
                    let mut k = c3.lock().unwrap();
                    let runtime = failure.runtime;
                    k.failure = Some(LastFailure { fp, failure, runtime, note: String::new(), save: None });
                    refresh_failure(&ui, &k);
                    refresh_check(&ui, &mut k);
                    ui.set_fail_visible(true);
                });
            }
            if !copy_back {
                return;
            }
            let Some(id) = target else { return };
            let src = profile.join("mod-settings.dat");
            if !src.exists() {
                return;
            }
            let (dst, name) = {
                let k = c2.lock().unwrap();
                (k.paths.pack_settings(&id), k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default())
            };
            if let Some(d) = dst.parent() {
                let _ = fs::create_dir_all(d);
            }
            if fs::copy(&src, &dst).is_ok() {
                post_status(&w2, format!("Partie terminée : réglages enregistrés dans « {name} »."));
                post_refresh(&w2, &c2);
            }
        });
        Ok(msg)
    });
}

fn toggle_pack(ui: &AppWindow, core: &Shared, id: &str, on: bool) {
    let auto_sync = {
        let mut k = core.lock().unwrap();
        k.state.active.retain(|a| a != id);
        if on {
            k.state.active.push(id.to_string());
        }
        k.save_state();
        refresh_all(ui, &k);
        k.config.auto_sync_on_activate
    };
    let res = core.lock().unwrap().resolution();
    if !res.conflicts.is_empty() {
        let mut k = core.lock().unwrap();
        show_conflicts(ui, &mut k, res.conflicts, if auto_sync { After::Sync } else { After::Nothing });
    } else if auto_sync {
        start_sync(ui, core);
    }
}

// ---------------------------------------------------------------------------
// Démarrage
// ---------------------------------------------------------------------------

pub fn run() -> Result<()> {
    let paths = Paths::new()?;
    let _instance = match instance::acquire(&paths.data)? {
        instance::Instance::Primary(file) => file,
        instance::Instance::Activated => return Ok(()),
        instance::Instance::Running => {
            rfd::MessageDialog::new()
                .set_title("Factorio Mod Manager")
                .set_description("Le gestionnaire est déjà ouvert : utilise la fenêtre existante (barre des tâches).")
                .set_level(rfd::MessageLevel::Info)
                .show();
            return Ok(());
        }
    };
    journal::init(paths.data.join("journal.log"));
    journal::log(format!("Démarrage de Factorio Mod Manager v{}", env!("CARGO_PKG_VERSION")));
    let mut config = Config::load(&paths.data.join("config.json"));
    if config.factorio_user_dir.is_none() {
        config.factorio_user_dir = factorio::default_user_dir().filter(|d| d.exists());
    }
    if config.factorio_exe.is_none() {
        config.factorio_exe = factorio::detect_exe(config.factorio_user_dir.as_deref());
    }
    let state = State::load(&paths.data.join("state.json"));
    let store = Store::new(paths.store.clone(), paths.cache.join("icons-128"))?;
    let portal = Arc::new(Portal::new(paths.cache.clone())?);
    let mut core = Core {
        paths,
        config,
        state,
        store,
        index: Index::new(),
        portal,
        portal_list: None,
        portal_list_at: None,
        last_check: None,
        checking: false,
        app_update: None,
        updates: BTreeMap::new(),
        builtins: vec![],
        game_version: None,
        icons: HashMap::new(),
        session_choices: BTreeMap::new(),
        selected_mod: None,
        selected_pack: None,
        dl: None,
        dl_gen: 0,
        conflicts: vec![],
        conflict_sel: vec![],
        after_conflict: After::Nothing,
        confirm: None,
        lib_selection: BTreeSet::new(),
        pending: BTreeMap::new(),
        check_dirs: CheckDirs::new(PathBuf::new()),
        check: None,
        check_gen: 0,
        failure: None,
        game_running: false,
        pending_missing: HashMap::new(),
        upd: None,
    };
    core.check_dirs = CheckDirs::new(core.paths.data.join("check"));
    core.refresh_factorio();
    core.selected_pack = core.state.packs.first().map(|p| p.id.clone());
    core.save_config();
    let core: Shared = Arc::new(Mutex::new(core));

    let ui = AppWindow::new()?;
    {
        let k = core.lock().unwrap();
        refresh_settings(&ui, &k);
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
        ui.set_search_categories(strings(CATEGORIES.iter().map(|(l, _)| l.to_string()).collect()));
        ui.set_search_versions(strings(VERSIONS.iter().map(|s| s.to_string()).collect()));
        let target = k.target_factorio(None);
        ui.set_search_version(VERSIONS.iter().position(|v| *v == target).unwrap_or(0) as i32);
    }
    wire_library(&ui, &core);
    wire_search(&ui, &core);
    wire_download(&ui, &core);
    wire_packs(&ui, &core);
    wire_dialogs(&ui, &core);
    wire_settings(&ui, &core);
    wire_updates(&ui, &core);
    wire_journal(&ui, &core);

    // Glisser-déposer de fichiers sur la fenêtre (événement winit, une fois la fenêtre créée).
    {
        let (c, w) = (core.clone(), ui.as_weak());
        slint::Timer::single_shot(Duration::ZERO, move || {
            use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
            let Some(ui) = w.upgrade() else { return };
            let w2 = w.clone();
            ui.window().on_winit_window_event(move |_, ev| {
                if let WindowEvent::DroppedFile(p) = ev {
                    if let Some(ui) = w2.upgrade() {
                        handle_drop(&ui, &c, p.clone());
                    }
                }
                EventResult::Propagate
            });
        });
    }
    #[cfg(windows)]
    let _tray = tray::setup(&ui, &core);

    task(&ui, &core, "Analyse du dépôt…", |c, w| {
        rescan(c);
        post_refresh(w, c);
        let (empty, auto, has_mods) = {
            let k = c.lock().unwrap();
            let has_mods = k
                .config
                .mods_dir()
                .and_then(|d| fs::read_dir(d).ok())
                .map(|mut rd| rd.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "zip")).unwrap_or(false)))
                .unwrap_or(false);
            (k.index.is_empty(), k.config.auto_check_updates, has_mods)
        };
        let _ = auto;
        if let Err(e) = ensure_portal_list(c, false) {
            eprintln!("index du portail: {e:#}");
        }
        let mut msg = periodic_check(c, w)?;
        post_refresh(w, c);
        fetch_missing_icons(c);
        if empty && has_mods {
            msg = "Dépôt vide : clique sur « Importer les mods de Factorio » pour reprendre tes mods actuels.".into();
        } else if msg.is_empty() {
            msg = "Prêt".into();
        }
        Ok(msg)
    });

    // Vérification régulière (mises à jour fréquentes des mods, index, version de l'app).
    let timer = slint::Timer::default();
    {
        let (c, w) = (core.clone(), ui.as_weak());
        timer.start(slint::TimerMode::Repeated, Duration::from_secs(60), move || {
            let Some(ui) = w.upgrade() else { return };
            let due = {
                let k = c.lock().unwrap();
                !k.checking && k.last_check.map(|t| t.elapsed() >= k.config.check_interval()).unwrap_or(false)
            };
            if due {
                task(&ui, &c, "Vérification périodique…", periodic_check);
            }
        });
    }

    let check_timer = slint::Timer::default();
    {
        let (c, w) = (core.clone(), ui.as_weak());
        check_timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), move || {
            if let Some(ui) = w.upgrade() {
                status_tick(&ui, &c);
            }
        });
    }

    ui.run()?;
    drop(timer);
    drop(check_timer);
    // Arrête une vérification en cours : l'instance de Factorio ne doit pas survivre à l'app.
    if let Some(run) = core.lock().unwrap().check.take() {
        run.cancel.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Câblage des callbacks
// ---------------------------------------------------------------------------

fn wire_library(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_library_filter_changed(move || {
        let ui = w.unwrap();
        refresh_library(&ui, &c.lock().unwrap());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_select_mod(move |name| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.selected_mod = Some(name.to_string());
        ui.set_add_version_index(0);
        ui.set_selected_mod(name.clone());
        refresh_detail(&ui, &k);
        let preset = k.index.get(name.as_str()).map(|v| v[0].version.to_string());
        drop(k);
        open_download(&ui, &c, name.to_string(), preset, DlTarget::NoPack, false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_import_existing(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Import des mods de Factorio dans le dépôt…", import_existing);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_check_updates(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Vérification des mises à jour…", |c, _| check_updates(c));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_refresh_library(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Analyse du dépôt…", |c, _| {
            rescan(c);
            fetch_missing_icons(c);
            Ok("Dépôt actualisé.".into())
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_lib_toggle(move |name, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        if on {
            k.lib_selection.insert(name.to_string());
        } else {
            k.lib_selection.remove(name.as_str());
        }
        // Mise à jour de la seule ligne : reconstruire la liste ferait perdre le défilement.
        let lib = ui.get_library();
        for i in 0..lib.row_count() {
            if let Some(mut r) = lib.row_data(i).filter(|r| r.name == name) {
                r.checked = on;
                lib.set_row_data(i, r);
            }
        }
        ui.set_lib_selected_count(k.lib_selection.len() as i32);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_lib_select_all(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let lib = ui.get_library();
        for i in 0..lib.row_count() {
            if let Some(r) = lib.row_data(i).filter(|r| !r.downloading) {
                k.lib_selection.insert(r.name.to_string());
            }
        }
        refresh_library(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_bulk_clear(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.lib_selection.clear();
        refresh_library(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_bulk_add(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some((id, pname)) =
            k.state.packs.get(ui.get_bulk_pack_index().max(0) as usize).map(|p| (p.id.clone(), p.name.clone()))
        else {
            ui.set_status("Crée d'abord un modpack.".into());
            return;
        };
        let infos: Vec<store::ModInfo> =
            k.lib_selection.iter().filter_map(|n| k.index.get(n).map(|v| v[0].clone())).collect();
        let mut deps = 0;
        for info in &infos {
            deps += add_mod_to_pack(&mut k, &id, info);
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{} mod(s) ajouté(s) à « {pname} » (+{deps} dépendance(s)).", infos.len()).into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_bulk_remove(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let idx = ui.get_bulk_pack_index().max(0) as usize;
        let selection = k.lib_selection.clone();
        let Some(pack) = k.state.packs.get_mut(idx) else { return };
        let before = pack.mods.len();
        pack.mods.retain(|e| !selection.contains(&e.name));
        let (n, pname) = (before - pack.mods.len(), pack.name.clone());
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{n} mod(s) retiré(s) de « {pname} ».").into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_bulk_delete(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let (used, free): (Vec<String>, Vec<String>) = k
            .lib_selection
            .iter()
            .filter(|n| k.index.contains_key(*n))
            .cloned()
            .partition(|n| !k.state.packs_using(n, None).is_empty());
        if free.is_empty() {
            ui.set_status("Les mods sélectionnés sont tous utilisés par un modpack : retire-les d'abord des modpacks.".into());
            return;
        }
        let mut text = format!(
            "{} mod(s) et toutes leurs versions seront supprimés définitivement du dépôt :\n{}",
            free.len(),
            free.join(", ")
        );
        if !used.is_empty() {
            text.push_str(&format!(
                "\n\n{} mod(s) utilisé(s) par un modpack ne seront pas supprimés :\n{}",
                used.len(),
                used.join(", ")
            ));
        }
        ui.set_confirm_title(format!("Supprimer {} mod(s) du dépôt ?", free.len()).into());
        ui.set_confirm_text(text.into());
        ui.set_confirm_ok("Supprimer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::BulkDelete(free));
    });
    ui.on_open_portal(|name| {
        let _ = open::that(format!("https://mods.factorio.com/mod/{}", portal::urlencode(&name)));
    });
    ui.on_open_url(|url| {
        if url.starts_with("http://") || url.starts_with("https://") {
            let _ = open::that(url.as_str());
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_download_update(move |name| {
        let ui = w.unwrap();
        open_updates(&ui, &c, Some(vec![name.to_string()]), None);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_update_all(move || {
        let ui = w.unwrap();
        open_updates(&ui, &c, None, None);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_close_detail(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.selected_mod = None;
        if !ui.get_dl_visible() {
            k.dl = None;
        }
        refresh_detail(&ui, &k);
        ui.set_selected_mod("".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_clean_repo(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let list = cleanable_versions(&k);
        if list.is_empty() {
            ui.set_status("Rien à nettoyer : chaque ancienne version est utilisée par un modpack.".into());
            return;
        }
        let total: u64 = list.iter().map(|m| file_size(&m.path)).sum();
        let mut text = format!(
            "{} ancienne(s) version(s) ne sont utilisées par aucun modpack (ni par leur historique). \
             La version la plus récente de chaque mod est conservée.\n\nPlace libérée : {}\n\n",
            list.len(),
            human_size(total)
        );
        for m in &list {
            text.push_str(&format!("  - {} {}  ({})\n", m.name, m.version, human_size(file_size(&m.path))));
        }
        ui.set_confirm_title(format!("Libérer {} dans le dépôt ?", human_size(total)).into());
        ui.set_confirm_text(text.into());
        ui.set_confirm_ok("Nettoyer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::CleanRepo(list));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_delete_version(move |name, version| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Ok(v) = version.parse::<Version>() else { return };
        ui.set_confirm_title(format!("Supprimer {name} {version} du dépôt ?").into());
        ui.set_confirm_text("Le fichier sera supprimé définitivement du dépôt. Tu pourras le retélécharger depuis le portail.".into());
        ui.set_confirm_ok("Supprimer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::DeleteVersion(name.to_string(), v));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_use_version_everywhere(move |name, version| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let mut n = 0;
        let pinned = !is_latest(&k.index, &name, &version.parse().unwrap_or_default());
        for p in k.state.packs.iter_mut() {
            if let Some(e) = p.mods.iter_mut().find(|e| e.name == name.as_str()) {
                e.version = version.to_string();
                e.pinned = pinned;
                n += 1;
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{name} {version} utilisé dans {n} modpack(s).").into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_add_to_pack(move |name, pack_idx, ver_idx| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(versions) = k.index.get(name.as_str()).cloned() else { return };
        let Some(info) = versions.get(ver_idx.max(0) as usize).cloned() else { return };
        let Some((pack_id, pname)) =
            k.state.packs.get(pack_idx.max(0) as usize).map(|p| (p.id.clone(), p.name.clone()))
        else {
            return;
        };
        let added = add_mod_to_pack(&mut k, &pack_id, &info);
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{} {} ajouté à « {pname} » (+{added} dépendance(s)).", info.name, info.version).into());
    });
}

fn wire_search(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_do_search(move || {
        let ui = w.unwrap();
        let query = ui.get_search_query().to_string();
        let cat = CATEGORIES.get(ui.get_search_category().max(0) as usize).and_then(|(_, c)| *c).map(String::from);
        let ver = match ui.get_search_version() {
            i if i > 0 => VERSIONS.get(i as usize).map(|s| s.to_string()),
            _ => None,
        };
        let loaded = c.lock().unwrap().portal_list.is_some();
        let label = if loaded { "Recherche…" } else { "Téléchargement de l'index du portail (une fois par jour)…" };
        let stale = portal_list_stale(&c.lock().unwrap());
        task(&ui, &c, label, move |c, w| {
            let list = ensure_portal_list(c, false)?;
            let hits = portal::search(&list, &query, cat.as_deref(), ver.as_deref(), 200);
            let names: Vec<String> = hits.iter().map(|m| m.name.clone()).collect();
            let data: Vec<ModListEntry> = hits.into_iter().cloned().collect();
            let total = data.len();
            let c2 = c.clone();
            let index_size = list.len();
            let _ = w.upgrade_in_event_loop(move |ui| {
                let k = c2.lock().unwrap();
                let rows: Vec<SearchRow> = data
                    .iter()
                    .map(|m| {
                        let rel = m.latest_release.as_ref();
                        let title = display_title(&m.title, &m.name);
                        let thumb = k.portal_thumb_cached(&m.name);
                        let icon = load_image(thumb.as_ref());
                        SearchRow {
                            name: m.name.clone().into(),
                            letter: letter(&title),
                            title: title.into(),
                            owner: one_line(&m.owner).into(),
                            summary: short(&m.summary, 300).into(),
                            category: portal::category_label(m.category.as_deref().unwrap_or("")).into(),
                            is_modpack: portal::is_modpack_category(m.category.as_deref()),
                            downloads: thousands(m.downloads_count).into(),
                            factorio: rel.map(|r| r.factorio()).unwrap_or_default().into(),
                            version: rel.map(|r| r.version.clone()).unwrap_or_default().into(),
                            has_icon: icon.is_some(),
                            icon: icon.unwrap_or_default(),
                            installed: k
                                .index
                                .get(&m.name)
                                .map(|v| v[0].version.to_string())
                                .unwrap_or_default()
                                .into(),
                        }
                    })
                    .collect();
                ui.set_search_results(model(rows));
                ui.set_search_info(format!("{total} résultat(s) • index : {index_size} mods").into());
            });
            // Miniatures des premiers résultats
            let portal = c.lock().unwrap().portal.clone();
            let first: Vec<String> = names.into_iter().take(60).collect();
            for chunk in first.chunks(12) {
                let found = parallel_map(chunk, 6, |n| portal.thumbnail(n).map(|p| (n.clone(), p)));
                let _ = w.upgrade_in_event_loop(move |ui| {
                    let rows = ui.get_search_results();
                    for (name, path) in found {
                        for i in 0..rows.row_count() {
                            if let Some(mut r) = rows.row_data(i) {
                                if r.name.as_str() == name {
                                    if let Some(img) = load_image(Some(&path)) {
                                        r.icon = img;
                                        r.has_icon = true;
                                        rows.set_row_data(i, r);
                                    }
                                }
                            }
                        }
                    }
                });
            }
            if stale && loaded {
                // Index ancien : on le rafraîchit puis on relance la recherche avec la liste à jour.
                post_status(w, "Rafraîchissement de l'index du portail…");
                ensure_portal_list(c, true)?;
                let _ = w.upgrade_in_event_loop(|ui| ui.invoke_do_search());
                return Ok("Index du portail rafraîchi.".into());
            }
            Ok(format!("{total} résultat(s)."))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_refresh_index(move || {
        let ui = w.unwrap();
        task(&ui, &c, "Téléchargement de l'index du portail…", |c, _| {
            let l = ensure_portal_list(c, true)?;
            Ok(format!("Index du portail mis à jour ({} mods).", l.len()))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_open_download(move |name| {
        let ui = w.unwrap();
        open_download(&ui, &c, name.to_string(), None, DlTarget::SelectedPack, true);
    });
}

impl Core {
    fn portal_thumb_cached(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = self.icons.get(name) {
            return Some(p.clone());
        }
        let p = self.paths.cache.join(portal::THUMBS_DIR).join(format!("{}.png", portal::sanitize(name)));
        p.exists().then_some(p)
    }
}

fn wire_download(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_version_changed(move |idx| {
        let ui = w.unwrap();
        ui.set_dl_loading(true);
        let c = c.clone();
        let w = ui.as_weak();
        std::thread::spawn(move || load_dl_items(&c, &w, idx.max(0) as usize));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_toggle_dep(move |i, checked| {
        let _ = &w;
        let mut k = c.lock().unwrap();
        if let Some(item) = k.dl.as_mut().and_then(|d| d.items.get_mut(i as usize)) {
            if item.checkable {
                item.checked = checked;
            }
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_toggle_group(move |group, checked| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(d) = k.dl.as_mut() {
            for item in d.items.iter_mut().filter(|i| i.checkable && i.kind.group() == group) {
                item.checked = checked;
            }
        }
        refresh_dl(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_cancel(move || {
        let ui = w.unwrap();
        c.lock().unwrap().dl = None;
        ui.set_dl_visible(false);
        reselect_mod(&ui);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_dl_confirm(move || {
        let ui = w.unwrap();
        let (target, title) = {
            let k = c.lock().unwrap();
            let idx = ui.get_dl_target_index();
            let target = if idx > 0 { k.state.packs.get(idx as usize - 1).map(|p| p.id.clone()) } else { None };
            (target, k.dl.as_ref().map(|d| d.title.clone()).unwrap_or_default())
        };
        // La création de modpack n'est proposée que dans la fenêtre.
        let new_pack = (ui.get_dl_visible() && ui.get_dl_is_modpack() && ui.get_dl_make_pack()).then(|| {
            let n = ui.get_dl_pack_name().trim().to_string();
            if n.is_empty() { title.clone() } else { n }
        });
        let target = if new_pack.is_some() { None } else { target };
        ui.set_dl_visible(false);
        task(&ui, &c, &format!("Téléchargement de {title}…"), move |c, w| {
            let res = confirm_download(c, w, target, new_pack);
            let _ = w.upgrade_in_event_loop(|ui| reselect_mod(&ui));
            res
        });
    });
}

/// Recharge la liste des dépendances intégrée à la fiche du mod sélectionné (bibliothèque).
fn reselect_mod(ui: &AppWindow) {
    let sel = ui.get_selected_mod();
    if ui.get_page() == 0 && !sel.is_empty() && !ui.get_dl_visible() {
        ui.invoke_select_mod(sel);
    }
}

fn wire_packs(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_new_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let mut p = Modpack::new("Nouveau modpack");
        if k.config.feat_factorio_version {
            let t = k.target_factorio(None);
            p.factorio_version = (!t.is_empty()).then_some(t);
        }
        k.selected_pack = Some(p.id.clone());
        k.state.packs.push(p);
        k.save_state();
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_select_pack(move |id| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.selected_pack = Some(id.to_string());
        ui.set_pack_add_query("".into());
        ui.set_pack_add_results(model(vec![]));
        ui.set_pack_mod_filter("".into());
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_toggle_pack(move |id, on| {
        let ui = w.unwrap();
        toggle_pack(&ui, &c, id.as_str(), on);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_save_pack_meta(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let name = ui.get_pack_name().trim().to_string();
        let desc = ui.get_pack_description().to_string();
        let fv = ui.get_pack_factorio().trim().to_string();
        if let Some(p) = k.state.pack_mut(&id) {
            if !name.is_empty() {
                p.name = name;
            }
            p.description = desc;
            p.factorio_version = (!fv.is_empty()).then_some(fv);
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status("Modpack enregistré.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_delete_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let name = k.state.pack(&id).map(|p| p.name.clone()).unwrap_or_default();
        ui.set_confirm_title(format!("Supprimer le modpack « {name} » ?").into());
        ui.set_confirm_text("Le modpack et ses réglages seront supprimés. Les mods restent dans le dépôt.".into());
        ui.set_confirm_ok("Supprimer".into());
        ui.set_confirm_visible(true);
        k.confirm = Some(Confirm::DeletePack(id));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_duplicate_pack(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(src) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)).cloned() else { return };
        let mut p = src.clone();
        p.id = modpack::new_id();
        p.name = format!("{} (copie)", src.name);
        let from = k.paths.pack_settings(&src.id);
        if from.exists() {
            let to = k.paths.pack_settings(&p.id);
            let _ = fs::create_dir_all(to.parent().unwrap());
            let _ = fs::copy(from, to);
        }
        k.selected_pack = Some(p.id.clone());
        k.state.packs.push(p);
        k.save_state();
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_version(move |i, vi| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let index = k.index.clone();
        if let Some(e) = k.state.pack_mut(&id).and_then(|p| p.mods.get_mut(i as usize)) {
            if let Some(v) = pack_mod_versions(&index, e).get(vi.max(0) as usize) {
                e.version = v.clone();
                e.pinned = !is_latest(&index, &e.name, &v.parse().unwrap_or_default());
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_enabled(move |i, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(e) = k.state.pack_mut(&id).and_then(|p| p.mods.get_mut(i as usize)) {
            e.enabled = on;
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_remove(move |i| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if (i as usize) < p.mods.len() {
                p.mods.remove(i as usize);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_builtin(move |i, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let Some(name) = k.builtins.get(i as usize).cloned() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            p.disabled_builtins.retain(|b| b != &name);
            if !on {
                p.disabled_builtins.push(name);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_capture_settings(move || {
        let ui = w.unwrap();
        let k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let candidates: Vec<PathBuf> = [k.config.mods_dir(), Some(k.paths.profile.clone())]
            .into_iter()
            .flatten()
            .map(|d| d.join("mod-settings.dat"))
            .filter(|p| p.exists())
            .collect();
        let newest = candidates.into_iter().max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        let msg = match newest {
            None => "Aucun mod-settings.dat trouvé (lance le jeu une fois).".to_string(),
            Some(src) => {
                let dst = k.paths.pack_settings(&id);
                let _ = fs::create_dir_all(dst.parent().unwrap());
                match fs::copy(&src, &dst) {
                    Ok(_) => format!("Réglages capturés depuis {}.", src.display()),
                    Err(e) => format!("Erreur : {e}"),
                }
            }
        };
        refresh_all(&ui, &k);
        ui.set_status(msg.into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_clear_settings(move || {
        let ui = w.unwrap();
        let k = c.lock().unwrap();
        if let Some(id) = &k.selected_pack {
            let _ = fs::remove_file(k.paths.pack_settings(id));
        }
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_add_save(move || {
        let ui = w.unwrap();
        let dir = c.lock().unwrap().config.factorio_user_dir.as_ref().map(|d| d.join("saves"));
        let mut dlg = rfd::FileDialog::new().add_filter("Sauvegarde Factorio", &["zip"]);
        if let Some(d) = dir {
            dlg = dlg.set_directory(d);
        }
        let Some(path) = dlg.pick_file() else { return };
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if !p.saves.contains(&path) {
                p.saves.push(path);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_remove_save(move |i| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(p) = k.state.pack_mut(&id) {
            if (i as usize) < p.saves.len() {
                p.saves.remove(i as usize);
            }
        }
        k.save_state();
        refresh_all(&ui, &k);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_launch_save(move |i| {
        let ui = w.unwrap();
        let save = {
            let mut k = c.lock().unwrap();
            let Some(id) = k.selected_pack.clone() else { return };
            let Some(save) = k.state.pack(&id).and_then(|p| p.saves.get(i as usize)).cloned() else { return };
            if !k.state.is_active(&id) {
                k.state.active.push(id);
                k.save_state();
            }
            refresh_all(&ui, &k);
            save
        };
        proceed(&ui, &c, After::Launch(Some(save)));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_export_pack(move || {
        let ui = w.unwrap();
        let (pack, settings) = {
            let k = c.lock().unwrap();
            let Some(p) = k.selected_pack.as_ref().and_then(|id| k.state.pack(id)).cloned() else { return };
            let s = k.config.feat_mod_settings.then(|| k.paths.pack_settings(&p.id));
            (p, s)
        };
        let Some(dest) = rfd::FileDialog::new()
            .add_filter("Modpack", &["fmpack"])
            .set_file_name(format!("{}.fmpack", portal::sanitize(&pack.name)))
            .save_file()
        else {
            return;
        };
        task(&ui, &c, "Export du modpack…", move |c, _| {
            let (index, include) = {
                let k = c.lock().unwrap();
                (k.index.clone(), k.config.export_include_mods)
            };
            transfer::export(&pack, &dest, &index, settings.as_deref(), include)?;
            Ok(format!("Modpack exporté vers {}.", dest.display()))
        });
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_update(move || {
        let ui = w.unwrap();
        let id = c.lock().unwrap().selected_pack.clone();
        if let Some(id) = id {
            open_updates(&ui, &c, None, Some(id));
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_undo(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let Some((at, name)) = k.state.pack_mut(&id).and_then(|p| p.undo().map(|at| (at, p.name.clone()))) else { return };
        k.save_state();
        refresh_all(&ui, &k);
        let msg = format!("« {name} » est revenu à son état d'avant la mise à jour du {at}.");
        journal::log(&msg);
        ui.set_status(msg.into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_import_pack(move || {
        let ui = w.unwrap();
        let Some(src) = rfd::FileDialog::new().add_filter("Modpack", &["fmpack"]).pick_file() else { return };
        task(&ui, &c, "Import du modpack…", move |c, w| import_pack(c, w, &src));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_deps(move |i| {
        let ui = w.unwrap();
        let (id, entry) = {
            let k = c.lock().unwrap();
            let Some(id) = k.selected_pack.clone() else { return };
            let Some(e) = k.state.pack(&id).and_then(|p| p.mods.get(i as usize)).cloned() else { return };
            (id, e)
        };
        open_download(&ui, &c, entry.name, Some(entry.version), DlTarget::Pack(id), true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_disable_mod(move |name| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        if let Some(e) = k.state.pack_mut(&id).and_then(|p| p.mods.iter_mut().find(|e| e.name == name.as_str())) {
            e.enabled = false;
        }
        k.save_state();
        refresh_all(&ui, &k);
        ui.set_status(format!("{name} désactivé dans le modpack.").into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_auto_fix(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let Some(id) = k.selected_pack.clone() else { return };
        let (index, builtins, game) = (k.index.clone(), k.builtins.clone(), k.target_factorio(None));
        let found = k.state.found_incompatibilities.clone();
        let Some(pack) = k.state.pack_mut(&id) else { return };
        let off = modpack::auto_disable(pack, &index, &builtins, &game, &found);
        let left = modpack::pack_issues(pack, &index, &builtins, &game, &found).len();
        k.save_state();
        refresh_all(&ui, &k);
        let mut msg = format!("{} mod(s) désactivé(s) : {}.", off.len(), off.join(", "));
        if left > 0 {
            msg.push_str(&format!(" {left} incompatibilité(s) restent à arbitrer."));
        }
        ui.set_status(msg.into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_mod_filter_changed(move || {
        let ui = w.unwrap();
        refresh_pack_detail(&ui, &c.lock().unwrap(), false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_add_search(move || {
        let ui = w.unwrap();
        let query = ui.get_pack_add_query().to_string();
        let needs_index = c.lock().unwrap().portal_list.is_none() && query.trim().chars().count() >= 2;
        let rows = pack_add_rows(&c.lock().unwrap(), &query);
        ui.set_pack_add_results(model(rows));
        if needs_index {
            // Charge l'index du portail en arrière-plan puis relance la recherche.
            task(&ui, &c, "Chargement de l'index du portail…", |c, w| {
                ensure_portal_list(c, false)?;
                let _ = w.upgrade_in_event_loop(|ui| ui.invoke_pack_add_search());
                Ok(String::new())
            });
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_pack_add(move |name, installed| {
        let ui = w.unwrap();
        // Dans tous les cas, on propose les dépendances avant d'ajouter le mod au modpack.
        let preset = if installed {
            c.lock().unwrap().index.get(name.as_str()).map(|v| v[0].version.to_string())
        } else {
            None
        };
        let Some(id) = c.lock().unwrap().selected_pack.clone() else { return };
        ui.set_pack_add_query("".into());
        ui.set_pack_add_results(model(vec![]));
        // Ajout depuis la vue d'un modpack : on met à jour ce modpack, sans en proposer un nouveau.
        open_download(&ui, &c, name.to_string(), preset, DlTarget::Pack(id), true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_browse_modpacks(move || {
        let ui = w.unwrap();
        ui.set_page(1);
        ui.set_search_category(1);
        ui.invoke_do_search();
        let _ = &c;
    });
}

fn import_pack(c: &Shared, w: &Weak<AppWindow>, src: &Path) -> Result<String> {
    let (store, tmp, portal, creds, downloads) = {
        let k = c.lock().unwrap();
        (k.store.clone(), k.paths.downloads.join("import"), k.portal.clone(), k.credentials(), k.paths.downloads.clone())
    };
    let imported = transfer::import(src, &store, &tmp)?;
    let mut fetched = 0;
    let mut failed = Vec::new();
    if !imported.missing.is_empty() {
        let n = imported.missing.len();
        for (i, (name, ver)) in imported.missing.iter().enumerate() {
            post_status(w, format!("Téléchargement {}/{n} : {name} {ver}", i + 1));
            post_progress(w, i as f32 / n as f32);
            let r = (|| -> Result<()> {
                let (u, t) = creds.clone().ok_or_else(|| anyhow!("identifiants manquants"))?;
                let full = portal.full(name)?;
                let rel = full
                    .releases
                    .iter()
                    .find(|r| &r.version == ver)
                    .ok_or_else(|| anyhow!("version introuvable"))?;
                let p = portal.download(rel, &u, &t, &downloads, |_, _| {})?;
                store.add_file(&p, AddMode::Move)?;
                Ok(())
            })();
            match r {
                Ok(()) => fetched += 1,
                Err(e) => failed.push(format!("{name} {ver} ({e})")),
            }
        }
    }
    rescan(c);
    let mut k = c.lock().unwrap();
    let pack = imported.pack;
    if let Some(bytes) = imported.settings {
        let dst = k.paths.pack_settings(&pack.id);
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::write(dst, bytes)?;
    }
    let name = pack.name.clone();
    k.selected_pack = Some(pack.id.clone());
    k.state.packs.push(pack);
    k.save_state();
    let mut msg = format!("Modpack « {name} » importé ({fetched} mod(s) téléchargé(s))");
    if !failed.is_empty() {
        msg.push_str(&format!(" — échecs : {}", failed.join(", ")));
    }
    Ok(msg)
}

fn wire_dialogs(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_select(move |i, sel| {
        let _ = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(s) = k.conflict_sel.get_mut(i as usize) {
            *s = sel.max(0) as usize;
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_confirm(move || {
        let ui = w.unwrap();
        let after = {
            let mut k = c.lock().unwrap();
            let remember = ui.get_conflict_remember();
            let picks: Vec<(String, String)> = k
                .conflicts
                .iter()
                .zip(k.conflict_sel.iter())
                .filter_map(|(c, s)| c.options.get(*s).map(|o| (c.name.clone(), o.version.clone())))
                .collect();
            for (name, ver) in picks {
                if remember {
                    k.state.conflict_choices.insert(name.clone(), ver.clone());
                    k.session_choices.remove(&name);
                } else {
                    k.session_choices.insert(name, ver);
                }
            }
            k.conflicts.clear();
            k.save_state();
            refresh_all(&ui, &k);
            std::mem::replace(&mut k.after_conflict, After::Nothing)
        };
        ui.set_conflict_visible(false);
        match after {
            After::Nothing => ui.set_status("Conflits résolus.".into()),
            After::Sync => start_sync(&ui, &c),
            After::Launch(s) => start_launch(&ui, &c, s),
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_conflict_cancel(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.conflicts.clear();
        k.after_conflict = After::Nothing;
        ui.set_conflict_visible(false);
        ui.set_status("Conflits non résolus : la version la plus récente sera utilisée par défaut.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_confirm_cancel(move || {
        let ui = w.unwrap();
        c.lock().unwrap().confirm = None;
        ui.set_confirm_visible(false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_confirm_accept(move || {
        let ui = w.unwrap();
        ui.set_confirm_visible(false);
        let action = c.lock().unwrap().confirm.take();
        match action {
            None => {}
            Some(Confirm::Sync(plan, settings)) => {
                task(&ui, &c, "Synchronisation du dossier mods/…", move |c, _| {
                    let mods_dir = c.lock().unwrap().config.mods_dir().ok_or_else(|| anyhow!("dossier non configuré"))?;
                    sync::apply(&mods_dir, &plan, settings.as_deref())?;
                    journal::log(format!("Synchronisation de {} :\n{}", mods_dir.display(), plan.summary()));
                    let backup = if settings.is_some() { "mod-list.json et mod-settings.dat (.fmm-backup)" } else { "mod-list.json.fmm-backup" };
                    Ok(format!("Factorio synchronisé : +{} / -{} mod(s). Sauvegarde : {backup}", plan.add.len(), plan.remove.len()))
                });
            }
            Some(Confirm::CleanRepo(list)) => {
                let mut k = c.lock().unwrap();
                let (mut freed, mut n) = (0u64, 0);
                let mut errors = Vec::new();
                for m in &list {
                    let size = file_size(&m.path);
                    match k.store.remove(m) {
                        Ok(()) => {
                            freed += size;
                            n += 1;
                            if let Some(v) = k.index.get_mut(&m.name) {
                                v.retain(|x| x.version != m.version);
                            }
                        }
                        Err(e) => errors.push(format!("{} {} ({e})", m.name, m.version)),
                    }
                }
                refresh_all(&ui, &k);
                let mut msg = format!("Dépôt nettoyé : {n} version(s) supprimée(s), {} libéré(s).", human_size(freed));
                if !errors.is_empty() {
                    msg.push_str(&format!(" Erreurs : {}", errors.join(", ")));
                }
                ui.set_status(msg.into());
            }
            Some(Confirm::DeletePack(id)) => {
                let mut k = c.lock().unwrap();
                k.state.packs.retain(|p| p.id != id);
                k.state.active.retain(|a| a != &id);
                let _ = fs::remove_dir_all(k.paths.packs.join(&id));
                k.selected_pack = k.state.packs.first().map(|p| p.id.clone());
                k.save_state();
                refresh_all(&ui, &k);
                refresh_pack_detail(&ui, &k, true);
                ui.set_status("Modpack supprimé.".into());
            }
            Some(Confirm::BulkDelete(names)) => {
                let mut k = c.lock().unwrap();
                let mut removed = 0;
                let mut errors = Vec::new();
                for name in &names {
                    for info in k.index.get(name).cloned().unwrap_or_default() {
                        match k.store.remove(&info) {
                            Ok(()) => removed += 1,
                            Err(e) => errors.push(format!("{} {} ({e})", info.name, info.version)),
                        }
                    }
                    k.index.remove(name);
                    k.icons.remove(name);
                    k.lib_selection.remove(name);
                    if k.selected_mod.as_deref() == Some(name.as_str()) {
                        k.selected_mod = None;
                    }
                }
                refresh_all(&ui, &k);
                let mut msg = format!("{} mod(s) supprimé(s) du dépôt ({removed} fichier(s)).", names.len());
                if !errors.is_empty() {
                    msg.push_str(&format!(" Erreurs : {}", errors.join(", ")));
                }
                ui.set_status(msg.into());
            }
            Some(Confirm::DeleteVersion(name, v)) => {
                let mut k = c.lock().unwrap();
                let info = store::find(&k.index, &name, &v).cloned();
                let msg = match info.map(|i| k.store.remove(&i)) {
                    Some(Ok(())) => {
                        if let Some(list) = k.index.get_mut(&name) {
                            list.retain(|m| m.version != v);
                            if list.is_empty() {
                                k.index.remove(&name);
                                k.selected_mod = None;
                            }
                        }
                        format!("{name} {v} supprimé du dépôt.")
                    }
                    Some(Err(e)) => format!("Erreur : {e:#}"),
                    None => "Version introuvable.".into(),
                };
                refresh_all(&ui, &k);
                ui.set_status(msg.into());
            }
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_sync_now(move || {
        let ui = w.unwrap();
        proceed(&ui, &c, After::Sync);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_launch_game(move || {
        let ui = w.unwrap();
        proceed(&ui, &c, After::Launch(None));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_fail_disable(move |name| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let off = disable_everywhere(&mut k, &name);
        refresh_failure(&ui, &k);
        refresh_all(&ui, &k);
        refresh_pack_detail(&ui, &k, false);
        let msg = if off.len() > 1 {
            format!("{name} désactivé, ainsi que {} mod(s) qui en dépendaient : {}.", off.len() - 1, off[1..].join(", "))
        } else {
            format!("{name} désactivé dans les modpacks actifs.")
        };
        ui.set_status(msg.into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_fail_bisect(move || start_bisect(&w.unwrap(), &c));
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_fail_launch(move || {
        let ui = w.unwrap();
        ui.set_fail_visible(false);
        let save = c.lock().unwrap().failure.as_ref().and_then(|f| f.save.clone());
        proceed(&ui, &c, After::Launch(save));
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_fail_launch_anyway(move || {
        let ui = w.unwrap();
        let save = c.lock().unwrap().failure.as_ref().and_then(|f| f.save.clone());
        do_launch(&ui, &c, save, None);
    });
    let w = ui.as_weak();
    ui.on_fail_close(move || w.unwrap().set_fail_visible(false));
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_show_failure(move || {
        let ui = w.unwrap();
        refresh_failure(&ui, &c.lock().unwrap());
        ui.set_fail_visible(true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_check_now(move || {
        let ui = w.unwrap();
        {
            let mut k = c.lock().unwrap();
            if k.config.factorio_exe.is_none() {
                ui.set_status("Configure l'exécutable de Factorio dans Paramètres.".into());
                return;
            }
            if !k.resolution().conflicts.is_empty() {
                drop(k);
                proceed(&ui, &c, After::Nothing);
                return;
            }
            if let Some(run) = k.check.take() {
                if run.bisecting {
                    k.check = Some(run);
                    ui.set_status("Recherche du conflit en cours : attends sa fin ou annule-la.".into());
                    return;
                }
                run.cancel.store(true, Ordering::SeqCst);
            }
        }
        start_check(&ui, &c, None, true);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_cancel_check(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(run) = k.check.take() {
            run.cancel.store(true, Ordering::SeqCst);
        }
        ui.set_check_running(false);
        ui.set_status("Vérification annulée.".into());
    });
}

fn wire_updates(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_upd_toggle(move |i, on| {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        if let Some(it) = k.upd.as_mut().and_then(|u| u.items.get_mut(i as usize)) {
            it.checked = on;
        }
        let n = k.upd.as_ref().map(|u| u.items.iter().filter(|i| i.checked).count()).unwrap_or(0);
        ui.set_upd_count(n as i32);
    });
    let c = core.clone();
    ui.on_upd_opt_toggle(move |i, on| {
        let mut k = c.lock().unwrap();
        if let Some(o) = k.upd.as_mut().and_then(|u| u.opts.get_mut(i as usize)).filter(|o| o.checkable) {
            o.checked = on;
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_upd_cancel(move || {
        let ui = w.unwrap();
        c.lock().unwrap().upd = None;
        ui.set_upd_visible(false);
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_upd_confirm(move || {
        let ui = w.unwrap();
        let Some(session) = c.lock().unwrap().upd.take() else { return };
        ui.set_upd_visible(false);
        task(&ui, &c, "Installation des mises à jour…", move |c, w| run_updates(c, w, session));
    });
}

fn wire_journal(ui: &AppWindow, core: &Shared) {
    let _ = core;
    ui.on_status_logged(|s| {
        // Les messages « en cours… » sont suivis de leur résultat : seul celui-ci est gardé.
        let s = s.trim();
        if !s.is_empty() && !s.ends_with('…') && s != "Prêt" && !s.starts_with("Téléchargement : ") {
            journal::log(s);
        }
    });
    let w = ui.as_weak();
    ui.on_journal_refresh(move || {
        let ui = w.unwrap();
        let text = journal::tail(800);
        ui.set_journal_text(if text.is_empty() { "Le journal est vide.".into() } else { text.into() });
    });
    ui.on_journal_open_file(|| {
        if let Some(p) = journal::path() {
            let _ = open::that(p);
        }
    });
    let w = ui.as_weak();
    ui.on_journal_clear(move || {
        journal::clear();
        let ui = w.unwrap();
        ui.invoke_journal_refresh();
    });
}

/// Icône dans la zone de notification (Windows) : lancer le jeu avec les modpacks actifs, afficher
/// la fenêtre, quitter.
#[cfg(windows)]
mod tray {
    use super::{AppWindow, Shared, instance};
    use slint::ComponentHandle;
    use std::time::Duration;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        _icon: TrayIcon,
        _timer: slint::Timer,
    }

    pub fn setup(ui: &AppWindow, core: &Shared) -> Option<Tray> {
        let img = image::load_from_memory(include_bytes!("../ui/icon.png")).ok()?.thumbnail(32, 32).into_rgba8();
        let (w, h) = img.dimensions();
        let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()?;
        let launch = MenuItem::new("Lancer Factorio", true, None);
        let show = MenuItem::new("Afficher la fenêtre", true, None);
        let quit = MenuItem::new("Quitter", true, None);
        let menu = Menu::new();
        menu.append_items(&[&launch, &show, &quit]).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Factorio Mod Manager")
            .with_icon(icon)
            .build()
            .ok()?;
        let (launch_id, show_id, quit_id) = (launch.id().clone(), show.id().clone(), quit.id().clone());
        let timer = slint::Timer::default();
        let (c, wk) = (core.clone(), ui.as_weak());
        let mut label = String::new();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            let Some(ui) = wk.upgrade() else { return };
            // Libellé : modpacks qui seront lancés.
            let want = {
                let k = c.lock().unwrap();
                let names: Vec<String> = k.state.active_packs().iter().map(|p| p.name.clone()).collect();
                if names.is_empty() { "Lancer Factorio".to_string() } else { format!("Lancer Factorio ({})", names.join(", ")) }
            };
            if want != label {
                launch.set_text(&want);
                label = want;
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == launch_id {
                    ui.invoke_launch_game();
                } else if ev.id == show_id {
                    instance::activate(std::process::id());
                } else if ev.id == quit_id {
                    let _ = slint::quit_event_loop();
                }
            }
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::DoubleClick { .. } = ev {
                    instance::activate(std::process::id());
                }
            }
        });
        Some(Tray { _icon: tray, _timer: timer })
    }
}

fn wire_settings(ui: &AppWindow, core: &Shared) {
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_save_settings(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        let path = |s: SharedString| {
            let s = s.trim().to_string();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        k.config.factorio_user_dir = path(ui.get_s_user_dir());
        k.config.factorio_exe = path(ui.get_s_exe());
        k.config.username = ui.get_s_username().trim().to_string();
        k.config.token = ui.get_s_token().trim().to_string();
        k.config.target_factorio = ui.get_s_target().trim().to_string();
        k.config.feat_mod_settings = ui.get_s_feat_settings();
        k.config.feat_factorio_version = ui.get_s_feat_version();
        k.config.feat_export = ui.get_s_feat_export();
        k.config.feat_saves = ui.get_s_feat_saves();
        k.config.auto_check_updates = ui.get_s_auto_updates();
        k.config.auto_sync_on_activate = ui.get_s_auto_sync();
        k.config.export_include_mods = ui.get_s_export_mods();
        k.config.copy_back_settings = ui.get_s_copy_back();
        k.config.check_interval_minutes = ui.get_s_interval().trim().parse().unwrap_or(10).clamp(1, 1440);
        k.config.auto_update_app = ui.get_s_auto_app();
        k.config.check_before_launch = ui.get_s_check_launch();
        k.config.cache_sprite_atlas = ui.get_s_sprite_cache();
        k.refresh_factorio();
        k.save_config();
        refresh_settings(&ui, &k);
        refresh_all(&ui, &k);
        ui.set_status("Paramètres enregistrés.".into());
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_detect_factorio(move || {
        let ui = w.unwrap();
        let mut k = c.lock().unwrap();
        k.config.factorio_user_dir = factorio::default_user_dir().filter(|d| d.exists());
        k.config.factorio_exe = factorio::detect_exe(k.config.factorio_user_dir.as_deref());
        k.refresh_factorio();
        k.save_config();
        refresh_settings(&ui, &k);
        ui.set_status("Détection terminée.".into());
    });
    let w = ui.as_weak();
    ui.on_browse_user_dir(move || {
        let ui = w.unwrap();
        if let Some(d) = rfd::FileDialog::new().pick_folder() {
            ui.set_s_user_dir(d.display().to_string().into());
        }
    });
    let w = ui.as_weak();
    ui.on_browse_exe(move || {
        let ui = w.unwrap();
        if let Some(f) = rfd::FileDialog::new().pick_file() {
            ui.set_s_exe(f.display().to_string().into());
        }
    });
    let (c, w) = (core.clone(), ui.as_weak());
    ui.on_install_app_update(move || {
        let ui = w.unwrap();
        let Some(rel) = c.lock().unwrap().app_update.clone() else { return };
        task(&ui, &c, &format!("Téléchargement de la version {}…", rel.version), move |c, w| {
            let dir = c.lock().unwrap().paths.downloads.join("app-update");
            let w2 = w.clone();
            let file = updater::download(&rel, &dir, |done, total| {
                if total > 0 {
                    post_progress(&w2, done as f32 / total as f32);
                }
            })?;
            if updater::install(&file)? {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
                return Ok(format!("Installation de la version {}…", rel.version));
            }
            Ok(format!("Version {} téléchargée : glisse l'application dans Applications.", rel.version))
        });
    });
    let c = core.clone();
    ui.on_open_data_dir(move || {
        let _ = open::that(&c.lock().unwrap().paths.data);
    });
}

#[cfg(test)]
mod tests {
    use super::{library_match, library_rank};

    #[test]
    fn library_match_hints() {
        let packs = vec!["Space Age run".to_string(), "Vanilla+".to_string()];
        let m = |f: &str| library_match(f, "bobinserters", "Bob's Adjustable Inserters", &packs, "Bobingabout").unwrap().1;
        assert_eq!(m("bobins"), "ID : bobinserters");
        assert_eq!(m("inserters"), "", "le titre affiche déjà le mot");
        assert_eq!(m("space"), "Trouvé dans le modpack : Space Age run");
        assert_eq!(m("gabout"), "Auteur : Bobingabout");
    }

    #[test]
    fn library_rank_priorities() {
        let packs = vec!["Space Age run".to_string()];
        let r = |f: &str| library_rank(f, "bobinserters", "Bob's Adjustable Inserters", &packs, "Bobingabout");
        assert_eq!(r(""), Some(0));
        assert_eq!(r("bobinserters"), Some(0));
        assert_eq!(r("bobins"), Some(1));
        assert_eq!(r("inserters"), Some(2));
        assert_eq!(r("adjustable"), Some(4));
        assert_eq!(r("space"), Some(5));
        assert_eq!(r("gabout"), Some(6));
        assert_eq!(r("bob space"), Some(5), "tous les mots doivent correspondre");
        assert_eq!(r("krastorio"), None);
    }
}
