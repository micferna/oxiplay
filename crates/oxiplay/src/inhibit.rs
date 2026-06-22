//! Inhibition de la mise en veille / de l'économiseur d'écran pendant la
//! lecture vidéo (pour que l'écran ne s'éteigne pas en plein film).
//!
//! Sous Linux, on s'appuie sur `systemd-inhibit` (présent sur la quasi-totalité
//! des bureaux modernes) : la commande maintient un verrou tant que son
//! processus enfant vit. On garde donc un enfant `sleep` vivant pendant la
//! lecture et on le tue à la pause/l'arrêt. Aucune dépendance externe.
//!
//! Sur les autres plateformes, l'implémentation est pour l'instant neutre
//! (Windows : `SetThreadExecutionState` ; macOS : `IOPMAssertion` — à venir).

/// Garde un verrou d'inhibition de veille tant qu'il est actif.
#[derive(Default)]
pub struct Inhibitor {
    /// Processus `systemd-inhibit` maintenu en vie pendant la lecture.
    child: Option<std::process::Child>,
}

impl Inhibitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Active ou désactive l'inhibition (idempotent : ne fait rien si l'état
    /// demandé est déjà en place).
    pub fn set(&mut self, active: bool) {
        if active {
            self.inhibit();
        } else {
            self.release();
        }
    }

    fn inhibit(&mut self) {
        if self.child.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            use std::process::{Command, Stdio};
            // `--mode=block` empêche réellement la veille/l'extinction d'écran ;
            // l'enfant `sleep` très long maintient le verrou jusqu'à son kill.
            match Command::new("systemd-inhibit")
                .args([
                    "--what=idle:sleep",
                    "--who=OxiPlay",
                    "--why=Lecture vidéo en cours",
                    "--mode=block",
                    "sleep",
                    "2147483647",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => self.child = Some(child),
                Err(e) => log::debug!("inhibition de veille indisponible : {e}"),
            }
        }
    }

    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.release();
    }
}
