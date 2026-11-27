//! Orchestration d'une publication complète (binaire `datachain`, lancé par le
//! Job du chart `game`, H-06) : ce qu'il faut produire, et où.
//!
//! Deux volumes :
//!
//! ```text
//! sources/                       PVC « sources », gardée d'une publication à l'autre
//!   <édition>/<fichier>          fichiers du manifeste
//!   derives/gpkg/z<zone>/D029.gpkg
//!   derives/relief/z<zone>_1m_D029.rel, z<zone>_5m_D029.rel
//!   derives/insee/carreaux_1km_<mart|reun>.csv
//! tiles/                         PVC « tiles »
//!   <publication>.partiel/       en cours : national/, departements/, journaux/
//!   <publication>/               terminée : departements/D029/…, graphe.bin, MANIFESTE, COMPLET
//! ```
//!
//! Chaque résultat est écrit à part puis renommé : un résultat présent est
//! complet, et un pod tué reprend où il en était. La publication est adressée
//! par ses **entrées** (digest de l'image, qui embarque le manifeste, et
//! départements choisis) : son nom est donné par le chart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::manifeste::Source;

/// Zones de projection des tiles, d'après le nom d'une édition de l'IGN.
pub fn zone(edition: &str) -> Option<u8> {
    [("LAMB93", 0), ("UTM20", 1), ("UTM22", 2), ("UTM40S", 3), ("UTM38S", 4), ("U21", 5)]
        .into_iter()
        .find(|(k, _)| edition.contains(k))
        .map(|(_, z)| z)
}

/// Code de département pour la chaîne : `D029` → `29`, `D02A` → `2A`, `D974` → `974`.
pub fn code_dept(zone: &str) -> String {
    let c = zone.trim_start_matches('D');
    c.strip_prefix('0').unwrap_or(c).to_owned()
}

/// Quart de la Guyane (`_NE_`…) dans le nom d'une édition, sinon vide.
pub fn quart(edition: &str) -> &'static str {
    ["NE", "NO", "SE", "SO"].into_iter().find(|q| edition.contains(&format!("_{q}_"))).unwrap_or("")
}

/// Une édition à transformer, avec ses fichiers dans le volume des sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edition {
    pub dept: String,
    pub zone: u8,
    pub nom: String,
    /// Fichiers, triés ; le premier est celui à ouvrir (`.7z` ou `.7z.001`).
    pub fichiers: Vec<PathBuf>,
    /// Résultat dans `derives/`.
    pub sortie: PathBuf,
}

/// Ce que demande une publication.
#[derive(Debug, Default)]
pub struct Plan {
    /// Département → (zone, GeoPackage).
    pub departements: BTreeMap<String, (u8, PathBuf)>,
    pub extractions: Vec<Edition>,
    pub reliefs: Vec<Edition>,
    /// Carreaux de 200 m (Parquet) et zip des carreaux de 1 km.
    pub insee200: Option<PathBuf>,
    pub insee1k_zip: Option<PathBuf>,
}

/// Plan d'une publication à partir des sources du manifeste choisies.
pub fn planifier(sources: &[Source], racine: &Path) -> Result<Plan, String> {
    let derives = racine.join("derives");
    let mut par_edition: BTreeMap<&str, Vec<&Source>> = BTreeMap::new();
    for s in sources {
        par_edition.entry(&s.edition).or_default().push(s);
    }
    let mut plan = Plan::default();
    for (ed, fs) in par_edition {
        let s0 = fs[0];
        let mut fichiers: Vec<PathBuf> = fs.iter().map(|s| s.chemin(racine)).collect();
        fichiers.sort();
        match s0.ressource.as_str() {
            "INSEE" if ed.contains("200M") => plan.insee200 = fichiers.first().cloned(),
            "INSEE" if ed.contains("1KM") => plan.insee1k_zip = fichiers.first().cloned(),
            "BDTOPO" => {
                let z = zone(ed).ok_or_else(|| format!("{ed} : zone de projection inconnue"))?;
                let sortie = derives.join(format!("gpkg/z{z}/{}.gpkg", s0.zone));
                if plan.departements.insert(s0.zone.clone(), (z, sortie.clone())).is_some() {
                    return Err(format!("{} : deux éditions de la BD TOPO", s0.zone));
                }
                plan.extractions.push(Edition { dept: s0.zone.clone(), zone: z, nom: ed.into(), fichiers, sortie });
            }
            "RGEALTI" => {
                let z = zone(ed).ok_or_else(|| format!("{ed} : zone de projection inconnue"))?;
                let pas = if ed.contains("_5M_") { "5m" } else { "1m" };
                let sortie = derives.join(format!("relief/z{z}_{pas}_{}{}.rel", s0.zone, quart(ed)));
                // Volumes multiples : on ouvre le premier (.7z.001).
                fichiers.retain(|f| f.extension().is_some_and(|e| e == "7z" || e == "001"));
                plan.reliefs.push(Edition { dept: s0.zone.clone(), zone: z, nom: ed.into(), fichiers, sortie });
            }
            r => return Err(format!("{ed} : ressource {r} inattendue")),
        }
    }
    Ok(plan)
}

