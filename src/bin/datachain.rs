//! `datachain` : une publication complète de tiles, de bout en bout (Job du
//! chart `game`, H-06). Voir `beta1_datachain::orchestre` pour les volumes.
//!
//! ```text
//! datachain --sources /donnees/sources --tiles /donnees/tiles --publication <id> [--zones D029,D974]
//! ```
//!
//! Idempotent : si `<tiles>/<id>/COMPLET` existe, sort aussitôt ; sinon reprend
//! où il s'était arrêté. Codes de sortie : 0 publication complète, 1 échec.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use beta1_datachain::erreur::{Contexte, Erreur, Res};
use beta1_datachain::manifeste::{self, Source};
use beta1_datachain::nettoyage;
use beta1_datachain::orchestre::{self, Edition, Plan};
use beta1_datachain::telechargement::{self, Options};
use clap::Parser;

/// Manifeste embarqué : le digest de l'image couvre les sources.
const MANIFESTE: &str = include_str!("../../sources/manifeste.csv");

#[derive(Parser)]
#[command(version, about = "Publication complète des tiles de La Bataille de France")]
struct Cli {
    /// Volume des sources (et des dérivés).
    #[arg(long)]
    sources: PathBuf,
    /// Volume des tiles.
    #[arg(long)]
    tiles: PathBuf,
    /// Nom de la publication (`[a-z0-9-]`), donné par le chart.
    #[arg(long)]
    publication: String,
    /// Départements (toute la France par défaut).
    #[arg(long, value_delimiter = ',')]
    zones: Vec<String>,
    /// Processus `chaine` en parallèle.
    #[arg(long, default_value_t = 2)]
    processus: usize,
    /// Threads par processus.
    #[arg(long, default_value_t = 8)]
    threads: usize,
    /// Ne rien télécharger : les sources doivent déjà être là.
    #[arg(long)]
    sans_telechargement: bool,
    /// Publications complètes gardées en plus de la courante (les plus
    /// récentes) ; les autres sont supprimées une fois celle-ci complète.
    #[arg(long, default_value_t = 1)]
    garder: usize,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let t0 = Instant::now();
    match executer(&cli) {
        Ok(()) => {
            journal(&format!("publication {} complète en {:.0} s", cli.publication, t0.elapsed().as_secs_f64()));
            ExitCode::SUCCESS
        }
        Err(e) => {
            journal(&format!("ÉCHEC : {e}"));
            ExitCode::from(1)
        }
    }
}

fn journal(m: &str) {
    println!("[datachain] {m}");
}

