//! Réutilisation des départements d'une publication à l'autre.
//!
//! Une publication est adressée par le digest de l'image : toute nouvelle
//! version de la chaîne, même si elle ne touche qu'au graphe, en produit une
//! autre. Sans réutilisation, c'est la France entière à refaire (12 à 15 h).
//! Comme le cache des couches d'une image de conteneur, chaque département
//! produit porte une **clé** (fichier `CLE`) : le hash de tout ce dont sa
//! production dépend. Un département de même clé, dans une autre publication
//! du même volume (complète ou interrompue), est repris par liens physiques :
//! instantané, sans place en plus, et les fichiers ne changent jamais après
//! écriture (ils sont immuables, comme toute publication).
//!
//! La clé couvre :
//! - la version de la production d'un département ([`VERSION`], à monter à
//!   tout changement de sa sortie), l'empreinte de l'essai de Brest (qui change
//!   avec la sortie, CI à l'appui) et la version du format des tiles ;
//! - les sources du département (BD TOPO), le relief de **toute sa zone** (un
//!   département lit les dalles voisines à ses bords) et les fichiers de relief
//!   présents, les carreaux de l'INSEE ;
//! - le contenu de la référence nationale de la zone (communes et côtes de tous
//!   les départements de la zone) : ajouter un département voisin la change, et
//!   tout est refait, ce qui est voulu.

use std::fs;
use std::path::{Path, PathBuf};

use crate::erreur::{Contexte, Res};
use crate::manifeste::Source;
use crate::orchestre::{self, publication_valide};

/// Version de la production d'un département : à monter quand sa sortie change
/// sans que l'essai de Brest le voie (outre-mer, montagne…).
pub const VERSION: u32 = 1;
/// Nom du fichier de clé, dans le dossier du département.
pub const FICHIER: &str = "CLE";

const BREST: &str = include_str!("../essais/brest.empreinte");

fn identite(s: &Source) -> String {
    format!("{} {} {} {} {}\n", s.ressource, s.edition, s.url, s.octets, s.md5)
}

/// Clé d'un département : `sources` (choisies pour la publication), dossier des
/// paquets de relief et référence nationale de sa zone.
pub fn cle(dept: &str, zone: u8, sources: &[Source], relief: &Path, national: &Path) -> Res<String> {
    let mut h = blake3::Hasher::new();
    h.update(
        format!(
            "beta1-datachain departement {VERSION}\nbrest {}\ntile {}\ndept {dept} zone {zone}\n",
            BREST.trim(),
            beta1_common::tile::VERSION
        )
        .as_bytes(),
    );
    let mut ids: Vec<String> = sources
        .iter()
        .filter(|s| match s.ressource.as_str() {
            "BDTOPO" => s.zone == dept,
            "RGEALTI" => orchestre::zone(&s.edition) == Some(zone),
            _ => true,
        })
        .map(identite)
        .collect();
    ids.sort();
    ids.iter().for_each(|i| {
        h.update(i.as_bytes());
    });
    // Paquets de relief que la chaîne ouvrira pour cette zone.
    let mut rel: Vec<String> = fs::read_dir(relief)
        .ctx(relief.display())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(&format!("z{zone}_")) && n.ends_with(".rel"))
        .collect();
    rel.sort();
    h.update(format!("relief {}\n", rel.join(" ")).as_bytes());
    let n = fs::read(national).ctx(national.display())?;
    h.update(b"national ");
    h.update(blake3::hash(&n).as_bytes());
    Ok(h.finalize().to_hex().to_string())
}

/// Département `dept` de même clé dans une autre publication du volume
/// `tiles` (complète ou interrompue), terminé (`controles.txt`).
pub fn trouver(tiles: &Path, courante: &str, dept: &str, cle: &str) -> Option<PathBuf> {
    let mut noms: Vec<String> =
        fs::read_dir(tiles).ok()?.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
    noms.sort();
    noms.into_iter()
        .filter(|n| {
            let id = n.strip_suffix(".partiel").unwrap_or(n);
            publication_valide(id) && id != courante
        })
        .map(|n| tiles.join(n).join("departements").join(dept))
        .find(|d| {
            d.join("controles.txt").is_file() && fs::read_to_string(d.join(FICHIER)).is_ok_and(|c| c.trim() == cle)
        })
}

