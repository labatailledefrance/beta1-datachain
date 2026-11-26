//! Contrôle de tiles écrites, dans un ou plusieurs départements à la fois :
//! empreintes, décodage de toutes les sections, tiles et bâtiments en double,
//! raccords du relief (bords égaux au nœud près), population, routes.

use crate::decodage;
use crate::erreur::{Contexte, Res};
use crate::relief::{self, SANS};
use crate::tuile::{self, Sec};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

struct Bords {
    src: u8,
    /// ouest, est, sud, nord : 1001 nœuds (1 m), ou 501 (2 m seulement).
    b: [Vec<i32>; 4],
}

struct Vu {
    zone: u8,
    ix: i32,
    iy: i32,
    dept: usize,
    flags: u8,
    habitants: u64,
    bats: Vec<u64>,
    routes_m: f64,
    bords: Option<Bords>,
    octets: u64,
    sections: BTreeMap<u8, u64>,
    faux: Option<String>,
}

fn voir(b: &[u8], dept: usize) -> Res<Vu> {
    let n = b.len().checked_sub(32).ctx("tile plus courte que son empreinte")?;
    let faux = (blake3::hash(&b[..n]).as_bytes() != &b[n..]).then(|| "empreinte".to_string());
    let t = tuile::lire(b, false)?;
    let e = &t.entete;
    let bats = t.section(Sec::Bati)?.map(|s| decodage::bati(&s)).unwrap_or_default();
    let routes_m = t
        .section(Sec::Reseau)?
        .map(|s| decodage::reseau(&s))
        .unwrap_or_default()
        .iter()
        .filter(|m| !m.ferre)
        .flat_map(|m| {
            m.pts.windows(2).map(|s| ((s[1][0] - s[0][0]) as f64).hypot((s[1][1] - s[0][1]) as f64)).collect::<Vec<_>>()
        })
        .sum();
    t.section(Sec::Ponts)?.map(|s| decodage::ponts(&s).len());
    t.section(Sec::Zone)?.map(|s| decodage::zone(&s).1.len());
    let com: u64 =
        t.section(Sec::Communes)?.map(|s| decodage::communes(&s)).unwrap_or_default().iter().map(|c| c.2 as u64).sum();
    let fin = t.section(Sec::Relief1m)?;
    let bords = t.section(Sec::Relief)?.map(|s| {
        let g = match &fin {
            Some(f) => relief::decoder_deux(&s, f),
            None => relief::decoder(&s, 2),
        };
        let m = g.m;
        Bords {
            src: e.flags >> 4 & 3,
            b: [
                (0..m).map(|j| g.get(0, j)).collect(),
                (0..m).map(|j| g.get(m - 1, j)).collect(),
                (0..m).map(|i| g.get(i, 0)).collect(),
                (0..m).map(|i| g.get(i, m - 1)).collect(),
            ],
        }
    });
    let faux =
        faux.or((com != e.habitants as u64).then(|| format!("habitants {com} != {}", e.habitants)))
            .or((bats.len() as u32 != e.nb_bat).then(|| "nb_bat".into()));
    Ok(Vu {
        zone: e.zone,
        ix: e.ix,
        iy: e.iy,
        dept,
        flags: e.flags,
        habitants: e.habitants as u64,
        bats: bats.iter().map(|b| b.id).collect(),
        routes_m,
        bords,
        octets: b.len() as u64,
        sections: t.sections.iter().map(|s| (s.0 as u8, s.2.len() as u64)).collect(),
        faux,
    })
}

/// Écart au bord commun : (nœuds comparés, différents, max cm, donnée d'un seul côté).
fn comparer(a: &[i32], b: &[i32]) -> (u64, u64, i32, u64) {
    let (a, b): (Vec<i32>, Vec<i32>) = match (a.len(), b.len()) {
        (x, y) if x == y => (a.to_vec(), b.to_vec()),
        (1001, _) => (a.iter().step_by(2).copied().collect(), b.to_vec()),
        _ => (a.to_vec(), b.iter().step_by(2).copied().collect()),
    };
    let mut r = (0, 0, 0, 0);
    for (x, y) in a.iter().zip(&b) {
        if (*x == SANS) != (*y == SANS) {
            r.3 += 1;
        } else if *x != SANS {
            r.0 += 1;
            if x != y {
                r.1 += 1;
                r.2 = r.2.max((x - y).abs());
            }
        }
    }
    r
}

