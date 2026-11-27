//! Journal de l'application : un fichier texte dans le dossier de données, consultable dans l'app
//! (téléchargements, synchronisations, lancements, erreurs).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Taille au-delà de laquelle le journal est archivé (`journal.old.log`) et recommencé.
const MAX_SIZE: u64 = 1024 * 1024;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static LOCK: Mutex<()> = Mutex::new(());

pub fn init(path: PathBuf) {
    let _ = PATH.set(path);
}

pub fn path() -> Option<PathBuf> {
    PATH.get().cloned()
}

/// Ajoute une ligne horodatée (heure locale).
pub fn log(msg: impl AsRef<str>) {
    let Some(path) = PATH.get() else { return };
    let msg = msg.as_ref().trim();
    if msg.is_empty() {
        return;
    }
    let _guard = LOCK.lock();
    if fs::metadata(path).map(|m| m.len() > MAX_SIZE).unwrap_or(false) {
        let _ = fs::rename(path, path.with_file_name("journal.old.log"));
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else { return };
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    // Une entrée par ligne : les messages sur plusieurs lignes sont indentés.
    let body = msg.lines().collect::<Vec<_>>().join("\n                     ");
    let _ = writeln!(f, "{now}  {body}");
}

/// Dernières lignes du journal, la plus récente en dernier.
pub fn tail(max_lines: usize) -> String {
    let Some(path) = PATH.get() else { return String::new() };
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max_lines)..].join("\n")
}

pub fn clear() {
    if let Some(path) = PATH.get() {
        let _guard = LOCK.lock();
        let _ = fs::write(path, "");
    }
}
