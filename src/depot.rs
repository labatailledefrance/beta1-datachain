//! Dépôt de relief : le RGE ALTI 1 m et 5 m rangé par dalles, pour que chaque
//! tile lise aussi les dalles des départements voisins (pas de LiDAR HD, B-01).
//!
//! - Un **paquet** par archive (`z0_1m_D005.rel`) : dalles de l'IGN en cm,
//!   codées comme le relief des tiles (MED, zstd), puis un index.
//! - La valeur d'un nœud ne dépend que de sa position et des sources : deux
//!   tiles voisines, même de départements différents, ont les mêmes bords.
//! - **Priorité** au 1 m, puis 5 m (bilinéaire). Au bord du 1 m, **fondu** sur
//!   `FONDU` m vers le 5 m : pas de marche.

use crate::erreur::{Contexte, Res, le};
use crate::relief::{self, Dalle, Noeuds, SANS};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Largeur du fondu entre deux sources (m).
pub const FONDU: usize = 32;
/// Un trou plus étroit que deux fois ce rayon (m) ne déclenche pas de fondu.
pub const FRANGE: usize = 8;
/// Marge lue autour d'une tile : deux fondus successifs et leurs ouvertures.
const MARGE: usize = 2 * (FONDU + 2 * FRANGE) + 2 * FRANGE + 2;
const NF: usize = 1001 + 2 * MARGE;

/// Archive RGE ALTI → paquet. Écrit à part puis renommé : un paquet présent
/// est complet.
///
/// `boite` (m, x0, y0, x1, y1) : ne garde que les dalles qui la touchent
/// (jeux d'essai).
pub fn empaqueter(archives: &[PathBuf], sortie: &Path, boite: Option<[i64; 4]>) -> Res<()> {
    // Un fichier partiel par processus : deux fabrications en même temps ne se
    // mélangent pas (arrivé sur D050).
    let partiel = sortie.with_extension(format!("partiel{}", std::process::id()));
    let mut fic = std::io::BufWriter::new(std::fs::File::create(&partiel).ctx(partiel.display())?);
    fic.write_all(b"P26R0001")?;
    let f = Mutex::new((fic, 8u64, Vec::new()));
    for a in archives {
        relief::parcourir_7z(a, |d| {
            let cote = d.n as i64 * d.pas;
            let dehors = boite.is_some_and(|[x0, y0, x1, y1]| {
                d.x_no >= x1 || d.x_no + cote <= x0 || d.y_no - cote >= y1 || d.y_no < y0
            });
            if dehors || d.cm.iter().all(|v| *v == SANS) {
                return Ok(());
            }
            let g = Noeuds { step: d.pas as usize, side: d.n, cm: d.cm };
            let z = crate::zst(&relief::encoder(&g, 1), 3)?;
            let mut w = f.lock().ctx("verrou empoisonné")?;
            let off = w.1;
            w.0.write_all(&z)?;
            w.1 += z.len() as u64;
            w.2.push((d.x_no, d.y_no, d.pas, d.n as u64, off, z.len() as u64));
            Ok(())
        })?;
    }
    let (mut w, off, mut idx) = f.into_inner().ctx("verrou empoisonné")?;
    // L'ordre d'écriture dépend des threads ; celui de l'index, non. Deux
    // dalles au même endroit (quarts de la Guyane) : la plus grosse gagne.
    idx.sort_by_key(|e| (e.0, e.1, e.5, e.4));
    for e in &idx {
        for v in [e.0, e.1, e.2, e.3 as i64, e.4 as i64, e.5 as i64] {
            w.write_all(&v.to_le_bytes())?;
        }
    }
    w.write_all(&off.to_le_bytes())?;
    w.write_all(&(idx.len() as u64).to_le_bytes())?;
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&partiel, sortie).ctx(sortie.display())?;
    eprintln!("{} dalles, {:.2} Go", idx.len(), off as f64 / 1e9);
    Ok(())
}

#[derive(Clone, Copy)]
struct Ref {
    pas: i64,
    n: usize,
    off: u64,
    len: usize,
}

struct Paquet {
    fic: std::fs::File,
    idx: HashMap<(i64, i64), Ref>,
    nom: String,
}