/// Renvoie le nombre de défauts bloquants (plafonné à 100).
pub fn executer(dossiers: &[PathBuf]) -> Res<usize> {
    let mut vus: Vec<Vu> = Vec::new();
    for (k, d) in dossiers.iter().enumerate() {
        let pq = tuile::paquets(d)?;
        let mut par_fic: BTreeMap<PathBuf, Vec<(u64, usize)>> = BTreeMap::new();
        for (f, o, n) in pq.index.into_values() {
            par_fic.entry(f).or_default().push((o, n));
        }
        // Un paquet à la fois par thread : lecture séquentielle.
        let v: Vec<Vec<Vu>> = par_fic
            .par_iter()
            .map(|(f, l)| -> Res<Vec<Vu>> {
                let b = std::fs::read(f).ctx(f.display())?;
                l.iter()
                    .map(|(o, n)| {
                        let t = b
                            .get(*o as usize..*o as usize + n)
                            .ctx(format!("{} : tile hors du paquet", f.display()))?;
                        voir(t, k).ctx(f.display())
                    })
                    .collect()
            })
            .collect::<Res<_>>()?;
        let v: Vec<Vu> = v.into_iter().flatten().collect();
        let terre = v.iter().filter(|t| t.flags & 1 != 0).count();
        let mut src = [0; 4];
        v.iter().filter(|t| t.flags & 1 != 0).for_each(|t| src[t.bords.as_ref().map_or(0, |b| b.src) as usize] += 1);
        println!(
            "{} : {} tiles ({terre} terre), {} bâtiments, {} habitants, {:.1} km de routes, {:.1} Mo, relief sans/-/1 m/5 m {:?}",
            d.display(),
            v.len(),
            v.iter().map(|t| t.bats.len()).sum::<usize>(),
            v.iter().map(|t| t.habitants).sum::<u64>(),
            v.iter().map(|t| t.routes_m).sum::<f64>() / 1e3,
            v.iter().map(|t| t.octets).sum::<u64>() as f64 / 1e6,
            src
        );
        vus.extend(v);
    }
    let mut defauts = 0;
    for (t, faux) in vus.iter().filter_map(|t| t.faux.as_ref().map(|f| (t, f))) {
        println!("DÉFAUT tile z{} {},{} : {faux}", t.zone, t.ix, t.iy);
        defauts += 1;
    }
    let mut pos: HashMap<(u8, i32, i32), usize> = HashMap::new();
    let (mut dt, mut db) = (0, 0);
    let mut ids: HashMap<(u8, u64), usize> = HashMap::new();
    for (k, t) in vus.iter().enumerate() {
        if let Some(a) = pos.insert((t.zone, t.ix, t.iy), k) {
            dt += 1;
            if dt <= 5 {
                println!(
                    "tile en double z{} {},{} : {} et {}",
                    t.zone,
                    t.ix,
                    t.iy,
                    dossiers[vus[a].dept].display(),
                    dossiers[t.dept].display()
                );
            }
        }
        for b in &t.bats {
            if ids.insert((t.zone, *b), k).is_some() {
                db += 1;
            }
        }
    }
    // Raccords du relief, dans un département et entre deux.
    let mut st = [(0u64, 0u64, 0i32, 0u64, 0u64); 2];
    for (k, t) in vus.iter().enumerate() {
        for (dx, dy, ba, bb) in [(1, 0, 1usize, 0usize), (0, 1, 3, 2)] {
            let Some(&j) = pos.get(&(t.zone, t.ix + dx, t.iy + dy)) else { continue };
            let (Some(a), Some(b)) = (&vus[k].bords, &vus[j].bords) else { continue };
            let r = comparer(&a.b[ba], &b.b[bb]);
            let s = &mut st[(t.dept != vus[j].dept) as usize];
            (s.0, s.1, s.2, s.3) = (s.0 + r.0, s.1 + r.1, s.2.max(r.2), s.3 + r.3);
            s.4 += 1;
        }
    }
    for (n, s) in ["dans un département", "entre départements"].iter().zip(st) {
        println!(
            "raccords {n} : {} bords, {} nœuds, {} différents (max {} cm), {} donnés d'un seul côté",
            s.4, s.0, s.1, s.2, s.3
        );
    }
    let mut sec: BTreeMap<u8, u64> = BTreeMap::new();
    vus.iter().flat_map(|t| &t.sections).for_each(|(s, n)| *sec.entry(*s).or_default() += n);
    let terre = vus.iter().filter(|t| t.flags & 1 != 0).count().max(1);
    println!(
        "octets par tile de terre : {}",
        sec.iter()
            .map(|(s, n)| format!("{} {}", Sec::de(*s).map_or("?", Sec::nom), n / terre as u64))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "total : {} tiles, {dt} en double, {db} bâtiments en double, {} habitants",
        vus.len(),
        vus.iter().map(|t| t.habitants).sum::<u64>()
    );
    defauts += dt + db + st.iter().map(|s| (s.1 > 0 || s.3 > 0) as usize).sum::<usize>();
    println!("{}", if defauts == 0 { "CONFORME".into() } else { format!("{defauts} défauts") });
    Ok(defauts.min(100))
}