fn executer(cli: &Cli) -> Res<()> {
    if !orchestre::publication_valide(&cli.publication) {
        return Err("--publication : [a-z0-9-], 1 à 64 caractères".into());
    }
    let fin = cli.tiles.join(&cli.publication);
    if fin.join("COMPLET").exists() {
        journal(&format!("{} déjà publiée", fin.display()));
        nettoyer(cli);
        return Ok(());
    }
    let travail = cli.tiles.join(format!("{}.partiel", cli.publication));
    for d in ["national", "departements", "journaux"] {
        fs::create_dir_all(travail.join(d)).ctx(travail.display())?;
    }

    // 1. Sources choisies : départements demandés et sources nationales.
    let toutes = manifeste::lire(MANIFESTE).map_err(|e| Erreur(e.to_string()))?;
    let choisies: Vec<Source> = toutes
        .into_iter()
        .filter(|s| s.zone == "FRANCE" || cli.zones.is_empty() || cli.zones.contains(&s.zone))
        .collect();
    if let Some(z) = cli.zones.iter().find(|z| !choisies.iter().any(|s| &s.zone == *z)) {
        return Err(format!("{z} absent du manifeste").into());
    }
    let octets: u64 = choisies.iter().map(|s| s.octets).sum();
    journal(&format!("{} fichiers de sources, {:.1} Go", choisies.len(), octets as f64 / 1e9));
    if cli.sans_telechargement {
        if let Some(s) = choisies.iter().find(|s| !s.chemin(&cli.sources).exists()) {
            return Err(format!("source absente : {}", s.chemin(&cli.sources).display()).into());
        }
    } else {
        let b = telechargement::telecharger(&choisies, &Options::nouvelles(cli.sources.clone()));
        journal(&format!("sources : {} reçues, {} déjà là, {} échecs", b.recus, b.presents, b.echecs.len()));
        if !b.echecs.is_empty() {
            return Err("téléchargement incomplet (relancer : la reprise est automatique)".into());
        }
    }
    let plan = orchestre::planifier(&choisies, &cli.sources).map_err(Erreur)?;

    // 2. Dérivés, réutilisés d'une publication à l'autre.
    let chaine = binaire("chaine")?;
    let journaux = travail.join("journaux");
    parallele(cli.processus, &plan.extractions, |e| {
        lancer(
            &chaine,
            &["extraire".into(), "--source".into(), arg(&e.fichiers[0]), "--sortie".into(), arg(&e.sortie)],
            e,
            &journaux,
            1,
        )
    })?;
    parallele(cli.processus, &plan.reliefs, |e| {
        let mut a = vec!["relief".into(), "--source".into()];
        a.extend(e.fichiers.iter().map(|f| arg(f)));
        a.extend(["--sortie".into(), arg(&e.sortie)]);
        lancer(&chaine, &a, e, &journaux, cli.threads)
    })?;
    let insee1k = extraire_insee1k(&plan, &cli.sources)?;

    // 3. Référence nationale de chaque zone (communes, côtes).
    let mut zones: Vec<u8> = plan.departements.values().map(|d| d.0).collect();
    zones.sort();
    zones.dedup();
    for z in &zones {
        let sortie = travail.join(format!("national/z{z}.bin"));
        if sortie.exists() {
            continue;
        }
        let mut a = vec!["national".into(), "--source".into()];
        a.extend(plan.departements.values().filter(|d| d.0 == *z).map(|d| arg(&d.1)));
        let partiel = sortie.with_extension("partiel");
        a.extend(["--sortie".into(), arg(&partiel)]);
        executer_commande(&chaine, &a, &journaux.join(format!("national-z{z}.log")), cli.threads)?;
        fs::rename(&partiel, &sortie).ctx(sortie.display())?;
        journal(&format!("national z{z} : fait"));
    }

    // 4. Départements : un processus chacun.
    let depts: Vec<(&String, &(u8, PathBuf))> = plan.departements.iter().collect();
    parallele(cli.processus, &depts, |(d, (z, gpkg))| {
        let sortie = travail.join("departements").join(d);
        if sortie.join("controles.txt").exists() {
            return Ok(());
        }
        let partiel = travail.join("departements").join(format!("{d}.partiel"));
        let _ = fs::remove_dir_all(&partiel);
        let mut a: Vec<String> = vec![
            "departement".into(),
            "--gpkg".into(),
            arg(gpkg),
            "--dept".into(),
            orchestre::code_dept(d),
            "--zone".into(),
            z.to_string(),
            "--national".into(),
            arg(&travail.join(format!("national/z{z}.bin"))),
            "--relief".into(),
            arg(&cli.sources.join("derives/relief")),
            "--sortie".into(),
            arg(&partiel),
            "--etiquette".into(),
            d.to_string(),
        ];
        if *z == 0 {
            let p = plan.insee200.as_ref().ctx("carreaux de 200 m absents du manifeste")?;
            a.extend(["--insee200".into(), arg(p)]);
        } else if let Some(f) = orchestre::carreaux_1km(d) {
            let p = insee1k.as_ref().ctx("carreaux de 1 km absents du manifeste")?;
            a.extend(["--insee1k".into(), arg(&p.join(f))]);
        }
        let t = Instant::now();
        executer_commande(&chaine, &a, &journaux.join(format!("{d}.log")), cli.threads)?;
        fs::rename(&partiel, &sortie).ctx(sortie.display())?;
        journal(&format!("{d} : {:.0} s, empreinte {}", t.elapsed().as_secs_f64(), empreinte(&sortie)?));
        Ok(())
    })?;

    // 5. Contrôle croisé : décodage, doublons, raccords entre départements.
    let mut a = vec!["controle".into()];
    a.extend(plan.departements.keys().map(|d| arg(&travail.join("departements").join(d))));
    executer_commande(&chaine, &a, &journaux.join("controle.log"), cli.threads)
        .map_err(|e| Erreur(format!("contrôle non conforme ({e}) : voir journaux/controle.log")))?;
    journal("contrôle : CONFORME");

    // 6. Graphe routier national, ordre CCH compris : un seul processus, tous les threads.
    let graphe = travail.join("graphe.bin");
    if !graphe.exists() {
        let mut a = vec!["graphe".into(), "--sortie".into(), arg(&graphe)];
        a.extend(plan.departements.keys().map(|d| arg(&travail.join("departements").join(d))));
        let t = Instant::now();
        executer_commande(&chaine, &a, &journaux.join("graphe.log"), cli.processus * cli.threads)?;
        journal(&format!("graphe : {:.0} s", t.elapsed().as_secs_f64()));
    }
    let b = fs::read(&graphe).ctx(graphe.display())?;
    let empreinte_graphe = blake3::hash(&b).to_hex()[..16].to_string();
    drop(b);

    // 7. Manifeste de la publication, marqueur, renommage.
    let mut m = format!("publication {}\nversion {}\n", cli.publication, env!("CARGO_PKG_VERSION"));
    for d in plan.departements.keys() {
        m.push_str(&format!("{d} {}\n", empreinte(&travail.join("departements").join(d))?));
    }
    m.push_str(&format!("graphe {empreinte_graphe}\n"));
    fs::write(travail.join("MANIFESTE"), m).ctx("MANIFESTE")?;
    fs::write(travail.join("COMPLET"), "").ctx("COMPLET")?;
    fs::rename(&travail, &fin).ctx(fin.display())?;
    nettoyer(cli);
    Ok(())
}

