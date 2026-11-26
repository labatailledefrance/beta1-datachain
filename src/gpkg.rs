//! Lecture d'un GeoPackage : une base SQLite dont les géométries sont du WKB
//! précédé d'un en-tête « GP » (même décodage que P-01 et P-05, en Rust).
//!
//! Chaque couche est lue en entier, triée par `fid` (ordre stable, quel que
//! soit le nombre de threads). Plusieurs couches sont lues en parallèle, une
//! connexion SQLite par thread (lecture seule).

use crate::erreur::{Contexte, Res, le};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::path::Path;

/// Un anneau de polygone ou une ligne : sommets (x, y, z) en Lambert-93.
pub type Ligne = Vec<[f64; 3]>;
/// Un polygone : anneau extérieur puis trous.
pub type Polygone = Vec<Ligne>;

#[derive(Clone, Debug, Default)]
pub enum Geom {
    #[default]
    Vide,
    Point([f64; 3]),
    Lignes(Vec<Ligne>),
    Polygones(Vec<Polygone>),
}

impl Geom {
    pub fn bbox(&self) -> [f64; 4] {
        let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        let mut p = |q: &[f64; 3]| {
            b[0] = b[0].min(q[0]);
            b[1] = b[1].min(q[1]);
            b[2] = b[2].max(q[0]);
            b[3] = b[3].max(q[1]);
        };
        match self {
            Geom::Vide => {}
            Geom::Point(q) => p(q),
            Geom::Lignes(ls) => ls.iter().flatten().for_each(&mut p),
            Geom::Polygones(ps) => ps.iter().flatten().flatten().for_each(&mut p),
        }
        b
    }
}

struct Lecteur<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Lecteur<'a> {
    fn octets<const N: usize>(&mut self) -> Res<[u8; N]> {
        let v = le::<N>(self.b, self.off)?;
        self.off += N;
        Ok(v)
    }
    fn u32(&mut self, le: bool) -> Res<u32> {
        let v = self.octets::<4>()?;
        Ok(if le { u32::from_le_bytes(v) } else { u32::from_be_bytes(v) })
    }
    fn f64(&mut self, le: bool) -> Res<f64> {
        let v = self.octets::<8>()?;
        Ok(if le { f64::from_le_bytes(v) } else { f64::from_be_bytes(v) })
    }
    fn points(&mut self, le: bool, dims: usize) -> Res<Ligne> {
        let n = self.u32(le)? as usize;
        // Pas de réservation plus grande que ce que le blob peut contenir.
        let mut v = Vec::with_capacity(n.min(self.b.len().saturating_sub(self.off) / 16));
        for _ in 0..n {
            let x = self.f64(le)?;
            let y = self.f64(le)?;
            let z = if dims >= 3 { self.f64(le)? } else { 0.0 };
            if dims == 4 {
                self.f64(le)?;
            }
            v.push([x, y, z]);
        }
        Ok(v)
    }
    fn geom(&mut self, out: &mut Geom, profondeur: u32) -> Res<()> {
        if profondeur > 8 {
            return Err("WKB : collections trop imbriquées".into());
        }
        let le = self.octets::<1>()?[0] == 1;
        let t = self.u32(le)?;
        // ISO (1000, 2000, 3000) ou EWKB (bits de poids fort).
        let mut dims = 2;
        if t & 0x8000_0000 != 0 || matches!((t % 10000) / 1000, 1 | 2) {
            dims = 3;
        }
        if (t & 0xC000_0000) == 0xC000_0000 || (t % 10000) / 1000 == 3 {
            dims = 4;
        }
        let base = (t & 0xFFFF) % 1000;
        match base {
            1 => {
                let x = self.f64(le)?;
                let y = self.f64(le)?;
                let z = if dims >= 3 { self.f64(le)? } else { 0.0 };
                if dims == 4 {
                    self.f64(le)?;
                }
                *out = Geom::Point([x, y, z]);
            }
            2 => {
                let l = self.points(le, dims)?;
                match out {
                    Geom::Lignes(v) => v.push(l),
                    _ => *out = Geom::Lignes(vec![l]),
                }
            }
            3 => {
                let nr = self.u32(le)? as usize;
                let p: Polygone = (0..nr).map(|_| self.points(le, dims)).collect::<Res<_>>()?;
                match out {
                    Geom::Polygones(v) => v.push(p),
                    _ => *out = Geom::Polygones(vec![p]),
                }
            }
            4..=7 => {
                let n = self.u32(le)? as usize;
                for _ in 0..n {
                    self.geom(out, profondeur + 1)?;
                }
            }
            _ => return Err(format!("type WKB inattendu : {t}").into()),
        }
        Ok(())
    }
}

