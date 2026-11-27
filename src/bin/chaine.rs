//! `chaine` : production des tiles, portée de P-26.
//!
//! ```text
//! chaine extraire --source BDTOPO….7z --sortie D029.gpkg
//! chaine relief --source RGEALTI….7z[.001] --sortie z0_1m_D029.rel
//! chaine national --source D029.gpkg … --sortie national-z0.bin
//! chaine departement --gpkg D029.gpkg --dept 29 --zone 0 --national … --relief … --sortie …
//! chaine controle DOSSIER…
//! chaine graphe --sortie graphe.bin DOSSIER…
//! chaine verifier-relief PAQUET…
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use beta1_datachain::erreur::{Contexte, Res};
use beta1_datachain::geo::TileId;
use beta1_datachain::{chaine, controle, depot, graphe, national, relief};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Chaîne de production des tiles de La Bataille de France")]
struct Cli {
    /// Threads (0 : tous les cœurs).
    #[arg(long, global = true, default_value_t = 0)]
    threads: usize,
    #[command(subcommand)]
    commande: Commande,
}

#[derive(Subcommand)]
enum Commande {
    /// Archive 7z de la BD TOPO → GeoPackage.
    Extraire {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        sortie: PathBuf,
    },
    /// Archive(s) du RGE ALTI → paquet de relief.
    Relief {
        #[arg(long, num_args = 1..)]
        source: Vec<PathBuf>,
        #[arg(long)]
        sortie: PathBuf,
        /// Ne garder que les dalles qui touchent cette boîte (km : x0,y0,x1,y1).
        #[arg(long, value_delimiter = ',')]
        boite: Option<Vec<i64>>,
    },
    /// GeoPackages d'une zone → communes et traits de côte de référence.
    National {
        #[arg(long, num_args = 1..)]
        source: Vec<PathBuf>,
        #[arg(long)]
        sortie: PathBuf,
    },
    /// Un département → paquets de tiles.
    Departement(Departement),
    /// Contrôle des tiles écrites (un ou plusieurs départements).
    Controle { dossiers: Vec<PathBuf> },
    /// Départements produits → graphe routier national (ordre CCH compris).
    Graphe {
        #[arg(long)]
        sortie: PathBuf,
        dossiers: Vec<PathBuf>,
    },
    /// Décode toutes les dalles de paquets de relief.
    VerifierRelief { paquets: Vec<PathBuf> },
}

#[derive(Args)]
struct Departement {
    #[arg(long)]
    gpkg: PathBuf,
    /// Code du département sans zéro de tête (29, 2A, 974).
    #[arg(long)]
    dept: String,
    /// Zone de projection (0 Lambert-93, 1 UTM20, 2 UTM22, 3 UTM40S, 4 UTM38S, 5 UTM21).
    #[arg(long, default_value_t = 0)]
    zone: u8,
    /// Dossier des paquets de relief.
    #[arg(long)]
    relief: Option<PathBuf>,
    /// Référence nationale de la zone (communes, côtes).
    #[arg(long)]
    national: Option<PathBuf>,
    #[arg(long)]
    sortie: PathBuf,
    /// Carreaux de 200 m de l'INSEE (Parquet, métropole).
    #[arg(long)]
    insee200: Option<PathBuf>,
    /// Carreaux de 1 km de l'INSEE (CSV, Martinique et La Réunion).
    #[arg(long)]
    insee1k: Option<PathBuf>,
    /// Pas de quantification du relief (cm).
    #[arg(long, default_value_t = 10)]
    relief_q: i32,
    /// Niveau de compression zstd.
    #[arg(long, default_value_t = 9)]
    niveau: i32,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    murs: bool,
    /// Taille minimale d'une cour fermée à percer (cases de 2 m).
    #[arg(long, default_value_t = 4)]
    cour_min: u32,
    /// Longueur maximale d'un passage (cases de 2 m).
    #[arg(long, default_value_t = 30)]
    passage_max: u32,
    /// Tiles à traiter seules (« x146_y6838,… »), pour les essais.
    #[arg(long, value_delimiter = ',')]
    essai: Vec<String>,
    /// Bande de mer gardée autour des côtes (km).
    #[arg(long, default_value_t = 30.0)]
    mer_km: f64,
    #[arg(long, default_value = "")]
    etiquette: String,
}

fn tile_de(s: &str) -> Res<TileId> {
    let s = s.trim();
    let (a, b) =
        s.trim_start_matches('x').split_once("_y").ok_or_else(|| format!("tile « {s} » : format x146_y6838"))?;
    let ix = a.parse().map_err(|_| format!("tile « {s} » : ix"))?;
    let iy = b.parse().map_err(|_| format!("tile « {s} » : iy"))?;
    Ok(TileId { ix, iy })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match executer(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("erreur : {e}");
            ExitCode::from(2)
        }
    }
}

fn executer(cli: Cli) -> Res<ExitCode> {
    if cli.threads > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(cli.threads).build_global().ctx("threads")?;
    }
    match cli.commande {
        Commande::Extraire { source, sortie } => relief::extraire_gpkg(&source, &sortie)?,
        Commande::Relief { source, sortie, boite } => {
            let boite = match boite.as_deref() {
                None => None,
                Some(&[x0, y0, x1, y1]) => Some([x0 * 1000, y0 * 1000, x1 * 1000, y1 * 1000]),
                Some(_) => return Err("--boite : quatre valeurs x0,y0,x1,y1 (km)".into()),
            };
            depot::empaqueter(&source, &sortie, boite)?
        }
        Commande::National { source, sortie } => national::construire(&source, &sortie)?,
        Commande::Departement(d) => {
            let essai = d.essai.iter().map(|s| tile_de(s)).collect::<Result<Vec<_>, _>>()?;
            chaine::executer(&chaine::Params {
                gpkg: d.gpkg,
                dept: d.dept,
                sortie: d.sortie,
                zone: d.zone,
                relief: d.relief,
                national: d.national,
                insee200: d.insee200,
                insee1k: d.insee1k,
                relief_q: d.relief_q,
                niveau: d.niveau,
                murs: d.murs,
                cour_min: d.cour_min,
                passage_max: d.passage_max,
                essai,
                mer_km: d.mer_km,
                etiquette: d.etiquette,
            })?;
        }
        Commande::Controle { dossiers } => {
            return Ok(if controle::executer(&dossiers)? == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) });
        }
        Commande::Graphe { sortie, dossiers } => {
            println!("empreinte {}", graphe::executer(&dossiers, &sortie)?);
        }
        Commande::VerifierRelief { paquets } => {
            let mut mal = 0;
            for f in &paquets {
                let (n, faux) = depot::verifier(f)?;
                println!("{} : {n} dalles, {faux} fausses", f.display());
                mal += faux;
            }
            return Ok(if mal == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) });
        }
    }
    Ok(ExitCode::SUCCESS)
}