/// Fichier des carreaux de 1 km d'un département d'outre-mer, s'il en a.
pub fn carreaux_1km(dept: &str) -> Option<&'static str> {
    match dept {
        "D972" => Some("carreaux_1km_mart.csv"),
        "D974" => Some("carreaux_1km_reun.csv"),
        _ => None,
    }
}

/// Nom de publication valide : `[a-z0-9-]`, 1 à 64 caractères.
pub fn publication_valide(p: &str) -> bool {
    (1..=64).contains(&p.len()) && p.bytes().all(|o| o.is_ascii_lowercase() || o.is_ascii_digit() || o == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(zone: &str, ressource: &str, edition: &str, fichier: &str) -> Source {
        Source {
            zone: zone.into(),
            ressource: ressource.into(),
            edition: edition.into(),
            url: format!("https://data.geopf.fr/x/{fichier}"),
            octets: 1,
            md5: "0".repeat(32),
            sha256: String::new(),
        }
    }

    #[test]
    fn plan_du_finistere_et_de_la_guyane() {
        let s = [
            src("D029", "BDTOPO", "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15", "a.7z"),
            src("D029", "RGEALTI", "RGEALTI_2-0_1M_ASC_LAMB93-IGN69_D029_2022-10-14", "r.7z.001"),
            src("D029", "RGEALTI", "RGEALTI_2-0_1M_ASC_LAMB93-IGN69_D029_2022-10-14", "r.7z.002"),
            src("D029", "RGEALTI", "RGEALTI_2-0_5M_ASC_LAMB93-IGN69_D029_2022-10-14", "c.7z"),
            src("D973", "RGEALTI", "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NE_2021-01-01", "g.7z"),
            src("FRANCE", "INSEE", "FILOSOFI_2021_CARREAUX_200M", "c.parquet"),
        ];
        let p = planifier(&s, Path::new("/s")).unwrap();
        assert_eq!(p.departements["D029"], (0, PathBuf::from("/s/derives/gpkg/z0/D029.gpkg")));
        assert_eq!(p.reliefs.len(), 3);
        let r1 = &p.reliefs[0];
        assert_eq!(r1.sortie, PathBuf::from("/s/derives/relief/z0_1m_D029.rel"));
        assert_eq!(r1.fichiers.len(), 1, "seul le premier volume s'ouvre");
        assert!(p.reliefs.iter().any(|r| r.sortie.ends_with("z2_1m_D973NE.rel")));
        assert_eq!(p.insee200, Some(PathBuf::from("/s/FILOSOFI_2021_CARREAUX_200M/c.parquet")));
    }

    #[test]
    fn codes() {
        assert_eq!((code_dept("D029"), code_dept("D02A"), code_dept("D974")), ("29".into(), "2A".into(), "974".into()));
        assert_eq!((zone("X_LAMB93_D029"), zone("RGR92UTM40S"), zone("X_U21_D975")), (Some(0), Some(3), Some(5)));
        assert!(publication_valide("a1b2-c3") && !publication_valide("../x") && !publication_valide(""));
    }
}
