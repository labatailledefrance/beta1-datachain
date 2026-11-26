//! `sources` : manifeste des sources, inventaire et téléchargement.
//!
//! ```text
//! sources inventaire                       régénère sources/manifeste.csv depuis l'IGN
//! sources telecharger --destination DIR    télécharge ce qui manque (reprise, vérification)
//! sources verifier --destination DIR       recalcule les empreintes des fichiers présents
//! sources empreintes --destination DIR     ajoute au manifeste les SHA-256 des fichiers vérifiés
//! ```
//!
//! Filtres communs : `--zones D029,FRANCE`, `--ressources BDTOPO,INSEE`,
//! `--editions _5M_` (morceau du nom de l'édition).
//!
//! Codes de sortie : 0 tout est là et vérifié ; 1 fichiers manquants ou faux ;
//! 2 erreur d'usage ou de manifeste.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use beta1_datachain::manifeste::{self, Source};
use beta1_datachain::telechargement::{self, Options};
use beta1_datachain::{empreinte, inventaire};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Manifeste, inventaire et téléchargement des sources de La Bataille de France")]
struct Cli {
    /// Manifeste des sources.
    #[arg(long, global = true, default_value = "sources/manifeste.csv")]
    manifeste: PathBuf,
    #[command(subcommand)]
    commande: Commande,
}

#[derive(Subcommand)]
enum Commande {
    /// Régénère le manifeste depuis les flux de l'IGN (garde INSEE et SHA-256 connus).
    Inventaire {
        /// Zones à inventorier (toutes par défaut) ; les autres lignes sont gardées.
        #[arg(long, value_delimiter = ',')]
        zones: Vec<String>,
    },
    /// Télécharge les fichiers manquants, avec reprise et vérification.
    Telecharger {
        #[command(flatten)]
        choix: Choix,
        /// Connexions simultanées (l'IGN ralentit au-delà de deux).
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=4))]
        connexions: u8,
    },
    /// Recalcule taille et empreintes des fichiers présents.
    Verifier {
        #[command(flatten)]
        choix: Choix,
    },
    /// Calcule le SHA-256 des fichiers présents dont le MD5 est bon, et l'écrit au manifeste.
    Empreintes {
        #[command(flatten)]
        choix: Choix,
    },
}

#[derive(Args)]
struct Choix {
    /// Dossier des sources (un sous-dossier par édition).
    #[arg(long)]
    destination: PathBuf,
    #[arg(long, value_delimiter = ',')]
    zones: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    ressources: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    editions: Vec<String>,
}

impl Choix {
    fn garde(&self, s: &Source) -> bool {
        (self.zones.is_empty() || self.zones.contains(&s.zone))
            && (self.ressources.is_empty() || self.ressources.contains(&s.ressource))
            && (self.editions.is_empty() || self.editions.iter().any(|e| s.edition.contains(e.as_str())))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match executer(&cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("erreur : {e}");
            ExitCode::from(2)
        }
    }
}

fn lire(chemin: &Path) -> Result<Vec<Source>, String> {
    let texte = fs::read_to_string(chemin).map_err(|e| format!("{}: {e}", chemin.display()))?;
    manifeste::lire(&texte).map_err(|e| format!("{} : {e}", chemin.display()))
}

/// Écrit à côté puis renomme : le manifeste n'est jamais à moitié écrit.
fn ecrire(chemin: &Path, sources: &[Source]) -> Result<(), String> {
    let tmp = telechargement::partiel(chemin);
    fs::write(&tmp, manifeste::ecrire(sources)).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, chemin).map_err(|e| format!("{}: {e}", chemin.display()))
}

fn executer(cli: &Cli) -> Result<bool, String> {
    match &cli.commande {
        Commande::Inventaire { zones } => {
            let ancien = if cli.manifeste.exists() { lire(&cli.manifeste)? } else { Vec::new() };
            let choisies = if zones.is_empty() { inventaire::zones() } else { zones.clone() };
            let ign = inventaire::inventorier(&choisies)?;
            let mut tout = inventaire::fusionner(ign, &ancien);
            // Zones de l'IGN non inventoriées cette fois : lignes gardées telles quelles.
            tout.extend(
                ancien
                    .iter()
                    .filter(|s| {
                        inventaire::RESSOURCES_IGN.contains(&s.ressource.as_str()) && !choisies.contains(&s.zone)
                    })
                    .cloned(),
            );
            // Relecture : ce qu'on écrit doit se relire.
            manifeste::lire(&manifeste::ecrire(&tout)).map_err(|e| e.to_string())?;
            let octets: u64 = tout.iter().map(|s| s.octets).sum();
            ecrire(&cli.manifeste, &tout)?;
            eprintln!("{} fichiers, {:.1} Go", tout.len(), octets as f64 / 1e9);
            Ok(true)
        }
        Commande::Telecharger { choix, connexions } => {
            let sources: Vec<Source> = lire(&cli.manifeste)?.into_iter().filter(|s| choix.garde(s)).collect();
            let total: u64 = sources.iter().map(|s| s.octets).sum();
            eprintln!("{} fichiers choisis, {:.1} Go", sources.len(), total as f64 / 1e9);
            let mut o = Options::nouvelles(choix.destination.clone());
            o.connexions = usize::from(*connexions);
            let b = telechargement::telecharger(&sources, &o);
            eprintln!(
                "{} reçus ({:.1} Go), {} déjà présents, {} échecs",
                b.recus,
                b.octets as f64 / 1e9,
                b.presents,
                b.echecs.len()
            );
            Ok(b.echecs.is_empty())
        }
        Commande::Verifier { choix } => {
            let mut bon = true;
            for s in lire(&cli.manifeste)?.iter().filter(|s| choix.garde(s)) {
                let c = s.chemin(&choix.destination);
                let r = if c.exists() { telechargement::verifier(s, &c) } else { Err("absent".into()) };
                if let Err(e) = r {
                    println!("FAUX {} : {e}", c.display());
                    bon = false;
                }
            }
            Ok(bon)
        }
        Commande::Empreintes { choix } => {
            let mut sources = lire(&cli.manifeste)?;
            let mut ajoutes = 0;
            for s in sources.iter_mut().filter(|s| choix.garde(s) && s.sha256.is_empty()) {
                let c = s.chemin(&choix.destination);
                if !c.exists() {
                    continue;
                }
                let e = empreinte::calculer(&c).map_err(|e| format!("{}: {e}", c.display()))?;
                if fs::metadata(&c).map(|m| m.len()).ok() != Some(s.octets) || e.md5 != s.md5 {
                    println!("FAUX {} : taille ou MD5", c.display());
                    continue;
                }
                s.sha256 = e.sha256;
                ajoutes += 1;
            }
            ecrire(&cli.manifeste, &sources)?;
            eprintln!("{ajoutes} SHA-256 ajoutés");
            Ok(true)
        }
    }
}
