//! Habitants initiaux par bâtiment, tirés des carreaux de 200 m de l'INSEE
//! (Filosofi 2021), d'où le nombre initial de zombies par tile et par
//! fragment (A-10, A-19).
//!
//! Les carreaux sont sur la grille INSPIRE en ETRS89-LAEA (EPSG 3035), pas en
//! Lambert-93 : un carreau est à cheval sur plusieurs tiles. Répartition
//! **dasymétrique** : les habitants d'un carreau vont à ses bâtiments
//! résidentiels, au prorata de leur plancher ; à défaut, à ses autres
//! bâtiments (sauf annexes) ; à défaut, à 100 points répartis sur le carreau.
//!
//! Conservation exacte : chaque carreau porte un entier (ind arrondi), réparti
//! par la méthode du plus fort reste, égalités départagées par l'identifiant.
//! Le résultat ne dépend que des données, pas de l'ordre de traitement : deux
//! départements voisins qui ont le même bâti dans leur zone tampon répartissent
//! un carreau frontalier de la même façon (aucune perte, aucun doublon).

use crate::erreur::{Contexte, Erreur, Res};
use crate::geo::Laea2L93;
use crate::objets::Batiment;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

pub struct Carreau {
    pub e: i64, // coin sud-ouest, m LAEA
    pub n: i64,
    pub ind: f64,
    /// Côté (m) : 200 en métropole, 1 000 en Martinique et à La Réunion.
    pub cote: i64,
    pub communes: String,
}

/// Lit les carreaux de 200 m dont le coin est dans la boîte LAEA donnée.
pub fn lire_200m(chemin: &Path, boite: [f64; 4]) -> Res<(Vec<Carreau>, usize)> {
    let ici = || chemin.display().to_string();
    let f = File::open(chemin).ctx(ici())?;
    let r = SerializedFileReader::new(f).ctx(ici())?;
    let mut out = Vec::new();
    let mut total = 0;
    for ligne in r.get_row_iter(None).ctx(ici())? {
        let ligne = ligne.ctx(ici())?;
        total += 1;
        let (mut id, mut ind, mut lcog) = (String::new(), 0.0f64, String::new());
        for (nom, champ) in ligne.get_column_iter() {
            match (nom.as_str(), champ) {
                ("idcar_200m", Field::Str(s)) => id = s.clone(),
                ("ind", Field::Double(v)) => ind = *v,
                ("ind", Field::Float(v)) => ind = *v as f64,
                ("ind", Field::Long(v)) => ind = *v as f64,
                ("ind", Field::Int(v)) => ind = *v as f64,
                ("lcog_geo", Field::Str(s)) => lcog = s.clone(),
                _ => {}
            }
        }
        let (n, e) = coin(&id).ctx(ici())?;
        if (e as f64) + 200.0 < boite[0]
            || (e as f64) > boite[2]
            || (n as f64) + 200.0 < boite[1]
            || (n as f64) > boite[3]
        {
            continue;
        }
        out.push(Carreau { e, n, ind, cote: 200, communes: lcog });
    }
    out.sort_by_key(|c| (c.n, c.e));
    Ok((out, total))
}

/// Coin sud-ouest (nord, est) d'un identifiant de carreau
/// (« CRS3035RES200mN2892000E3172000 »).
fn coin(id: &str) -> Res<(i64, i64)> {
    let faux = || Erreur(format!("identifiant de carreau « {id} »"));
    let pn = id.find("mN").ok_or_else(faux)? + 1;
    let pe = id.rfind('E').filter(|&pe| pe > pn).ok_or_else(faux)?;
    Ok((id[pn + 1..pe].parse().map_err(|_| faux())?, id[pe + 1..].parse().map_err(|_| faux())?))
}

