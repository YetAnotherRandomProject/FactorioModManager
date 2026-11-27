//! Instance unique : un fichier verrouillé tant que l'app tourne (le système le libère à la
//! fermeture, même en cas de plantage). Une seconde instance remet la fenêtre existante au
//! premier plan puis quitte.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

pub enum Instance {
    /// Cette instance est la seule : garder le fichier ouvert jusqu'à la fin.
    Primary(fs::File),
    /// Une instance tourne déjà et sa fenêtre a été remise au premier plan.
    Activated,
    /// Une instance tourne déjà, mais sa fenêtre n'a pas pu être affichée.
    Running,
}

pub fn acquire(data: &Path) -> Result<Instance> {
    let lock = data.join("instance.lock");
    let pid_file = data.join("instance.pid");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .context("ouverture du verrou d'instance")?;
    // Après une mise à jour, la nouvelle version peut démarrer avant la fin de l'ancienne
    // (fenêtre déjà fermée) : on attend alors quelques secondes que le verrou se libère.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut tried_activate = false;
    loop {
        match file.try_lock() {
            Ok(()) => {
                let _ = fs::write(&pid_file, std::process::id().to_string());
                return Ok(Instance::Primary(file));
            }
            Err(fs::TryLockError::WouldBlock) => {
                if !tried_activate {
                    tried_activate = true;
                    let pid = fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok());
                    if pid.is_some_and(activate) {
                        return Ok(Instance::Activated);
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(Instance::Running);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e).context("verrou d'instance"),
        }
    }
}

/// Restaure et passe au premier plan la fenêtre principale du processus `pid`.
#[cfg(windows)]
pub fn activate(pid: u32) -> bool {
    win::activate(pid)
}

#[cfg(not(windows))]
pub fn activate(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    type Hwnd = *mut c_void;
    type Bool = i32;
    type Lparam = isize;

    const GW_OWNER: u32 = 4;
    const SW_RESTORE: i32 = 9;
    const SW_SHOW: i32 = 5;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: unsafe extern "system" fn(Hwnd, Lparam) -> Bool, lparam: Lparam) -> Bool;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn IsWindowVisible(hwnd: Hwnd) -> Bool;
        fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
        fn IsIconic(hwnd: Hwnd) -> Bool;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
        fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    }

    struct Search {
        pid: u32,
        found: Hwnd,
    }

    /// Fenêtre principale du processus cherché : visible, sans propriétaire et titrée
    /// (winit crée aussi une fenêtre technique « visible » mais sans titre).
    unsafe extern "system" fn find(hwnd: Hwnd, lparam: Lparam) -> Bool {
        // SAFETY : `lparam` pointe sur la `Search` de `activate`, vivante pendant `EnumWindows`.
        unsafe {
            let search = &mut *(lparam as *mut Search);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == search.pid
                && IsWindowVisible(hwnd) != 0
                && GetWindow(hwnd, GW_OWNER).is_null()
                && GetWindowTextLengthW(hwnd) > 0
            {
                search.found = hwnd;
                return 0;
            }
        }
        1
    }

    pub fn activate(pid: u32) -> bool {
        let mut search = Search { pid, found: std::ptr::null_mut() };
        // SAFETY : appels Win32 sur des handles fournis par EnumWindows ; `search` survit à l'appel.
        unsafe {
            EnumWindows(find, &mut search as *mut Search as Lparam);
            if search.found.is_null() {
                return false;
            }
            let hwnd = search.found;
            ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
            // Autorisé : ce processus vient d'être lancé par l'utilisateur, il a la main sur le premier plan.
            SetForegroundWindow(hwnd);
        }
        true
    }
}
