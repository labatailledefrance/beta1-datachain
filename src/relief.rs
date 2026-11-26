//! Relief : RGE ALTI (ASC) lu **en flux** dans l'archive 7z de l'IGN, sans
//! la décompresser sur disque, et converti en grilles de nœuds par tile.
//!
//! - Les dalles ASC donnent l'altitude au **centre** des pixels, sur les
//!   multiples du pas (1 ou 5 m), bord ouest et bord nord de la dalle inclus.
//!   Les valeurs ont deux décimales : elles sont lues en **centimètres
//!   entiers**, sans passer par un flottant (déterminisme exact).
//! - Une tile stocke ses **nœuds** aux multiples de `r` m, bords compris :
//!   (1000/r + 1)² nœuds. Les nœuds du bord est et du bord sud viennent des
//!   dalles voisines : deux tiles voisines ont exactement les mêmes valeurs
//!   sur leur bord commun, donc aucune marche (contrôle `marches`).
//! - 2 m et 5 m sont **décimés** du 1 m (un nœud sur r), pas moyennés : la
//!   valeur d'un nœud ne dépend que de sa position globale.
//! - Encodage : quantification (pas de `q` cm), prédiction MED (LOCO-I, celle
//!   de JPEG-LS), résidus en zigzag sur deux plans d'octets, puis zstd. Les
//!   nœuds sans donnée (mer) sont un bitmap à part ; leur valeur est la
//!   prédiction, pour ne rien coûter.
//!
//! Analogie : la grille de nœuds est un journal de séquences partagé entre
//! deux répliques ; chaque nœud de bord a une seule source de vérité.

use crate::erreur::{Contexte, Erreur, Res};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

pub use beta1_common::tile::relief::NONE as SANS;
/// Grille de nœuds : celle du format des tiles (`beta1_common::tile::relief`).
pub use beta1_common::tile::relief::{Nodes as Noeuds, encode as encoder, encode_two as encoder_deux};

/// Décode une grille encodée par [`encoder`].
pub fn decoder(b: &[u8], pas: usize) -> Res<Noeuds> {
    Ok(beta1_common::tile::relief::decode(b, pas)?)
}

/// Une dalle décodée : altitudes en cm, ligne 0 au nord.
pub struct Dalle {
    /// Centre du pixel nord-ouest (m, entiers).
    pub x_no: i64,
    pub y_no: i64,
    pub pas: i64,
    pub n: usize,
    pub cm: Vec<i32>,
}

impl Dalle {
    #[inline]
    pub fn get(&self, x: i64, y: i64) -> Option<i32> {
        let (dx, dy) = (x - self.x_no, self.y_no - y);
        if dx < 0 || dy < 0 || dx % self.pas != 0 || dy % self.pas != 0 {
            return None;
        }
        let (i, j) = ((dx / self.pas) as usize, (dy / self.pas) as usize);
        if i >= self.n || j >= self.n {
            return None;
        }
        Some(self.cm[j * self.n + i])
    }
}

/// Analyse d'un fichier ASC (en-tête ESRI puis valeurs).
pub fn lire_asc(t: &[u8]) -> Res<Dalle> {
    let tronque = || Erreur::from("ASC : fichier tronqué");
    let at = |p: usize| t.get(p).copied().ok_or_else(tronque);
    let mut p = 0usize;
    let mut entete: HashMap<String, f64> = HashMap::new();
    for _ in 0..6 {
        while at(p)?.is_ascii_whitespace() {
            p += 1;
        }
        let d = p;
        while !at(p)?.is_ascii_whitespace() {
            p += 1;
        }
        let cle = String::from_utf8_lossy(&t[d..p]).to_ascii_lowercase();
        while matches!(at(p)?, b' ' | b'\t') {
            p += 1;
        }
        let d = p;
        while !at(p)?.is_ascii_whitespace() {
            p += 1;
        }
        let v = String::from_utf8_lossy(&t[d..p]);
        entete.insert(cle.clone(), v.parse().ctx(format!("ASC : {cle} « {v} »"))?);
    }
    let champ = |c: &str| entete.get(c).copied().ctx(format!("ASC : {c} absent"));
    let n = champ("ncols")? as usize;
    if n != champ("nrows")? as usize {
        return Err("ASC : dalle non carrée".into());
    }
    let pas = champ("cellsize")?;
    let nodata = (champ("nodata_value")? * 100.0).round() as i64;
    let x_no = (champ("xllcorner")? + pas / 2.0).round() as i64;
    let y_no = (champ("yllcorner")? + pas / 2.0 + (n as f64 - 1.0) * pas).round() as i64;
    let mut cm = Vec::with_capacity((n * n).min(t.len()));
    let len = t.len();
    while cm.len() < n * n {
        while p < len && t[p].is_ascii_whitespace() {
            p += 1;
        }
        let neg = at(p)? == b'-';
        if neg {
            p += 1;
        }
        let mut ent: i64 = 0;
        while p < len && t[p].is_ascii_digit() {
            ent = ent * 10 + (t[p] - b'0') as i64;
            p += 1;
        }
        let mut frac: i64 = 0;
        let mut nd = 0;
        if p < len && t[p] == b'.' {
            p += 1;
            while p < len && t[p].is_ascii_digit() {
                if nd < 3 {
                    frac = frac * 10 + (t[p] - b'0') as i64;
                }
                nd += 1;
                p += 1;
            }
        }
        // Centimètres : deux décimales exactes ; une troisième est arrondie.
        let v = match nd.min(3) {
            0 => ent * 100,
            1 => ent * 100 + frac * 10,
            2 => ent * 100 + frac,
            _ => ent * 100 + (frac + 5) / 10,
        };
        let v = if neg { -v } else { v };
        cm.push(if v == nodata { SANS } else { v as i32 });
    }
    Ok(Dalle { x_no, y_no, pas: pas.round() as i64, n, cm })
}