impl Paquet {
    fn ouvrir(p: &Path) -> Res<Paquet> {
        let ici = || p.display().to_string();
        let fic = std::fs::File::open(p).ctx(ici())?;
        let t = fic.metadata().ctx(ici())?.len();
        let mut b = [0u8; 16];
        fic.read_exact_at(&mut b, t.checked_sub(16).ctx(format!("{} : trop court", ici()))?).ctx(ici())?;
        let off = u64::from_le_bytes(le(&b, 0)?);
        let n = u64::from_le_bytes(le(&b, 8)?) as usize;
        if off.saturating_add(n as u64 * 48) > t {
            return Err(format!("{} : index hors du fichier", ici()).into());
        }
        let mut b = vec![0u8; n * 48];
        fic.read_exact_at(&mut b, off).ctx(ici())?;
        let mut idx = HashMap::new();
        for e in b.as_chunks::<48>().0 {
            let v = |k: usize| i64::from_le_bytes(std::array::from_fn(|o| e[8 * k + o]));
            idx.insert((v(0), v(1)), Ref { pas: v(2), n: v(3) as usize, off: v(4) as u64, len: v(5) as usize });
        }
        let nom = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Paquet { fic, idx, nom })
    }
    fn lire(&self, c: (i64, i64), r: Ref) -> Res<Dalle> {
        let mut b = vec![0u8; r.len];
        self.fic.read_exact_at(&mut b, r.off).ctx(&self.nom)?;
        let brut = zstd::bulk::decompress(&b, 16 + 3 * r.n * r.n + r.n * r.n * 4)
            .ctx(format!("{} : dalle {c:?}", self.nom))?;
        Ok(Dalle { x_no: c.0, y_no: c.1, pas: r.pas, n: r.n, cm: relief::decoder(&brut, r.pas as usize)?.cm })
    }
}

#[derive(Default)]
pub struct Depot {
    p1: Vec<Paquet>,
    p5: Vec<Paquet>,
}

/// Décode toutes les dalles d'un paquet ; rend (dalles, fausses).
pub fn verifier(f: &Path) -> Res<(usize, usize)> {
    let p = Paquet::ouvrir(f)?;
    let v: Vec<_> = p.idx.iter().collect();
    let faux = v
        .par_iter()
        .filter(|(_, r)| {
            let mut b = vec![0u8; r.len];
            p.fic.read_exact_at(&mut b, r.off).is_err() || zstd::bulk::decompress(&b, 16 + 7 * r.n * r.n).is_err()
        })
        .count();
    Ok((v.len(), faux))
}

/// Dalles chargées pour un bloc de 5 km.
pub struct Charge {
    m1: HashMap<(i64, i64), Vec<Arc<Dalle>>>,
    m5: Vec<Arc<Dalle>>,
}

#[derive(Clone, Default, Debug)]
pub struct Stat {
    /// Nœuds venant du 1 m, du 5 m, sans donnée (au pas de 1 m).
    pub part: [u32; 3],
    /// Raccord 1 m/5 m : points, somme |écart| cm, max cm.
    pub raccord: (u32, u64, i32),
    /// Plus grande marche restante entre deux nœuds voisins du fondu, en plus
    /// de la pente de chaque source (cm).
    pub marche: i32,
}

impl Stat {
    /// Source majoritaire, code de l'en-tête de tile : 0 aucune, 2 1 m, 3 5 m
    /// (1, le LiDAR HD de P-26, n'est plus produit).
    pub fn source(&self) -> u8 {
        let [m1, m5, _] = self.part;
        match (m1, m5) {
            (0, 0) => 0,
            _ if m1 >= m5 => 2,
            _ => 3,
        }
    }
}

impl Depot {
    /// `dossier` : dossier des paquets ; `zone` : numéro de zone.
    pub fn ouvrir(dossier: &Path, zone: u8) -> Res<Depot> {
        let mut d = Depot::default();
        let mut fs = std::fs::read_dir(dossier)
            .ctx(dossier.display())?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()
            .ctx(dossier.display())?;
        fs.sort();
        for f in fs {
            let n = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !n.ends_with(".rel") {
                continue;
            }
            if n.starts_with(&format!("z{zone}_1m_")) {
                d.p1.push(Paquet::ouvrir(&f)?);
            } else if n.starts_with(&format!("z{zone}_5m_")) {
                d.p5.push(Paquet::ouvrir(&f)?);
            }
        }
        Ok(d)
    }

