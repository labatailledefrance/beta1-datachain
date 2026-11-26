//! Manifeste des sources : un fichier par ligne, figé dans le dépôt.
//!
//! Format CSV sans guillemets : `zone,ressource,edition,url,octets,md5,sha256`.
//! Le manifeste joue le rôle d'un lockfile : la chaîne ne lit que des fichiers
//! dont la taille et les empreintes sont celles du manifeste. Le rafraîchir
//! (nouvelle édition de la BD TOPO…) est un changement relu en PR, comme une
//! mise à jour de dépendance.
//!
//! Tout ce qui sert à construire un chemin local (édition, nom du fichier) est
//! limité à `[A-Za-z0-9._-]`, et les URL aux seuls hôtes connus, en HTTPS.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub const EN_TETE: &str = "zone,ressource,edition,url,octets,md5,sha256";

/// Hôtes autorisés : Géoplateforme de l'IGN, data.gouv.fr, INSEE.
pub const HOTES: [&str; 3] = ["data.geopf.fr", "static.data.gouv.fr", "www.insee.fr"];

/// Une ligne du manifeste : un fichier à télécharger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Département (`D029`, `D02A`, `D974`…) ou `FRANCE` pour une source nationale.
    pub zone: String,
    /// Famille : `BDTOPO`, `RGEALTI`, `INSEE`.
    pub ressource: String,
    /// Édition : dossier local du fichier.
    pub edition: String,
    pub url: String,
    pub octets: u64,
    /// MD5 publié par l'IGN (vide s'il n'y en a pas).
    pub md5: String,
    /// SHA-256 calculé par le projet (vide tant qu'on ne l'a pas).
    pub sha256: String,
}

impl Source {
    /// Nom du fichier : dernier segment de l'URL (validé à la lecture).
    pub fn nom(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or_default()
    }

    /// Chemin local : `<racine>/<edition>/<nom>`.
    pub fn chemin(&self, racine: &Path) -> PathBuf {
        racine.join(&self.edition).join(self.nom())
    }

    fn ligne(&self) -> String {
        format!(
            "{},{},{},{},{},{},{}",
            self.zone, self.ressource, self.edition, self.url, self.octets, self.md5, self.sha256
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erreur {
    pub ligne: usize,
    pub message: String,
}

impl fmt::Display for Erreur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "manifeste, ligne {} : {}", self.ligne, self.message)
    }
}

impl std::error::Error for Erreur {}

/// Lit et valide un manifeste complet.
pub fn lire(texte: &str) -> Result<Vec<Source>, Erreur> {
    let mut lignes = texte.lines().enumerate().map(|(i, l)| (i + 1, l));
    match lignes.next() {
        Some((_, l)) if l == EN_TETE => {}
        _ => return Err(Erreur { ligne: 1, message: format!("en-tête attendu : {EN_TETE}") }),
    }
    let mut vues = HashSet::new();
    let mut sources = Vec::new();
    for (n, l) in lignes {
        if l.is_empty() {
            continue;
        }
        let s = source(l).map_err(|message| Erreur { ligne: n, message })?;
        if !vues.insert((s.edition.clone(), s.nom().to_owned())) {
            return Err(Erreur { ligne: n, message: format!("fichier en double : {}/{}", s.edition, s.nom()) });
        }
        sources.push(s);
    }
    Ok(sources)
}

/// Écrit un manifeste, trié (zone, ressource, édition, URL) : un diff lisible.
pub fn ecrire(sources: &[Source]) -> String {
    let mut triees: Vec<&Source> = sources.iter().collect();
    triees
        .sort_by(|a, b| (&a.zone, &a.ressource, &a.edition, &a.url).cmp(&(&b.zone, &b.ressource, &b.edition, &b.url)));
    let mut texte = String::from(EN_TETE);
    texte.push('\n');
    for s in triees {
        texte.push_str(&s.ligne());
        texte.push('\n');
    }
    texte
}

fn source(l: &str) -> Result<Source, String> {
    let c: Vec<&str> = l.split(',').collect();
    let [zone, ressource, edition, url, octets, md5, sha256] = c[..] else {
        return Err(format!("7 champs attendus, {} lus", c.len()));
    };
    let s = Source {
        zone: zone.to_owned(),
        ressource: ressource.to_owned(),
        edition: edition.to_owned(),
        url: url.to_owned(),
        octets: octets.parse().map_err(|_| format!("taille invalide : {octets}"))?,
        md5: md5.to_owned(),
        sha256: sha256.to_owned(),
    };
    valider(&s)?;
    Ok(s)
}