/// Décode une géométrie GeoPackage.
pub fn decoder(blob: &[u8]) -> Res<Geom> {
    if blob.len() < 8 || &blob[..2] != b"GP" {
        return Ok(Geom::Vide);
    }
    let flags = blob[3];
    if flags & 0x10 != 0 {
        return Ok(Geom::Vide); // géométrie vide
    }
    let env = match (flags >> 1) & 7 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        _ => 0,
    };
    let mut l = Lecteur { b: blob, off: 8 + env };
    let mut g = Geom::Vide;
    l.geom(&mut g, 0)?;
    Ok(g)
}

/// Une valeur d'attribut, réduite à ce dont la chaîne a besoin.
#[derive(Clone, Debug, Default)]
pub enum Val {
    #[default]
    Nul,
    Ent(i64),
    Reel(f64),
    Texte(String),
}

impl Val {
    pub fn f(&self) -> Option<f64> {
        match self {
            Val::Ent(i) => Some(*i as f64),
            Val::Reel(r) => Some(*r),
            _ => None,
        }
    }
    pub fn i(&self) -> Option<i64> {
        match self {
            Val::Ent(i) => Some(*i),
            Val::Reel(r) => Some(*r as i64),
            Val::Texte(t) => t.parse().ok(),
            _ => None,
        }
    }
    pub fn s(&self) -> &str {
        match self {
            Val::Texte(t) => t,
            _ => "",
        }
    }
    pub fn b(&self) -> bool {
        matches!(self, Val::Ent(1)) || matches!(self, Val::Texte(t) if t == "1" || t == "true")
    }
}

pub struct Objet {
    pub fid: i64,
    pub geom: Geom,
    pub att: Vec<Val>,
}

pub fn ouvrir(chemin: &Path) -> Res<Connection> {
    let c = Connection::open_with_flags(chemin, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .ctx(chemin.display())?;
    // Lecture d'un gros fichier : cache de 256 Mo, pas de verrou.
    c.execute_batch("PRAGMA cache_size=-262144; PRAGMA mmap_size=4294967296;").ok();
    Ok(c)
}

/// La table existe-t-elle ?
pub fn existe(c: &Connection, table: &str) -> bool {
    c.query_row("select count(*) from gpkg_contents where table_name=?", [table], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Lit une couche : géométrie et colonnes demandées (absentes → Nul), avec
/// un filtre SQL optionnel. Triée par fid.
pub fn lire(chemin: &Path, table: &str, colonnes: &[&str], filtre: &str) -> Res<Vec<Objet>> {
    let ici = || format!("{} : {table}", chemin.display());
    let c = ouvrir(chemin)?;
    if !existe(&c, table) {
        return Ok(Vec::new());
    }
    let presentes: Vec<String> = c
        .prepare(&format!("pragma table_info(\"{table}\")"))
        .ctx(ici())?
        .query_map([], |r| r.get::<_, String>(1))
        .ctx(ici())?
        .collect::<Result<_, _>>()
        .ctx(ici())?;
    let cols: Vec<String> = colonnes
        .iter()
        .map(|c| if presentes.iter().any(|p| p == c) { format!("\"{c}\"") } else { "NULL".into() })
        .collect();
    let sel = if cols.is_empty() { String::new() } else { format!(", {}", cols.join(", ")) };
    let w = if filtre.is_empty() { String::new() } else { format!(" where {filtre}") };
    let sql = format!("select fid, geometrie{sel} from \"{table}\"{w} order by fid");
    let mut st = c.prepare(&sql).ctx(ici())?;
    let n = colonnes.len();
    let mut out = Vec::new();
    let mut rows = st.query([]).ctx(ici())?;
    while let Some(r) = rows.next().ctx(ici())? {
        let fid: i64 = r.get(0).ctx(ici())?;
        let geom = match r.get_ref(1).ctx(ici())? {
            ValueRef::Blob(b) => decoder(b).ctx(format!("{}, fid {fid}", ici()))?,
            _ => Geom::Vide,
        };
        let att = (0..n)
            .map(|k| {
                Ok(match r.get_ref(2 + k)? {
                    ValueRef::Null => Val::Nul,
                    ValueRef::Integer(i) => Val::Ent(i),
                    ValueRef::Real(f) => Val::Reel(f),
                    ValueRef::Text(t) => Val::Texte(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => Val::Nul,
                })
            })
            .collect::<Result<_, rusqlite::Error>>()
            .ctx(ici())?;
        out.push(Objet { fid, geom, att });
    }
    Ok(out)
}

/// Identifiant numérique stable d'un objet BD TOPO : les 16 chiffres de son
/// `cleabs` (« BATIMENT0000000300445226 » → 300445226). Le préfixe est la
/// couche, implicite.
pub fn cle(v: &Val) -> u64 {
    let s = v.s();
    let d: String = s.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    d.parse().unwrap_or(0)
}
