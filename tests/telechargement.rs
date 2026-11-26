//! Téléchargement contre un serveur HTTP local : reprise après coupure, serveur
//! qui ignore les plages, contenu corrompu, fichier déjà présent.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use beta1_datachain::empreinte;
use beta1_datachain::manifeste::Source;
use beta1_datachain::telechargement::{Options, partiel, telecharger};

#[derive(Clone, Copy)]
enum Comportement {
    Normal,
    /// Première réponse coupée à la moitié du corps.
    Coupe,
    /// Ignore `Range` : toujours 200 et tout le fichier.
    SansPlage,
    /// Un octet changé.
    Corrompu,
}

/// Serveur d'un seul fichier ; renvoie l'adresse et le journal des en-têtes Range.
fn serveur(contenu: Vec<u8>, c: Comportement) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let adresse = format!("http://{}/edition/fichier.bin", l.local_addr().unwrap());
    let journal = Arc::new(Mutex::new(Vec::new()));
    let j = journal.clone();
    std::thread::spawn(move || {
        for (n, flux) in l.incoming().enumerate() {
            let mut flux = flux.unwrap();
            let mut lecteur = BufReader::new(flux.try_clone().unwrap());
            let mut debut = 0usize;
            let mut range = String::new();
            loop {
                let mut ligne = String::new();
                lecteur.read_line(&mut ligne).unwrap();
                let ligne = ligne.trim_end();
                if ligne.is_empty() {
                    break;
                }
                if let Some(v) = ligne.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    range = v.to_owned();
                    debut = v.trim_end_matches('-').parse().unwrap();
                }
            }
            j.lock().unwrap().push(range);
            let mut corps = contenu.clone();
            if matches!(c, Comportement::Corrompu) {
                corps[0] ^= 1;
            }
            let (statut, morceau) = if debut > 0 && !matches!(c, Comportement::SansPlage) {
                let t = contenu.len();
                (format!("206 Partial Content\r\nContent-Range: bytes {debut}-{}/{t}", t - 1), &corps[debut..])
            } else {
                ("200 OK".to_owned(), &corps[..])
            };
            let entete = format!("HTTP/1.1 {statut}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", morceau.len());
            flux.write_all(entete.as_bytes()).unwrap();
            let coupe = matches!(c, Comportement::Coupe | Comportement::SansPlage) && n == 0;
            let envoye = if coupe { &morceau[..morceau.len() / 2] } else { morceau };
            let _ = flux.write_all(envoye);
            let _ = flux.flush();
        }
    });
    (adresse, journal)
}

fn dossier(nom: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("beta1-datachain-{nom}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn contenu() -> Vec<u8> {
    (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect()
}

fn source(url: String, contenu: &[u8]) -> Source {
    let port = url.split(':').nth(2).unwrap_or_default().split('/').next().unwrap_or_default().to_owned();
    let d = std::env::temp_dir().join(format!("beta1-datachain-ref-{}-{port}", std::process::id()));
    std::fs::write(&d, contenu).unwrap();
    let e = empreinte::calculer(&d).unwrap();
    std::fs::remove_file(&d).unwrap();
    Source {
        zone: "D029".into(),
        ressource: "BDTOPO".into(),
        edition: "edition".into(),
        url,
        octets: contenu.len() as u64,
        md5: e.md5,
        sha256: e.sha256,
    }
}

fn options(racine: PathBuf) -> Options {
    let mut o = Options::nouvelles(racine);
    o.essais = 3;
    o.attente = Duration::from_millis(1);
    o.silence = Duration::from_secs(5);
    o.https_seul = false;
    o
}

#[test]
fn reprise_apres_coupure() {
    let c = contenu();
    let (url, journal) = serveur(c.clone(), Comportement::Coupe);
    let s = source(url, &c);
    let racine = dossier("coupe");
    let b = telecharger(std::slice::from_ref(&s), &options(racine.clone()));
    assert_eq!((b.recus, b.echecs.len()), (1, 0), "{:?}", b.echecs);
    assert_eq!(std::fs::read(s.chemin(&racine)).unwrap(), c);
    assert!(!partiel(&s.chemin(&racine)).exists());
    let j = journal.lock().unwrap().clone();
    assert_eq!(j, vec![String::new(), format!("{}-", c.len() / 2)], "la reprise doit partir de la coupure");

    // Deuxième appel : déjà présent, aucune requête.
    let b = telecharger(std::slice::from_ref(&s), &options(racine.clone()));
    assert_eq!((b.recus, b.presents), (0, 1));
    assert_eq!(journal.lock().unwrap().len(), 2);
    std::fs::remove_dir_all(racine).unwrap();
}

#[test]
fn serveur_sans_plage() {
    let c = contenu();
    let (url, _) = serveur(c.clone(), Comportement::SansPlage);
    let s = source(url, &c);
    let racine = dossier("sansplage");
    let b = telecharger(std::slice::from_ref(&s), &options(racine.clone()));
    assert_eq!((b.recus, b.echecs.len()), (1, 0), "{:?}", b.echecs);
    assert_eq!(std::fs::read(s.chemin(&racine)).unwrap(), c);
    std::fs::remove_dir_all(racine).unwrap();
}

#[test]
fn contenu_corrompu_refuse() {
    let c = contenu();
    let (url, journal) = serveur(c.clone(), Comportement::Corrompu);
    let s = source(url, &c);
    let racine = dossier("corrompu");
    let b = telecharger(std::slice::from_ref(&s), &options(racine.clone()));
    assert_eq!((b.recus, b.echecs.len()), (0, 1));
    assert!(b.echecs[0].1.contains("MD5"), "{}", b.echecs[0].1);
    assert!(!s.chemin(&racine).exists() && !partiel(&s.chemin(&racine)).exists());
    assert_eq!(journal.lock().unwrap().len(), 3, "trois essais");
    std::fs::remove_dir_all(racine).unwrap();
}

#[test]
fn normal_et_https_exige() {
    let c = contenu();
    let (url, _) = serveur(c.clone(), Comportement::Normal);
    let s = source(url, &c);
    let racine = dossier("normal");
    // Par défaut, HTTPS seul : une URL http est refusée sans être demandée.
    let mut o = options(racine.clone());
    o.https_seul = true;
    let b = telecharger(std::slice::from_ref(&s), &o);
    assert_eq!(b.echecs.len(), 1);
    let b = telecharger(std::slice::from_ref(&s), &options(racine.clone()));
    assert_eq!((b.recus, b.echecs.len()), (1, 0), "{:?}", b.echecs);
    std::fs::remove_dir_all(racine).unwrap();
}