    pub fn sources(&self) -> String {
        let n = |v: &[Paquet]| v.iter().map(|p| p.nom.clone()).collect::<Vec<_>>().join(" ");
        format!("1m [{}] 5m [{}]", n(&self.p1), n(&self.p5))
    }

    /// Charge ce qu'il faut pour les tiles du bloc de 5 km (bx, by), en km.
    pub fn charger(&self, bx: i64, by: i64) -> Res<Charge> {
        let mut a1 = Vec::new();
        let (x0, y0) = (bx * 1000, by * 1000);
        for ky in -1..=6i64 {
            for kx in -1..=5i64 {
                let c = (x0 + kx * 1000, y0 + ky * 1000);
                for p in &self.p1 {
                    if let Some(r) = p.idx.get(&c) {
                        a1.push((c, p, *r));
                    }
                }
            }
        }
        let lus: Vec<_> = a1.par_iter().map(|(c, p, r)| Ok((*c, Arc::new(p.lire(*c, *r)?)))).collect::<Res<_>>()?;
        let mut m1: HashMap<(i64, i64), Vec<Arc<Dalle>>> = HashMap::new();
        for (c, d) in lus {
            m1.entry(c).or_default().push(d); // ordre des paquets : trié
        }
        let m = MARGE as i64 + 10;
        let mut a5 = Vec::new();
        for p in &self.p5 {
            for (c, r) in &p.idx {
                let l = r.n as i64 * r.pas;
                if c.0 < x0 + 5000 + m && c.0 + l > x0 - m && c.1 - l < y0 + 5000 + m && c.1 > y0 - m {
                    a5.push((p.nom.clone(), *c, p, *r));
                }
            }
        }
        a5.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        let m5 = a5.par_iter().map(|(_, c, p, r)| Ok(Arc::new(p.lire(*c, *r)?))).collect::<Res<_>>()?;
        Ok(Charge { m1, m5 })
    }
}

impl Charge {
    fn n5(&self, x: i64, y: i64) -> i32 {
        for d in &self.m5 {
            if let Some(v) = d.get(x, y)
                && v != SANS
            {
                return v;
            }
        }
        SANS
    }