/// Copie `src` dans `dst` (qui ne doit pas exister) par liens physiques ; les
/// dossiers sont recréés, un lien symbolique est refusé.
pub fn lier(src: &Path, dst: &Path) -> Res<()> {
    fs::create_dir(dst).ctx(dst.display())?;
    for e in fs::read_dir(src).ctx(src.display())? {
        let e = e.ctx(src.display())?;
        let t = e.file_type().ctx(e.path().display())?;
        let cible = dst.join(e.file_name());
        if t.is_dir() {
            lier(&e.path(), &cible)?;
        } else if t.is_file() {
            fs::hard_link(e.path(), &cible).ctx(cible.display())?;
        } else {
            return Err(format!("{} : ni fichier ni dossier", e.path().display()).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn source(zone: &str, ressource: &str, edition: &str) -> Source {
        Source {
            zone: zone.into(),
            ressource: ressource.into(),
            edition: edition.into(),
            url: format!("https://data.geopf.fr/{edition}.7z"),
            octets: 10,
            md5: "0".repeat(32),
            sha256: String::new(),
        }
    }

    fn racine(nom: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("beta1-reutilisation-{nom}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        d
    }

    #[test]
    fn cle_couvre_les_entrees() {
        let r = racine("cle");
        let (relief, national) = (r.join("relief"), r.join("z0.bin"));
        fs::create_dir_all(&relief).unwrap();
        fs::write(relief.join("z0_1m_D029.rel"), "").unwrap();
        fs::write(relief.join("z3_1m_D974.rel"), "").unwrap();
        fs::write(&national, "communes").unwrap();
        let s = vec![
            source("D029", "BDTOPO", "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15"),
            source("D022", "BDTOPO", "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D022_2026-09-15"),
            source("D029", "RGEALTI", "RGEALTI_2-0_1M_ASC_LAMB93-IGN69_D029_2022-10-14"),
            source("D974", "RGEALTI", "RGEALTI_2-0_1M_ASC_RGR92UTM40S-REUN89_D974_2020-01-01"),
            source("FRANCE", "INSEE", "FILOSOFI_2021_CARREAUX_200M"),
        ];
        let c = cle("D029", 0, &s, &relief, &national).unwrap();
        assert_eq!(c, cle("D029", 0, &s, &relief, &national).unwrap(), "déterministe");
        // Indépendante d'une source d'un autre département ou d'une autre zone…
        let mut t = s.clone();
        t[1].md5 = "1".repeat(32);
        t[3].md5 = "1".repeat(32);
        fs::write(relief.join("z3_5m_D974.rel"), "").unwrap();
        assert_eq!(c, cle("D029", 0, &t, &relief, &national).unwrap());
        // … mais pas du relief de sa zone, de l'INSEE, du national ni de ses sources.
        for k in [0, 2, 4] {
            let mut t = s.clone();
            t[k].md5 = "2".repeat(32);
            assert_ne!(c, cle("D029", 0, &t, &relief, &national).unwrap(), "source {k}");
        }
        fs::write(relief.join("z0_1m_D022.rel"), "").unwrap();
        let c2 = cle("D029", 0, &s, &relief, &national).unwrap();
        assert_ne!(c, c2, "paquet de relief voisin");
        fs::write(&national, "communes et côtes").unwrap();
        assert_ne!(c2, cle("D029", 0, &s, &relief, &national).unwrap(), "national");
        fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn trouver_et_lier() {
        let r = racine("lier");
        let d = r.join("ancienne/departements/D029");
        fs::create_dir_all(d.join("paquets")).unwrap();
        fs::write(d.join("paquets/bloc_x4_y426.p07"), "tiles").unwrap();
        fs::write(d.join("controles.txt"), "empreinte e").unwrap();
        fs::write(d.join(FICHIER), "abc\n").unwrap();
        // Interrompue et sans contrôle : pas reprise.
        let i = r.join("autre.partiel/departements/D029");
        fs::create_dir_all(&i).unwrap();
        fs::write(i.join(FICHIER), "abc").unwrap();
        assert_eq!(trouver(&r, "nouvelle", "D029", "abc"), Some(d.clone()));
        assert_eq!(trouver(&r, "nouvelle", "D029", "abd"), None);
        assert_eq!(trouver(&r, "ancienne", "D029", "abc"), None, "jamais la publication courante");
        let dst = r.join("nouvelle.partiel/departements/D029");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        lier(&d, &dst).unwrap();
        let (a, b) = (d.join("paquets/bloc_x4_y426.p07"), dst.join("paquets/bloc_x4_y426.p07"));
        assert_eq!(fs::metadata(&a).unwrap().ino(), fs::metadata(&b).unwrap().ino(), "lien physique");
        assert!(lier(&d, &dst).is_err(), "destination existante");
        std::os::unix::fs::symlink(&a, d.join("lien")).unwrap();
        assert!(lier(&d, &r.join("x")).is_err(), "lien symbolique refusé");
        fs::remove_dir_all(&r).unwrap();
    }
}
