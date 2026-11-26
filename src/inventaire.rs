//! Inventaire des sources de l'IGN : régénère le manifeste (l'équivalent d'un
//! `cargo update`).
//!
//! Pour chaque département : dernière édition de la BD TOPO (GeoPackage, tous
//! thèmes), du RGE ALTI 1 m et 5 m, d'après les flux Atom du service de
//! téléchargement de la Géoplateforme (métadonnées seulement). Les sources qui
//! ne viennent pas de l'IGN (INSEE) sont reprises telles quelles du manifeste
//! précédent, comme les SHA-256 déjà calculés d'un fichier inchangé.

use std::collections::BTreeMap;
use std::time::Duration;

use ureq::Agent;

use crate::manifeste::{self, Source};
use crate::telechargement::AGENT_HTTP;

const API: &str = "https://data.geopf.fr/telechargement/resource";
const PAGE: usize = 50;

/// Familles de l'IGN suivies par l'inventaire.
pub const RESSOURCES_IGN: [&str; 2] = ["BDTOPO", "RGEALTI"];

/// Départements couverts par l'IGN : métropole, Corse, outre-mer (sans le
/// Pacifique, absent des données de l'IGN).
pub fn zones() -> Vec<String> {
    let mut z: Vec<String> = (1..=95).filter(|&i| i != 20).map(|i| format!("D{i:03}")).collect();
    z.extend(["D02A".into(), "D02B".into()]);
    z.extend((971..=978).map(|i| format!("D{i}")));
    z
}

/// Produit d'une famille : la BD TOPO en GeoPackage, le RGE ALTI en 1 m et 5 m.
fn produit(ressource: &str, titre: &str) -> bool {
    match ressource {
        // BDTOPO_3-<chiffre>_TOUSTHEMES_GPKG_…
        "BDTOPO" => {
            titre.starts_with("BDTOPO_3-")
                && titre.as_bytes().get(9).is_some_and(u8::is_ascii_digit)
                && titre.get(10..).is_some_and(|r| r.starts_with("_TOUSTHEMES_GPKG_"))
        }
        "RGEALTI" => titre.starts_with("RGEALTI_2-0_1M_ASC_") || titre.starts_with("RGEALTI_2-0_5M_ASC_"),
        _ => false,
    }
}

/// Dernières éditions d'une zone, parmi les titres du flux. Une zone peut
/// porter plusieurs emprises (les quatre quadrants de la Guyane) : on garde la
/// dernière date, toutes emprises à cette date confondues.
pub fn dernieres_editions(ressource: &str, titres: &[String]) -> Vec<String> {
    // Clé : le titre sans sa date (10 derniers caractères), et sans le pas
    // (1 m ou 5 m) pour que chaque pas garde sa propre dernière édition.
    let mut par_emprise: BTreeMap<&str, &str> = BTreeMap::new();
    for t in titres.iter().filter(|t| produit(ressource, t) && t.len() > 10 && t.is_char_boundary(t.len() - 10)) {
        let cle = &t[..t.len() - 10];
        let e = par_emprise.entry(cle).or_insert(t);
        if t.as_str() > *e {
            *e = t;
        }
    }
    let mut par_pas: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in par_emprise.values() {
        let pas = if t.contains("_5M_") { "5m" } else { "autre" };
        par_pas.entry(pas).or_default().push(t);
    }
    let mut sortie = Vec::new();
    for ts in par_pas.values() {
        let der = ts.iter().map(|t| &t[t.len() - 10..]).max().unwrap_or_default();
        sortie.extend(ts.iter().filter(|t| t.ends_with(der)).map(|t| t.to_string()));
    }
    sortie.sort();
    sortie
}

/// Les `<entry>` d'un flux Atom.
pub fn entrees(xml: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut reste = xml;
    while let Some(d) = reste.find("<entry>") {
        let apres = &reste[d + 7..];
        let Some(f) = apres.find("</entry>") else { break };
        v.push(&apres[..f]);
        reste = &apres[f + 8..];
    }
    v
}

/// Contenu de la première balise `<nom>…</nom>`.
pub fn balise<'a>(xml: &'a str, nom: &str) -> Option<&'a str> {
    let ouvrante = format!("<{nom}>");
    let d = xml.find(&ouvrante)? + ouvrante.len();
    let f = xml[d..].find(&format!("</{nom}>"))?;
    Some(&xml[d..d + f])
}