/// Supprime les anciennes publications ; une erreur est signalée sans faire
/// échouer la publication, déjà complète.
fn nettoyer(cli: &Cli) {
    let b = nettoyage::nettoyer(&cli.tiles, &cli.publication, cli.garder, std::time::SystemTime::now());
    for n in &b.supprimees {
        journal(&format!("ancienne publication supprimée : {n}"));
    }
    for e in &b.erreurs {
        journal(&format!("nettoyage : {e}"));
    }
}

fn arg(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Binaire voisin de celui-ci (même image, même dossier).
fn binaire(nom: &str) -> Res<PathBuf> {
    let ici = std::env::current_exe().ctx("chemin du binaire")?;
    let b = ici.parent().ctx("dossier du binaire")?.join(nom);
    if b.exists() { Ok(b) } else { Err(format!("{} introuvable", b.display()).into()) }
}

/// Lance `chaine` pour une édition, sauf si son résultat existe.
fn lancer(chaine: &Path, a: &[String], e: &Edition, journaux: &Path, threads: usize) -> Res<()> {
    if e.sortie.exists() {
        return Ok(());
    }
    if let Some(d) = e.sortie.parent() {
        fs::create_dir_all(d).ctx(d.display())?;
    }
    let t = Instant::now();
    executer_commande(chaine, a, &journaux.join(format!("{}.log", e.nom)), threads)?;
    journal(&format!("{} : {:.0} s", e.nom, t.elapsed().as_secs_f64()));
    Ok(())
}

fn executer_commande(chaine: &Path, a: &[String], log: &Path, threads: usize) -> Res<()> {
    let f = fs::File::create(log).ctx(log.display())?;
    let f2 = f.try_clone().ctx(log.display())?;
    let s = Command::new(chaine)
        .arg("--threads")
        .arg(threads.to_string())
        .args(a)
        // Sans ces réglages, l'allocateur garde 22 Go au lieu de 7,5 (Manche, P-26).
        .env("MALLOC_MMAP_THRESHOLD_", "262144")
        .env("MALLOC_TRIM_THRESHOLD_", "1048576")
        .env("MALLOC_ARENA_MAX", "8")
        .stdin(Stdio::null())
        .stdout(f)
        .stderr(f2)
        .status()
        .ctx(chaine.display())?;
    if s.success() { Ok(()) } else { Err(format!("{} ({s}), journal {}", a[0], log.display()).into()) }
}

/// Exécute `f` sur chaque élément, `n` à la fois ; la première erreur arrête la suite.
fn parallele<T: Sync>(n: usize, v: &[T], f: impl Fn(&T) -> Res<()> + Sync) -> Res<()> {
    let file = Mutex::new(v.iter());
    let erreur: Mutex<Option<Erreur>> = Mutex::new(None);
    std::thread::scope(|p| {
        for _ in 0..n.max(1) {
            p.spawn(|| {
                loop {
                    if erreur.lock().map_or(true, |e| e.is_some()) {
                        return;
                    }
                    let Some(x) = file.lock().ok().and_then(|mut i| i.next()) else { return };
                    if let Err(e) = f(x)
                        && let Ok(mut g) = erreur.lock()
                    {
                        g.get_or_insert(e);
                    }
                }
            });
        }
    });
    match erreur.into_inner() {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err("verrou empoisonné".into()),
    }
}

/// Extrait les CSV des carreaux de 1 km du zip de l'INSEE, une fois.
fn extraire_insee1k(plan: &Plan, sources: &Path) -> Res<Option<PathBuf>> {
    let Some(z) = &plan.insee1k_zip else { return Ok(None) };
    let dossier = sources.join("derives/insee");
    let besoin: Vec<&str> = plan.departements.keys().filter_map(|d| orchestre::carreaux_1km(d)).collect();
    if besoin.iter().all(|f| dossier.join(f).exists()) {
        return Ok(Some(dossier));
    }
    fs::create_dir_all(&dossier).ctx(dossier.display())?;
    let mut a = zip::ZipArchive::new(fs::File::open(z).ctx(z.display())?).ctx(z.display())?;
    for f in besoin {
        let mut e = a.by_name(f).ctx(format!("{} : {f}", z.display()))?;
        let partiel = dossier.join(format!("{f}.partiel"));
        let mut o = fs::File::create(&partiel).ctx(partiel.display())?;
        std::io::copy(&mut e, &mut o).ctx(partiel.display())?;
        fs::rename(&partiel, dossier.join(f)).ctx(f)?;
    }
    Ok(Some(dossier))
}

fn empreinte(dossier: &Path) -> Res<String> {
    let c = fs::read_to_string(dossier.join("controles.txt")).ctx(dossier.display())?;
    c.lines()
        .find_map(|l| l.strip_prefix("empreinte ").map(str::to_owned))
        .ctx(format!("{} : pas d'empreinte", dossier.display()))
}
