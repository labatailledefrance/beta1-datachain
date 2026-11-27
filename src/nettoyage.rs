//! Nettoyage des anciennes publications du volume des tiles.
//!
//! Une publication est adressée par ses entrées : chaque nouvelle image de la
//! chaîne en produit une autre (France entière : ~130 Go), et rien ne
//! supprimait les anciennes. Règle, appliquée par le Job une fois sa
//! publication complète :
//!
//! - la publication **courante** est gardée ;
//! - les `garder` publications complètes les plus récentes (date de `COMPLET`)
//!   aussi : les pods de la version précédente les servent encore pendant la
//!   bascule, comme un rolling update garde l'ancien ReplicaSet ;
//! - les autres publications complètes sont supprimées ;
//! - un `<id>.partiel` étranger est supprimé s'il n'a rien écrit depuis
//!   [`INACTIF`] : une production en cours (autre Job) n'est jamais touchée.
//!
//! Seuls les dossiers dont le nom est un identifiant valide sont considérés ;
//! les liens symboliques ne sont jamais suivis.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::orchestre::publication_valide;

/// Durée sans écriture au-delà de laquelle un `.partiel` est abandonné.
pub const INACTIF: Duration = Duration::from_secs(24 * 3600);

/// Ce que le nettoyage a supprimé (noms de dossiers) et les erreurs rencontrées.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Bilan {
    pub supprimees: Vec<String>,
    pub erreurs: Vec<String>,
}

/// Date de modification la plus récente sous `d` (le dossier compris), sans
/// suivre les liens.
fn derniere_ecriture(d: &Path) -> SystemTime {
    let mut t = fs::symlink_metadata(d).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(es) = fs::read_dir(d) {
        for e in es.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                t = t.max(derniere_ecriture(&e.path()));
            } else if let Ok(x) = m.modified() {
                t = t.max(x);
            }
        }
    }
    t
}

/// Nettoie `racine` (volume des tiles) en gardant `courante` et les `garder`
/// autres publications complètes les plus récentes. `maintenant` est passé
/// pour les tests.
pub fn nettoyer(racine: &Path, courante: &str, garder: usize, maintenant: SystemTime) -> Bilan {
    let mut bilan = Bilan::default();
    let Ok(entrees) = fs::read_dir(racine) else {
        bilan.erreurs.push(format!("{} illisible", racine.display()));
        return bilan;
    };
    let mut completes: Vec<(SystemTime, String)> = Vec::new();
    let mut partiels: Vec<String> = Vec::new();
    for e in entrees.flatten() {
        // Dossiers seulement, jamais un lien.
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(nom) = e.file_name().to_str().map(str::to_owned) else { continue };
        if let Some(id) = nom.strip_suffix(".partiel") {
            if publication_valide(id) && id != courante {
                partiels.push(nom);
            }
        } else if publication_valide(&nom) && nom != courante {
            match fs::metadata(e.path().join("COMPLET")).and_then(|m| m.modified()) {
                Ok(t) => completes.push((t, nom)),
                // Sans marqueur : pas une publication du Job, on n'y touche pas.
                Err(_) => continue,
            }
        }
    }
    // Plus récentes d'abord ; à date égale, ordre des noms (déterministe).
    completes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut a_supprimer: Vec<String> = completes.into_iter().skip(garder).map(|c| c.1).collect();
    for p in partiels {
        let t = derniere_ecriture(&racine.join(&p));
        if maintenant.duration_since(t).is_ok_and(|d| d >= INACTIF) {
            a_supprimer.push(p);
        }
    }
    a_supprimer.sort();
    for nom in a_supprimer {
        match fs::remove_dir_all(racine.join(&nom)) {
            Ok(()) => bilan.supprimees.push(nom),
            Err(e) => bilan.erreurs.push(format!("{nom} : {e}")),
        }
    }
    bilan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn vieillir(chemin: &Path, il_y_a: Duration, maintenant: SystemTime) {
        let t = maintenant - il_y_a;
        File::open(chemin).and_then(|f| f.set_modified(t)).unwrap();
    }

    fn publication(racine: &Path, id: &str, il_y_a_h: u64, maintenant: SystemTime) {
        fs::create_dir_all(racine.join(id).join("departements/D029")).unwrap();
        fs::write(racine.join(id).join("COMPLET"), "").unwrap();
        vieillir(&racine.join(id).join("COMPLET"), Duration::from_secs(il_y_a_h * 3600), maintenant);
    }

    #[test]
    fn garde_courante_et_precedente() {
        let r = std::env::temp_dir().join(format!("beta1-nettoyage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(&r).unwrap();
        let maintenant = SystemTime::now();
        publication(&r, "ancienne", 72, maintenant);
        publication(&r, "precedente", 2, maintenant);
        publication(&r, "plus-ancienne", 96, maintenant);
        publication(&r, "courante", 0, maintenant);
        // Production abandonnée (rien depuis deux jours) et production en cours.
        for p in ["abandon.partiel", "en-cours.partiel", "courante.partiel"] {
            fs::create_dir_all(r.join(p).join("journaux")).unwrap();
            fs::write(r.join(p).join("journaux/a.log"), "x").unwrap();
        }
        for f in ["abandon.partiel/journaux/a.log", "abandon.partiel/journaux", "abandon.partiel"] {
            vieillir(&r.join(f), Duration::from_secs(48 * 3600), maintenant);
        }
        // Ni marqueur ni nom valide : on n'y touche pas.
        fs::create_dir_all(r.join("sans-marqueur")).unwrap();
        fs::create_dir_all(r.join("Autre.Dossier")).unwrap();
        // Un lien vers une publication n'est pas suivi.
        std::os::unix::fs::symlink(r.join("precedente"), r.join("lien")).unwrap();

        let b = nettoyer(&r, "courante", 1, maintenant);
        assert_eq!(b.erreurs, Vec::<String>::new());
        assert_eq!(b.supprimees, ["abandon.partiel", "ancienne", "plus-ancienne"]);
        for reste in
            ["courante", "precedente", "en-cours.partiel", "courante.partiel", "sans-marqueur", "Autre.Dossier"]
        {
            assert!(r.join(reste).exists(), "{reste} supprimé");
        }
        assert!(r.join("lien").symlink_metadata().is_ok());
        // Relancé : plus rien à faire.
        assert_eq!(nettoyer(&r, "courante", 1, maintenant), Bilan::default());
        // garder = 0 : seule la courante reste.
        assert_eq!(nettoyer(&r, "courante", 0, maintenant).supprimees, ["precedente"]);
        fs::remove_dir_all(&r).unwrap();
    }
}