/// Résultat de la répartition.
pub struct Repartition {
    /// Habitants par bâtiment (indice dans la liste des bâtiments).
    pub par_bat: HashMap<usize, u32>,
    /// Habitants posés sur des points (carreaux sans bâtiment) : (x, y L93, n).
    pub points: Vec<(f64, f64, u32)>,
    pub carreaux: usize,
    pub somme_ind: f64,
    pub somme_entiers: u64,
    pub sur_residentiel: usize,
    pub sur_autres: usize,
    pub sur_points: usize,
}

/// Plus fort reste : répartit `total` selon `poids` (≥ 0), égalités
/// départagées par la clé.
pub fn plus_fort_reste(total: u64, poids: &[(u64, f64)]) -> Vec<u64> {
    let s: f64 = poids.iter().map(|p| p.1).sum();
    if s <= 0.0 {
        let mut v = vec![0; poids.len()];
        if !v.is_empty() {
            v[0] = total;
        }
        return v;
    }
    let mut parts: Vec<u64> = Vec::with_capacity(poids.len());
    let mut restes: Vec<(f64, u64, usize)> = Vec::with_capacity(poids.len());
    let mut donne = 0u64;
    for (k, (cle, p)) in poids.iter().enumerate() {
        let x = total as f64 * p / s;
        let e = x.floor() as u64;
        parts.push(e);
        donne += e;
        restes.push((x - e as f64, *cle, k));
    }
    restes.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut reste = total - donne;
    for (_, _, k) in restes {
        if reste == 0 {
            break;
        }
        parts[k] += 1;
        reste -= 1;
    }
    parts
}

/// Carreaux de 1 km (CSV) de Martinique et de La Réunion, dans la projection
/// des tiles.
pub fn lire_1km(chemin: &Path) -> Res<Vec<Carreau>> {
    let t = std::fs::read_to_string(chemin).ctx(chemin.display())?;
    let mut out: Vec<Carreau> = t
        .lines()
        .skip(1)
        .map(|l| -> Res<Carreau> {
            // Le champ des communes est entre guillemets et peut avoir des virgules.
            let (mut c, mut debut, mut dans) = (Vec::new(), 0, false);
            for (k, o) in l.bytes().enumerate() {
                match o {
                    b'"' => dans = !dans,
                    b',' if !dans => {
                        c.push(l[debut..k].trim_matches('"'));
                        debut = k + 1;
                    }
                    _ => {}
                }
            }
            c.push(l[debut..].trim_matches('"'));
            let [id, _, communes, ind, ..] = c[..] else {
                return Err(format!("{} : ligne « {l} »", chemin.display()).into());
            };
            let (n, e) = coin(id)?;
            Ok(Carreau {
                n,
                e,
                ind: ind.parse().ctx(format!("{} : ind « {ind} »", chemin.display()))?,
                cote: 1000,
                communes: communes.into(),
            })
        })
        .collect::<Res<_>>()?;
    out.sort_by_key(|c| (c.n, c.e));
    Ok(out)
}

/// Sans carreaux : population communale répartie sur les bâtiments
/// résidentiels de la commune (à défaut tous), au prorata du plancher.
pub fn par_commune(communes: &[crate::objets::Commune], dept: &str, bats: &[Batiment]) -> Repartition {
    let mut r = Repartition {
        par_bat: HashMap::new(),
        points: Vec::new(),
        carreaux: 0,
        somme_ind: 0.0,
        somme_entiers: 0,
        sur_residentiel: 0,
        sur_autres: 0,
        sur_points: 0,
    };
    for c in communes.iter().filter(|c| c.dept == dept) {
        r.somme_ind += c.population as f64;
        r.somme_entiers += c.population as u64;
        let bb = crate::gpkg::Geom::Polygones(c.polys.clone()).bbox();
        let mut ks: Vec<usize> = (0..bats.len())
            .filter(|&k| {
                let b = &bats[k];
                b.cx >= bb[0]
                    && b.cx <= bb[2]
                    && b.cy >= bb[1]
                    && b.cy <= bb[3]
                    && crate::chaine::dans_poly([b.cx, b.cy], &c.polys)
            })
            .collect();
        if ks.iter().any(|&k| bats[k].residentiel()) {
            ks.retain(|&k| bats[k].residentiel());
            r.sur_residentiel += 1;
        } else {
            r.sur_autres += 1;
        }
        ks.sort_by_key(|&k| bats[k].id);
        if ks.is_empty() {
            let b = (bb[0] / 2.0 + bb[2] / 2.0, bb[1] / 2.0 + bb[3] / 2.0);
            r.points.push((b.0, b.1, c.population));
            r.sur_points += 1;
            continue;
        }
        let parts = plus_fort_reste(
            c.population as u64,
            &ks.iter().map(|&k| (bats[k].id, bats[k].plancher())).collect::<Vec<_>>(),
        );
        for (k, n) in ks.iter().zip(parts) {
            if n > 0 {
                *r.par_bat.entry(*k).or_default() += n as u32;
            }
        }
    }
    r
}