/// Contrôles d'une source (aussi appliqués à celles de l'inventaire).
pub fn valider(s: &Source) -> Result<(), String> {
    if !s.zone.bytes().all(|o| o.is_ascii_alphanumeric()) || s.zone.is_empty() {
        return Err(format!("zone invalide : {}", s.zone));
    }
    if !s.ressource.bytes().all(|o| o.is_ascii_uppercase()) || s.ressource.is_empty() {
        return Err(format!("ressource invalide : {}", s.ressource));
    }
    segment(&s.edition).map_err(|e| format!("édition {e}"))?;
    let reste = s.url.strip_prefix("https://").ok_or_else(|| format!("URL sans HTTPS : {}", s.url))?;
    let hote = reste.split('/').next().unwrap_or_default();
    if !HOTES.contains(&hote) {
        return Err(format!("hôte non autorisé : {hote}"));
    }
    if s.url.contains(['?', '#', '%', '\\']) || s.url.contains("/../") || s.url.contains("/./") {
        return Err(format!("URL refusée : {}", s.url));
    }
    segment(s.nom()).map_err(|e| format!("nom de fichier {e}"))?;
    if s.octets == 0 {
        return Err("taille nulle".into());
    }
    hexa(&s.md5, 32).map_err(|e| format!("md5 {e}"))?;
    hexa(&s.sha256, 64).map_err(|e| format!("sha256 {e}"))?;
    if s.md5.is_empty() && s.sha256.is_empty() {
        return Err("ni md5 ni sha256 : fichier invérifiable".into());
    }
    Ok(())
}

/// Un segment de chemin sûr : `[A-Za-z0-9._-]`, ne commence pas par un point.
fn segment(s: &str) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('.')
        && s.bytes().all(|o| o.is_ascii_alphanumeric() || matches!(o, b'.' | b'_' | b'-'));
    if ok { Ok(()) } else { Err(format!("invalide : {s:?}")) }
}

fn hexa(s: &str, n: usize) -> Result<(), String> {
    if s.is_empty() || (s.len() == n && s.bytes().all(|o| matches!(o, b'0'..=b'9' | b'a'..=b'f'))) {
        Ok(())
    } else {
        Err(format!("invalide : {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BDTOPO: &str = "D029,BDTOPO,BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15,https://data.geopf.fr/telechargement/download/BDTOPO/BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15/BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15.7z,505679294,30158abb6bc3dd9dc29c7b8136a49082,";

    fn avec(ligne: &str) -> Result<Vec<Source>, Erreur> {
        lire(&format!("{EN_TETE}\n{ligne}\n"))
    }

    #[test]
    fn lecture_et_ecriture() {
        let s = avec(BDTOPO).unwrap();
        assert_eq!(s[0].nom(), "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15.7z");
        assert_eq!(s[0].octets, 505_679_294);
        assert_eq!(ecrire(&s), format!("{EN_TETE}\n{BDTOPO}\n"));
        assert_eq!(
            s[0].chemin(Path::new("/r")),
            Path::new("/r/BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15")
                .join("BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15.7z")
        );
    }

    #[test]
    fn refus() {
        let cas = [
            BDTOPO.replace("https://data.geopf.fr", "http://data.geopf.fr"),
            BDTOPO.replace("data.geopf.fr", "data.geopf.fr.exemple.org"),
            BDTOPO.replace(",BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15,", ",..,"),
            BDTOPO.replace("D029_2026-09-15.7z", "D029_2026-09-15.7z?x=1"),
            BDTOPO.replace("/BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15.7z", "/.cache"),
            BDTOPO.replace(",505679294,", ",0,"),
            BDTOPO.replace("30158abb6bc3dd9dc29c7b8136a49082", ""),
            BDTOPO.replace("30158abb6bc3dd9dc29c7b8136a49082", "30158ABB6BC3DD9DC29C7B8136A49082"),
            format!("{BDTOPO},en trop"),
        ];
        for c in cas {
            assert!(avec(&c).is_err(), "accepté à tort : {c}");
        }
        assert!(lire("zone,url\n").is_err());
        assert!(avec(&format!("{BDTOPO}\n{BDTOPO}")).is_err(), "doublon accepté");
    }
}
