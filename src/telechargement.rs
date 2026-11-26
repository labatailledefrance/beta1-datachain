//! Téléchargement des sources, avec reprise et contrôle d'intégrité.
//!
//! Le fichier `x` est reçu dans `x.partiel` (reprise par plage HTTP), vérifié
//! (taille, MD5, SHA-256 du manifeste), puis renommé : un fichier présent sous
//! son nom définitif est toujours un fichier vérifié. Comme un `docker pull` :
//! couche reçue, digest contrôlé, puis seulement rendue visible.
//!
//! Deux connexions au plus par défaut : l'IGN ralentit (502, 504) au-delà.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ureq::Agent;

use crate::empreinte;
use crate::manifeste::Source;

pub const AGENT_HTTP: &str =
    concat!("beta1-datachain/", env!("CARGO_PKG_VERSION"), " (+https://github.com/labatailledefrance/beta1-datachain)");

#[derive(Debug, Clone)]
pub struct Options {
    pub racine: PathBuf,
    pub connexions: usize,
    pub essais: u32,
    /// Attente après le premier échec, doublée à chaque essai.
    pub attente: Duration,
    pub attente_max: Duration,
    /// Silence maximal d'une connexion avant de la couper et de reprendre.
    pub silence: Duration,
    /// HTTPS seul, redirections comprises (faux seulement pour les tests).
    pub https_seul: bool,
}