/// `conv` absent : carreaux déjà dans la projection des tiles.
pub fn repartir(carreaux: &[Carreau], bats: &[Batiment], conv: Option<&Laea2L93>) -> Repartition {
    // Chaque bâtiment dans le carreau qui contient son centroïde (en LAEA).
    let cote = carreaux.first().map_or(200, |c| c.cote) as f64;
    let mut par_carreau: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for (k, b) in bats.iter().enumerate() {
        let (e, n) = match conv {
            Some(c) => {
                let (lat, lon) = c.l93.inverse(b.cx, b.cy);
                c.laea.avant(lat, lon)
            }
            None => (b.cx, b.cy),
        };
        let c = ((e / cote).floor() as i64 * cote as i64, (n / cote).floor() as i64 * cote as i64);
        par_carreau.entry(c).or_default().push(k);
    }
    let mut r = Repartition {
        par_bat: HashMap::new(),
        points: Vec::new(),
        carreaux: 0,
        somme_ind: 0.0,
        somme_entiers: 0,
        sur_residentiel: 0,
        sur_autres: 0,
        sur_points: 0,
    };
    for c in carreaux {
        let total = c.ind.round().max(0.0) as u64;
        r.carreaux += 1;
        r.somme_ind += c.ind;
        r.somme_entiers += total;
        if total == 0 {
            continue;
        }
        let vide = Vec::new();
        let ks = par_carreau.get(&(c.e, c.n)).unwrap_or(&vide);
        let mut poids: Vec<(u64, f64, usize)> =
            ks.iter().filter(|&&k| bats[k].residentiel()).map(|&k| (bats[k].id, bats[k].plancher(), k)).collect();
        if !poids.is_empty() {
            r.sur_residentiel += 1;
        } else {
            poids = ks
                .iter()
                .filter(|&&k| bats[k].usage1 != 2 && !bats[k].leger)
                .map(|&k| (bats[k].id, bats[k].plancher(), k))
                .collect();
            if !poids.is_empty() {
                r.sur_autres += 1;
            }
        }
        if !poids.is_empty() {
            poids.sort_by_key(|p| p.0);
            let parts = plus_fort_reste(total, &poids.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>());
            for (p, n) in poids.iter().zip(parts) {
                if n > 0 {
                    *r.par_bat.entry(p.2).or_default() += n as u32;
                }
            }
        } else {
            // 100 points au centre de cases de 20 m.
            r.sur_points += 1;
            let parts = plus_fort_reste(total, &(0..100u64).map(|k| (k, 1.0)).collect::<Vec<_>>());
            for (k, n) in parts.iter().enumerate() {
                if *n == 0 {
                    continue;
                }
                let (p, d) = (cote / 10.0, cote / 20.0);
                let (e, nn) = (c.e as f64 + d + p * (k % 10) as f64, c.n as f64 + d + p * (k / 10) as f64);
                let (x, y) = conv.map_or((e, nn), |c| c.conv(e, nn));
                r.points.push((x, y, *n as u32));
            }
        }
    }
    r
}
