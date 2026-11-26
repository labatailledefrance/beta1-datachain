//! Référence par zone : communes (une fois chacune) et trait de côte, lus dans
//! toutes les BD TOPO. Chaque département en tire la même réponse à « à qui est
//! cette tile ? » et « mer ou étranger ? ».

use crate::erreur::{Contexte, Erreur, Res, le};
use crate::gpkg::{self, Geom, Ligne, Polygone};
use crate::objets::{self, Commune};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct National {
    pub communes: Vec<Commune>,
    pub cotes: Vec<Ligne>,
}

/// Code de département d'un fichier « D075.gpkg », « D02A.gpkg », « D971.gpkg ».
fn dept_fichier(p: &Path) -> String {
    let n = p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let n = n.trim_start_matches('D').to_string();
    n.strip_prefix('0').map(String::from).unwrap_or(n)
}

pub fn construire(sources: &[PathBuf], sortie: &Path) -> Res<()> {
    let lus: Vec<_> = sources
        .par_iter()
        .map(|s| -> Res<_> {
            let cotes: Vec<Ligne> = gpkg::lire(s, "limite_terre_mer", &[], "")?
                .into_iter()
                .flat_map(|o| match o.geom {
                    Geom::Lignes(l) => l,
                    _ => Vec::new(),
                })
                .collect();
            Ok((dept_fichier(s), objets::lire_communes(s)?, cotes))
        })
        .collect::<Res<_>>()?;
    // Une commune vue par plusieurs extractions : celle de son département.
    let mut com: BTreeMap<String, (bool, Commune)> = BTreeMap::new();
    let mut cotes: BTreeMap<Vec<[i64; 2]>, Ligne> = BTreeMap::new();
    for (d, cs, ls) in lus {
        eprintln!("{d} : {} communes, {} lignes de côte", cs.len(), ls.len());
        for c in cs {
            let propre = c.dept == d;
            match com.get(&c.code) {
                Some((true, _)) => {}
                Some((false, _)) if !propre => {}
                _ => {
                    com.insert(c.code.clone(), (propre, c));
                }
            }
        }
        for l in ls {
            let cle = l.iter().map(|p| [(p[0] * 100.0).round() as i64, (p[1] * 100.0).round() as i64]).collect();
            cotes.insert(cle, l);
        }
    }
    let orphelines: Vec<_> = com.values().filter(|c| !c.0).map(|c| c.1.code.as_str()).collect();
    eprintln!("{} communes ({} hors de leur département), {} lignes de côte", com.len(), orphelines.len(), cotes.len());
    let pop: u64 = com.values().map(|c| c.1.population as u64).sum();
    eprintln!("population {pop}");
    let mut e = crate::tuile::Ecrivain::default();
    let ligne = |e: &mut crate::tuile::Ecrivain, l: &Ligne| {
        e.varint(l.len() as u64);
        for p in l {
            e.buf.extend_from_slice(&p[0].to_le_bytes());
            e.buf.extend_from_slice(&p[1].to_le_bytes());
        }
    };
    e.varint(com.len() as u64);
    for (_, c) in com.values() {
        e.string(&c.code);
        e.string(&c.nom);
        e.string(&c.dept);
        e.varint(c.population as u64);
        e.varint(c.polys.len() as u64);
        for p in &c.polys {
            e.varint(p.len() as u64);
            p.iter().for_each(|a| ligne(&mut e, a));
        }
    }
    e.varint(cotes.len() as u64);
    cotes.values().for_each(|l| ligne(&mut e, l));
    let z = crate::zst(&e.buf, 6)?;
    let mut f = (e.buf.len() as u64).to_le_bytes().to_vec();
    f.extend(z);
    std::fs::write(sortie, f).ctx(sortie.display())
}

pub fn lire(p: &Path) -> Res<National> {
    let f = std::fs::read(p).ctx(p.display())?;
    let n = u64::from_le_bytes(le(&f, 0)?) as usize;
    let b = zstd::bulk::decompress(&f[8..], n).ctx(p.display())?;
    let mut l = beta1_common::codec::Reader::new(&b);
    let faux = |_| Erreur(format!("{} : référence nationale illisible", p.display()));
    fn ligne(l: &mut beta1_common::codec::Reader) -> beta1_common::codec::Result<Ligne> {
        let n = l.count()?;
        (0..n).map(|_| Ok([f64::from_bits(l.u64()?), f64::from_bits(l.u64()?), 0.0])).collect()
    }
    let nc = l.count().map_err(faux)?;
    let communes = (0..nc)
        .map(|_| -> beta1_common::codec::Result<Commune> {
            let (code, nom, dept) = (l.string()?, l.string()?, l.string()?);
            let population = l.varint()? as u32;
            let np = l.count()?;
            let polys: Vec<Polygone> = (0..np)
                .map(|_| {
                    let na = l.count()?;
                    (0..na).map(|_| ligne(&mut l)).collect()
                })
                .collect::<beta1_common::codec::Result<_>>()?;
            Ok(Commune { code, nom, dept, population, polys })
        })
        .collect::<beta1_common::codec::Result<_>>()
        .map_err(faux)?;
    let nl = l.count().map_err(faux)?;
    let cotes = (0..nl).map(|_| ligne(&mut l)).collect::<beta1_common::codec::Result<_>>().map_err(faux)?;
    l.finish().map_err(faux)?;
    Ok(National { communes, cotes })
}