impl Options {
    pub fn nouvelles(racine: PathBuf) -> Self {
        Options {
            racine,
            connexions: 2,
            essais: 20,
            attente: Duration::from_secs(10),
            attente_max: Duration::from_secs(600),
            silence: Duration::from_secs(120),
            https_seul: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Bilan {
    /// Fichiers reçus et vérifiés pendant cet appel.
    pub recus: usize,
    /// Fichiers déjà présents (vérifiés lors d'un appel précédent).
    pub presents: usize,
    /// Fichiers abandonnés après tous les essais, avec la dernière erreur.
    pub echecs: Vec<(PathBuf, String)>,
    pub octets: u64,
}

/// Télécharge les sources manquantes sous `options.racine`.
pub fn telecharger(sources: &[Source], options: &Options) -> Bilan {
    let agent: Agent = Agent::config_builder()
        .user_agent(AGENT_HTTP)
        .https_only(options.https_seul)
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .timeout_per_read(Some(options.silence))
        .build()
        .into();
    let file = Mutex::new(sources.iter());
    let bilan = Mutex::new(Bilan::default());
    let octets = AtomicU64::new(0);
    let debut = Instant::now();
    std::thread::scope(|portee| {
        for _ in 0..options.connexions.max(1) {
            portee.spawn(|| {
                loop {
                    let Some(s) = file.lock().ok().and_then(|mut f| f.next()) else { return };
                    let r = un_fichier(&agent, s, options, &octets);
                    let Ok(mut b) = bilan.lock() else { return };
                    match r {
                        Ok(Etat::Present) => b.presents += 1,
                        Ok(Etat::Recu) => {
                            b.recus += 1;
                            let n = octets.load(Ordering::Relaxed);
                            let debit = n as f64 / debut.elapsed().as_secs_f64().max(1.0) / 1e6;
                            println!(
                                "ok {} ({:.0} Mo, {debit:.1} Mo/s)",
                                s.chemin(&options.racine).display(),
                                s.octets as f64 / 1e6
                            );
                        }
                        Err(e) => {
                            eprintln!("échec {} : {e}", s.chemin(&options.racine).display());
                            b.echecs.push((s.chemin(&options.racine), e));
                        }
                    }
                }
            });
        }
    });
    let mut b = bilan.into_inner().unwrap_or_default();
    b.octets = octets.load(Ordering::Relaxed);
    b
}

enum Etat {
    Present,
    Recu,
}

fn un_fichier(agent: &Agent, s: &Source, o: &Options, compteur: &AtomicU64) -> Result<Etat, String> {
    let but = s.chemin(&o.racine);
    if but.exists() {
        return Ok(Etat::Present);
    }
    let dossier = but.parent().ok_or("chemin sans dossier")?;
    fs::create_dir_all(dossier).map_err(|e| format!("{}: {e}", dossier.display()))?;
    let partiel = partiel(&but);
    let mut derniere = String::new();
    for essai in 0..o.essais {
        if essai > 0 {
            let attente = o.attente.saturating_mul(1 << (essai - 1).min(16)).min(o.attente_max);
            eprintln!("essai {essai} {} dans {} s : {derniere}", but.display(), attente.as_secs());
            std::thread::sleep(attente);
        }
        match recevoir(agent, s, &partiel, compteur) {
            Ok(()) => {}
            Err(Echec::Reprendre(e)) => {
                derniere = e;
                continue;
            }
            Err(Echec::Recommencer(e)) => {
                let _ = fs::remove_file(&partiel);
                derniere = e;
                continue;
            }
        }
        match verifier(s, &partiel) {
            Ok(()) => {
                fs::rename(&partiel, &but).map_err(|e| format!("renommage : {e}"))?;
                return Ok(Etat::Recu);
            }
            Err(e) => {
                let _ = fs::remove_file(&partiel);
                derniere = e;
            }
        }
    }
    Err(derniere)
}

/// `x.7z` → `x.7z.partiel`.
pub fn partiel(but: &Path) -> PathBuf {
    let mut p = but.as_os_str().to_owned();
    p.push(".partiel");
    PathBuf::from(p)
}

enum Echec {
    /// Coupure : on garde ce qui est reçu.
    Reprendre(String),
    /// Fichier partiel inutilisable : on repart de zéro.
    Recommencer(String),
}

fn recevoir(agent: &Agent, s: &Source, partiel: &Path, compteur: &AtomicU64) -> Result<(), Echec> {
    let deja = fs::metadata(partiel).map(|m| m.len()).unwrap_or(0);
    if deja > s.octets {
        return Err(Echec::Recommencer(format!("partiel de {deja} octets, plus grand que prévu")));
    }
    if deja == s.octets {
        return Ok(());
    }
    let mut requete = agent.get(&s.url);
    if deja > 0 {
        requete = requete.header("Range", format!("bytes={deja}-"));
    }
    let mut reponse = requete.call().map_err(|e| Echec::Reprendre(e.to_string()))?;
    let reprise = match reponse.status().as_u16() {
        206 => {
            // Le serveur doit reprendre exactement là où on s'est arrêté.
            let plage = reponse.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or_default();
            let attendu = format!("bytes {deja}-{}/{}", s.octets - 1, s.octets);
            if plage != attendu {
                return Err(Echec::Recommencer(format!("Content-Range « {plage} », attendu « {attendu} »")));
            }
            true
        }
        200 => false,
        c => return Err(Echec::Reprendre(format!("statut HTTP {c}"))),
    };
    let mut f = if reprise { OpenOptions::new().append(true).open(partiel) } else { File::create(partiel) }
        .map_err(|e| Echec::Reprendre(format!("{}: {e}", partiel.display())))?;
    let reste = s.octets - if reprise { deja } else { 0 };
    // Jamais plus que la taille du manifeste (plus un octet, pour détecter un
    // fichier trop long) : un serveur ne remplit pas le disque.
    let mut corps = reponse.body_mut().as_reader().take(reste + 1);
    let mut tampon = vec![0u8; 1 << 20];
    loop {
        let n = match corps.read(&mut tampon) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Echec::Reprendre(format!("réception : {e}"))),
        };
        f.write_all(&tampon[..n]).map_err(|e| Echec::Reprendre(format!("écriture : {e}")))?;
        compteur.fetch_add(n as u64, Ordering::Relaxed);
    }
    f.sync_all().map_err(|e| Echec::Reprendre(format!("écriture : {e}")))?;
    Ok(())
}

/// Taille, puis empreintes du manifeste.
pub fn verifier(s: &Source, chemin: &Path) -> Result<(), String> {
    let taille = fs::metadata(chemin).map_err(|e| format!("{}: {e}", chemin.display()))?.len();
    if taille != s.octets {
        return Err(format!("taille {taille} au lieu de {}", s.octets));
    }
    let e = empreinte::calculer(chemin).map_err(|e| format!("{}: {e}", chemin.display()))?;
    if !s.md5.is_empty() && e.md5 != s.md5 {
        return Err(format!("MD5 {} au lieu de {}", e.md5, s.md5));
    }
    if !s.sha256.is_empty() && e.sha256 != s.sha256 {
        return Err(format!("SHA-256 {} au lieu de {}", e.sha256, s.sha256));
    }
    Ok(())
}