/// Parcourt toutes les dalles ASC d'une archive 7z, en parallèle par bloc
/// (chaque bloc LZMA2 est un flux indépendant), et appelle `f` sur chacune.
/// Renvoie (nombre de dalles, octets décompressés).
pub fn parcourir_7z(chemin: &Path, f: impl Fn(Dalle) -> Res<()> + Sync) -> Res<(usize, u64)> {
    use sevenz_rust2::{Archive, BlockDecoder, Password};
    let mut fic = Volumes::ouvrir(chemin).ctx(chemin.display())?;
    let archive = Archive::read(&mut fic, &Password::empty()).ctx(chemin.display())?;
    let nb = archive.blocks.len();
    let compte = Mutex::new((0usize, 0u64));
    (0..nb).into_par_iter().try_for_each(|b| -> Res<()> {
        let mut fic = Volumes::ouvrir(chemin).ctx(chemin.display())?;
        let mdp = Password::empty();
        let dec = BlockDecoder::new(1, b, &archive, &mdp, &mut fic);
        let mut tampon: Vec<u8> = Vec::new();
        dec.for_each_entries(&mut |e, r| {
            if e.name().ends_with(".asc") {
                tampon.clear();
                r.read_to_end(&mut tampon)?;
                let n = tampon.len() as u64;
                let dalle = lire_asc(&tampon).map_err(|x| std::io::Error::other(format!("{} : {x}", e.name())))?;
                f(dalle).map_err(|x| std::io::Error::other(x.0))?;
                let mut c = compte.lock().map_err(|_| std::io::Error::other("verrou empoisonné"))?;
                c.0 += 1;
                c.1 += n;
            } else {
                std::io::copy(r, &mut std::io::sink())?;
            }
            Ok(true)
        })
        .map(|_| ())
        .ctx(format!("{}, bloc {b}", chemin.display()))
    })?;
    compte.into_inner().ctx("verrou empoisonné")
}

// ------------------------------------------------------------ Grilles de nœuds

/// Grille de nœuds d'une tile au pas `r` : (n+1)² valeurs en cm (ou SANS),
/// ligne j = 0 au **sud** (y = y0), comme les autres couches.
#[derive(Clone)]
pub struct Bords {
    pub ouest: Vec<i32>, // j = 0..=n, valeurs y0 + j·r (j = 0 inconnu : SANS)
    pub nord: Vec<i32>,  // i = 0..=n
}

/// Nœuds intérieurs d'une tile depuis sa dalle de 1 m (bords est et sud
/// manquants), et ses bords à exporter.
pub fn noeuds_de_dalle(d: &Dalle, x0: i64, y0: i64, r: usize) -> (Noeuds, Bords) {
    let mut g = Noeuds::new(r);
    let m = g.side;
    let ri = r as i64;
    for j in 0..m {
        for i in 0..m {
            if let Some(v) = d.get(x0 + i as i64 * ri, y0 + j as i64 * ri) {
                g.cm[j * m + i] = v;
            }
        }
    }
    let ouest = (0..m).map(|j| g.cm[j * m]).collect();
    let nord = (0..m).map(|i| g.cm[(m - 1) * m + i]).collect();
    (g, Bords { ouest, nord })
}

/// Complète les bords est et sud d'une grille avec les bords exportés des
/// voisines : est (ix+1, iy) → colonne ouest ; sud (ix, iy-1) → ligne nord ;
/// coin sud-est (ix+1, iy-1) → premier nœud de sa ligne nord.
pub fn completer(g: &mut Noeuds, est: Option<&Bords>, sud: Option<&Bords>, sud_est: Option<&Bords>) {
    let m = g.side;
    if let Some(b) = est {
        for j in 1..m {
            g.cm[j * m + m - 1] = b.ouest[j];
        }
    }
    if let Some(b) = sud {
        for i in 0..m - 1 {
            g.cm[i] = b.nord[i];
        }
    }
    if let Some(b) = sud_est {
        g.cm[m - 1] = b.nord[0];
    }
}