    /// Nœuds de 1 m de la tile (km) et source ; `None` si aucune donnée.
    pub fn tile(&self, ix: i64, iy: i64) -> (Option<Noeuds>, Stat) {
        let (x0, y0) = (ix * 1000 - MARGE as i64, iy * 1000 - MARGE as i64);
        let nf = NF as i64;
        let poser = |g: &mut [i32], d: &Dalle| {
            let (xa, xb) = (d.x_no.max(x0), (d.x_no + d.n as i64).min(x0 + nf));
            let (ya, yb) = ((d.y_no - d.n as i64 + 1).max(y0), (d.y_no + 1).min(y0 + nf));
            for y in ya..yb {
                for x in xa..xb {
                    let v = d.cm[((d.y_no - y) as usize) * d.n + (x - d.x_no) as usize];
                    let k = (y - y0) as usize * NF + (x - x0) as usize;
                    if g[k] == SANS {
                        g[k] = v;
                    }
                }
            }
        };
        let mut m1 = vec![SANS; NF * NF];
        for ky in -1..=2 {
            for kx in -1..=1 {
                for d in self.m1.get(&((ix + kx) * 1000, (iy + ky) * 1000)).into_iter().flatten() {
                    poser(&mut m1, d);
                }
            }
        }
        let mut st = Stat::default();
        // Le 5 m n'est calculé que s'il manque quelque chose.
        let trou = m1.contains(&SANS);
        let mut m5 = vec![SANS; NF * NF];
        if trou && !self.m5.is_empty() {
            let (cx, cy) = (x0.div_euclid(5), y0.div_euclid(5));
            let w = NF / 5 + 3;
            let c: Vec<i32> =
                (0..w * w).map(|k| self.n5((cx + (k % w) as i64) * 5, (cy + (k / w) as i64) * 5)).collect();
            for j in 0..NF {
                for i in 0..NF {
                    let (x, y) = (x0 + i as i64, y0 + j as i64);
                    let (a, b) = ((x.div_euclid(5) - cx) as usize, (y.div_euclid(5) - cy) as usize);
                    let (fx, fy) = (x.rem_euclid(5) as i32, y.rem_euclid(5) as i32);
                    let v = [c[b * w + a], c[b * w + a + 1], c[(b + 1) * w + a], c[(b + 1) * w + a + 1]];
                    let p = [(5 - fx) * (5 - fy), fx * (5 - fy), (5 - fx) * fy, fx * fy];
                    m5[j * NF + i] = if (0..4).all(|k| p[k] == 0 || v[k] != SANS) {
                        let s: i64 = (0..4).filter(|&k| p[k] > 0).map(|k| v[k] as i64 * p[k] as i64).sum();
                        s.div_euclid(25) as i32
                    } else {
                        v[(fx >= 3) as usize + 2 * (fy >= 3) as usize]
                    };
                }
            }
        }
        let mut g = m1.clone();
        st.raccord = fondre(&mut g, &m1, &m5);
        let mut out = Noeuds::new(1);
        for j in 0..1001 {
            for i in 0..1001 {
                let k = (j + MARGE) * NF + i + MARGE;
                out.cm[j * 1001 + i] = g[k];
                let s = if g[k] == SANS {
                    2
                } else if m1[k] != SANS {
                    0
                } else {
                    1
                };
                st.part[s] += 1;
            }
        }
        if st.raccord.0 > 0 {
            // Marche ajoutée par le fondu : saut du résultat moins le plus
            // grand saut de ses sources au même endroit.
            for j in MARGE..MARGE + 1000 {
                for i in MARGE..MARGE + 1000 {
                    let k = j * NF + i;
                    for kk in [k + 1, k + NF] {
                        if g[k] == SANS || g[kk] == SANS {
                            continue;
                        }
                        let s = [&m1, &m5]
                            .iter()
                            .filter(|s| s[k] != SANS && s[kk] != SANS)
                            .map(|s| (s[k] - s[kk]).abs())
                            .max();
                        if let Some(s) = s {
                            let m = (g[k] - g[kk]).abs() - s;
                            if m > st.marche {
                                st.marche = m;
                                if std::env::var("DEBUG_MARCHE").is_ok() {
                                    eprintln!(
                                        "marche {m} tile {ix},{iy} ({},{}) g {} {} 1m {} {} 5m {} {}",
                                        i - MARGE,
                                        j - MARGE,
                                        g[k],
                                        g[kk],
                                        m1[k],
                                        m1[kk],
                                        m5[k],
                                        m5[kk]
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        let vide = st.part[2] == 1001 * 1001;
        ((!vide).then_some(out), st)
    }
}

/// Distance (× 5) à l'ensemble `dans`, bornée à `cap` ; chanfrein 5-7.
fn chanfrein(dans: impl Fn(usize) -> bool, cap: i32) -> Vec<i32> {
    let mut d: Vec<i32> = (0..NF * NF).map(|k| if dans(k) { 0 } else { cap }).collect();
    for j in 0..NF {
        for i in 0..NF {
            let k = j * NF + i;
            let mut v = d[k];
            if i > 0 {
                v = v.min(d[k - 1] + 5);
            }
            if j > 0 {
                v = v.min(d[k - NF] + 5);
                if i > 0 {
                    v = v.min(d[k - NF - 1] + 7);
                }
                if i + 1 < NF {
                    v = v.min(d[k - NF + 1] + 7);
                }
            }
            d[k] = v;
        }
    }
    for j in (0..NF).rev() {
        for i in (0..NF).rev() {
            let k = j * NF + i;
            let mut v = d[k];
            if i + 1 < NF {
                v = v.min(d[k + 1] + 5);
            }
            if j + 1 < NF {
                v = v.min(d[k + NF] + 5);
                if i + 1 < NF {
                    v = v.min(d[k + NF + 1] + 7);
                }
                if i > 0 {
                    v = v.min(d[k + NF - 1] + 7);
                }
            }
            d[k] = v;
        }
    }
    d
}

/// `out` = `haut` là où il existe, sinon `bas` ; près d'un bord où seul `bas`
/// existe, mélange selon la distance (chanfrein 1 / √2, borné à FONDU).
fn fondre(out: &mut [i32], haut: &[i32], bas: &[i32]) -> (u32, u64, i32) {
    let seul = |k: usize| haut[k] == SANS && bas[k] != SANS;
    if !(0..NF * NF).any(seul) {
        return (0, 0, 0);
    }
    if haut.iter().all(|v| *v == SANS) {
        out.copy_from_slice(bas);
        return (0, 0, 0);
    }
    let cap = FONDU as i32 * 5;
    // Seuls les trous larges déclenchent le fondu (ouverture de rayon FRANGE) :
    // la frange de quelques mètres d'un trait de côte lu à deux pas ne doit pas
    // adoucir la falaise d'à côté.
    let r = FRANGE as i32 * 5;
    let coeur = chanfrein(|k| haut[k] != SANS, r);
    let large = chanfrein(|k| coeur[k] >= r, r + 1);
    let d = chanfrein(|k| seul(k) && large[k] <= r, cap);
    // Le poids de `bas` décroît aussi vers son propre bord (mer) : sans cela
    // le mélange s'arrêterait net là où `bas` finit.
    let b = FRANGE as i32 * 2 * 5;
    let bord = chanfrein(|k| bas[k] == SANS, b);
    let (mut n, mut s, mut mx) = (0u32, 0u64, 0i32);
    for k in 0..NF * NF {
        if haut[k] == SANS {
            out[k] = bas[k];
        } else if d[k] < cap && bas[k] != SANS {
            let (cap, b) = (cap as i64, b as i64);
            let w = (cap - d[k] as i64) * bord[k] as i64; // poids de bas, sur cap × b
            out[k] = ((haut[k] as i64 * (cap * b - w) + bas[k] as i64 * w).div_euclid(cap * b)) as i32;
            let (i, j) = (k % NF, k / NF);
            let dans = (MARGE..MARGE + 1000).contains(&i) && (MARGE..MARGE + 1000).contains(&j);
            if d[k] == 5 && dans {
                let e = (haut[k] - bas[k]).abs();
                n += 1;
                s += e as u64;
                mx = mx.max(e);
            }
        }
    }
    (n, s, mx)
}

pub struct ReliefTile {
    pub base: Vec<u8>,
    pub fin: Option<Vec<u8>>,
    pub stat: Stat,
}

/// Relief codé des tiles demandées, bloc de 5 km par bloc de 5 km.
pub fn calculer(
    d: &Depot,
    tiles: &[crate::geo::TileId],
    q: i32,
    niveau: i32,
) -> Res<BTreeMap<crate::geo::TileId, ReliefTile>> {
    let mut blocs: BTreeMap<(i64, i64), Vec<crate::geo::TileId>> = BTreeMap::new();
    for t in tiles {
        blocs.entry(((t.iy.div_euclid(5) * 5) as i64, (t.ix.div_euclid(5) * 5) as i64)).or_default().push(*t);
    }
    let mut out = BTreeMap::new();
    for ((by, bx), ts) in blocs {
        let c = d.charger(bx, by)?;
        let v: Vec<_> = ts
            .par_iter()
            .map(|t| -> Res<Option<_>> {
                let (g, stat) = c.tile(t.ix as i64, t.iy as i64);
                let Some(g) = g else { return Ok(None) };
                let (base, fin) = relief::encoder_deux(&g, q);
                // Là où la source est à 5 m, pas de complément.
                let fin = if stat.part[0] > 0 { Some(crate::zst(&fin, niveau)?) } else { None };
                Ok(Some((*t, ReliefTile { base: crate::zst(&base, niveau)?, fin, stat })))
            })
            .collect::<Res<Vec<_>>>()?;
        out.extend(v.into_iter().flatten());
    }
    Ok(out)
}