/// Valeur de la première occurrence de `nom="…"`.
pub fn attribut<'a>(xml: &'a str, nom: &str) -> Option<&'a str> {
    let motif = format!(" {nom}=\"");
    let d = xml.find(&motif)? + motif.len();
    let f = xml[d..].find('"')?;
    Some(&xml[d..d + f])
}

/// Les fichiers d'une édition, d'après les entrées de son flux.
pub fn fichiers(zone: &str, ressource: &str, edition: &str, entrees: &[&str]) -> Vec<Source> {
    let mut v = Vec::new();
    for e in entrees {
        let (Some(url), Some(octets)) = (attribut(e, "href"), attribut(e, "gpf_dl:length")) else { continue };
        if !url.contains("/download/") || url.ends_with(".md5") {
            continue;
        }
        v.push(Source {
            zone: zone.into(),
            ressource: ressource.into(),
            edition: edition.into(),
            url: url.into(),
            octets: octets.parse().unwrap_or(0),
            md5: balise(e, "content").unwrap_or_default().into(),
            sha256: String::new(),
        });
    }
    v
}

fn lire(agent: &Agent, url: &str) -> Result<String, String> {
    let mut derniere = String::new();
    for essai in 0..6u64 {
        if essai > 0 {
            std::thread::sleep(Duration::from_secs(5 * essai));
        }
        match agent.get(url).call() {
            Ok(mut r) => match r.body_mut().with_config().limit(64 << 20).read_to_string() {
                Ok(t) => return Ok(t),
                Err(e) => derniere = e.to_string(),
            },
            Err(e) => derniere = e.to_string(),
        }
        eprintln!("{url} : {derniere}");
    }
    Err(format!("{url} : {derniere}"))
}

/// Toutes les entrées d'un flux paginé.
fn pages(agent: &Agent, url: &str) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for page in 1.. {
        let sep = if url.contains('?') { '&' } else { '?' };
        let xml = lire(agent, &format!("{url}{sep}limit={PAGE}&page={page}"))?;
        let es = entrees(&xml);
        let n = es.len();
        v.extend(es.into_iter().map(str::to_owned));
        if n < PAGE {
            break;
        }
    }
    Ok(v)
}

/// Inventaire d'une zone, pour les familles de l'IGN.
pub fn zone(agent: &Agent, zone: &str) -> Result<Vec<Source>, String> {
    let mut sortie = Vec::new();
    for ressource in RESSOURCES_IGN {
        let titres: Vec<String> = pages(agent, &format!("{API}/{ressource}?zone={zone}"))?
            .iter()
            .filter_map(|e| balise(e, "title").map(str::to_owned))
            .collect();
        let editions = dernieres_editions(ressource, &titres);
        if editions.is_empty() {
            eprintln!("{zone} {ressource} : aucune édition");
        }
        for ed in editions {
            let es = pages(agent, &format!("{API}/{ressource}/{ed}"))?;
            let refs: Vec<&str> = es.iter().map(String::as_str).collect();
            sortie.extend(fichiers(zone, ressource, &ed, &refs));
        }
    }
    for s in &sortie {
        manifeste::valider(s).map_err(|e| format!("{zone} : {e}"))?;
    }
    Ok(sortie)
}

/// Inventaire complet (deux requêtes à la fois, comme le téléchargement).
pub fn inventorier(zones: &[String]) -> Result<Vec<Source>, String> {
    let agent: Agent = Agent::config_builder()
        .user_agent(AGENT_HTTP)
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    let file = std::sync::Mutex::new(zones.iter());
    let resultats = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|p| {
        for _ in 0..2 {
            p.spawn(|| {
                while let Some(z) = file.lock().ok().and_then(|mut f| f.next()) {
                    let r = zone(&agent, z);
                    eprintln!(
                        "{z} : {}",
                        r.as_ref().map(|v| format!("{} fichiers", v.len())).unwrap_or_else(|e| e.clone())
                    );
                    if let Ok(mut v) = resultats.lock() {
                        v.push(r);
                    }
                }
            });
        }
    });
    let mut sortie = Vec::new();
    for r in resultats.into_inner().map_err(|_| "verrou empoisonné")? {
        sortie.extend(r?);
    }
    Ok(sortie)
}