/// Erreur d'une grille au pas r (interpolation bilinéaire) par rapport aux
/// nœuds de 1 m de la même tile : (somme des carrés en cm², max en cm,
/// nombre de points). Seuls les points dont les quatre nœuds existent.
pub fn erreur(fine: &Noeuds, g: &Noeuds) -> (f64, i64, u64, Vec<u32>) {
    let r = g.step;
    let (mut s2, mut mx, mut n) = (0f64, 0i64, 0u64);
    let mut hist = vec![0u32; 64]; // par tranches de 5 cm
    for y in 0..1000usize {
        for x in 0..1000usize {
            let v = fine.get(x, y);
            if v == SANS {
                continue;
            }
            let (i, j) = (x / r, y / r);
            let (fx, fy) = ((x % r) as f64 / r as f64, (y % r) as f64 / r as f64);
            let (a, b, c, d) = (g.get(i, j), g.get(i + 1, j), g.get(i, j + 1), g.get(i + 1, j + 1));
            if a == SANS || b == SANS || c == SANS || d == SANS {
                continue;
            }
            let z = (a as f64 * (1.0 - fx) + b as f64 * fx) * (1.0 - fy) + (c as f64 * (1.0 - fx) + d as f64 * fx) * fy;
            let e = (z - v as f64).abs();
            s2 += e * e;
            mx = mx.max(e.round() as i64);
            hist[((e / 5.0) as usize).min(63)] += 1;
            n += 1;
        }
    }
    (s2, mx, n, hist)
}

/// Rééchantillonne une grille de 5 m native (dalles de 5 km) aux nœuds de la
/// tile : valeur du pixel natif au même point (les centres coïncident).
pub fn noeuds_natifs_5m(dalles: &HashMap<(i64, i64), Dalle>, x0: i64, y0: i64) -> Noeuds {
    let mut g = Noeuds::new(5);
    let m = g.side;
    for j in 0..m {
        for i in 0..m {
            let (x, y) = (x0 + 5 * i as i64, y0 + 5 * j as i64);
            // Dalle de 5 km qui contient le centre (x, y) : bord ouest et nord inclus.
            let xd = x.div_euclid(5000) * 5000;
            let yd = (y - 5).div_euclid(5000) * 5000 + 5000;
            if let Some(d) = dalles.get(&(xd, yd))
                && let Some(v) = d.get(x, y)
            {
                g.cm[j * m + i] = v;
            }
        }
    }
    g
}

/// Archive de l'IGN en un ou plusieurs volumes (`.7z.001`, `.7z.002`…) vus
/// comme un seul fichier.
pub struct Volumes {
    fics: Vec<(File, u64)>,
    pos: u64,
}

impl Volumes {
    pub fn ouvrir(chemin: &Path) -> std::io::Result<Volumes> {
        let mut fics = Vec::new();
        let nom = chemin.to_string_lossy();
        if let Some(base) = nom.strip_suffix(".001") {
            for k in 1.. {
                let f = format!("{base}.{k:03}");
                if !Path::new(&f).exists() {
                    break;
                }
                let f = File::open(f)?;
                let n = f.metadata()?.len();
                fics.push((f, n));
            }
        } else {
            let f = File::open(chemin)?;
            let n = f.metadata()?.len();
            fics.push((f, n));
        }
        Ok(Volumes { fics, pos: 0 })
    }
}
impl Read for Volumes {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let mut p = self.pos;
        for (f, n) in &self.fics {
            if p < *n {
                let m = b.len().min((n - p) as usize);
                let lu = f.read_at(&mut b[..m], p)?;
                self.pos += lu as u64;
                return Ok(lu);
            }
            p -= n;
        }
        Ok(0)
    }
}
impl Seek for Volumes {
    fn seek(&mut self, s: SeekFrom) -> std::io::Result<u64> {
        let total: u64 = self.fics.iter().map(|f| f.1).sum();
        let p = match s {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => total as i64 + d,
        };
        if p < 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

/// Extrait le premier `.gpkg` d'une archive.
pub fn extraire_gpkg(archive: &Path, sortie: &Path) -> Res<()> {
    use sevenz_rust2::{Archive, BlockDecoder, Password};
    let partiel = sortie.with_extension("partiel");
    let mut fic = Volumes::ouvrir(archive).ctx(archive.display())?;
    let mdp = Password::empty();
    let a = Archive::read(&mut fic, &mdp).ctx(archive.display())?;
    let mut fait = false;
    for b in 0..a.blocks.len() {
        BlockDecoder::new(1, b, &a, &mdp, &mut fic)
            .for_each_entries(&mut |e, r| {
                if e.name().ends_with(".gpkg") && !fait {
                    let mut o = File::create(&partiel)?;
                    std::io::copy(r, &mut o)?;
                    o.sync_all()?;
                    fait = true;
                } else {
                    std::io::copy(r, &mut std::io::sink())?;
                }
                Ok(true)
            })
            .ctx(format!("{}, bloc {b}", archive.display()))?;
    }
    if !fait {
        return Err(format!("pas de .gpkg dans {}", archive.display()).into());
    }
    std::fs::rename(&partiel, sortie).ctx(sortie.display())
}