/// Nouvel inventaire de l'IGN + sources hors IGN de l'ancien manifeste ; un
/// fichier inchangé (même URL, taille, MD5) garde son SHA-256.
pub fn fusionner(ign: Vec<Source>, ancien: &[Source]) -> Vec<Source> {
    let connus: BTreeMap<(&str, u64, &str), &str> = ancien
        .iter()
        .filter(|s| !s.sha256.is_empty())
        .map(|s| ((s.url.as_str(), s.octets, s.md5.as_str()), s.sha256.as_str()))
        .collect();
    let mut v: Vec<Source> = ign
        .into_iter()
        .map(|mut s| {
            if let Some(h) = connus.get(&(s.url.as_str(), s.octets, s.md5.as_str())) {
                s.sha256 = (*h).to_owned();
            }
            s
        })
        .collect();
    v.extend(ancien.iter().filter(|s| !RESSOURCES_IGN.contains(&s.ressource.as_str())).cloned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zones_couvertes() {
        let z = zones();
        assert_eq!(z.len(), 94 + 2 + 8);
        assert!(z.contains(&"D029".into()) && z.contains(&"D02B".into()) && !z.contains(&"D020".into()));
    }

    #[test]
    fn derniere_edition_par_emprise_et_par_pas() {
        let titres = t(&[
            "BDTOPO_3-3_TOUSTHEMES_GPKG_LAMB93_D029_2024-12-15",
            "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15",
            "BDTOPO_3-5_TOUSTHEMES_SHP_LAMB93_D029_2026-09-15",
            "BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-06-15",
        ]);
        assert_eq!(dernieres_editions("BDTOPO", &titres), t(&["BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15"]));
        // Guyane : quatre quadrants à la même date ; le 5 m a sa propre date.
        let titres = t(&[
            "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NE_2021-01-01",
            "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NO_2021-01-01",
            "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NO_2019-01-01",
            "RGEALTI_2-0_5M_ASC_UTM22-RGFG95_D973_2020-01-01",
            "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_2015-01-01",
        ]);
        assert_eq!(
            dernieres_editions("RGEALTI", &titres),
            t(&[
                "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NE_2021-01-01",
                "RGEALTI_2-0_1M_ASC_UTM22-RGFG95_D973_NO_2021-01-01",
                "RGEALTI_2-0_5M_ASC_UTM22-RGFG95_D973_2020-01-01",
            ])
        );
    }

    #[test]
    fn lecture_du_flux_atom() {
        let xml = r#"<feed><title>BDTOPO_3-5_TOUSTHEMES_GPKG_LAMB93_D029_2026-09-15</title>
  <entry>
    <link href="https://data.geopf.fr/telechargement/download/BDTOPO/E/E.7z" rel="alternate" type="application/x-7z-compressed" gpf_dl:length="505679294"/>
    <content>30158abb6bc3dd9dc29c7b8136a49082</content>
  </entry>
  <entry>
    <link href="https://data.geopf.fr/telechargement/download/BDTOPO/E/E.7z.md5" rel="alternate" gpf_dl:length="32"/>
  </entry></feed>"#;
        let es = entrees(xml);
        assert_eq!(es.len(), 2);
        let f = fichiers("D029", "BDTOPO", "E", &es);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].octets, f[0].md5.as_str()), (505_679_294, "30158abb6bc3dd9dc29c7b8136a49082"));
        assert_eq!(f[0].nom(), "E.7z");
    }

    #[test]
    fn fusion_garde_insee_et_sha256() {
        let ign = Source {
            zone: "D029".into(),
            ressource: "BDTOPO".into(),
            edition: "E".into(),
            url: "https://data.geopf.fr/a/E.7z".into(),
            octets: 10,
            md5: "0".repeat(32),
            sha256: String::new(),
        };
        let insee = Source {
            zone: "FRANCE".into(),
            ressource: "INSEE".into(),
            edition: "F".into(),
            url: "https://www.insee.fr/f.zip".into(),
            octets: 5,
            md5: String::new(),
            sha256: "1".repeat(64),
        };
        let ancien = [Source { sha256: "2".repeat(64), ..ign.clone() }, insee.clone()];
        let v = fusionner(vec![ign.clone()], &ancien);
        assert_eq!(v, vec![ancien[0].clone(), insee.clone()]);
        // Nouvelle édition (MD5 changé) : le SHA-256 ne suit pas.
        let v = fusionner(vec![Source { md5: "3".repeat(32), ..ign }], &ancien);
        assert!(v[0].sha256.is_empty());
    }
}
