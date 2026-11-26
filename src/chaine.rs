//! La chaîne : données brutes d'un département → paquets de tiles de 1 km.
//!
//! Étapes (chacune chronométrée, mémoire relevée) :
//!
//! 1. **lecture** de la BD TOPO (couches en parallèle) ;
//! 2. **inventaire** : quelles tiles appartiennent au département (centre dans
//!    une de ses communes, ou en mer à moins de 30 km de sa côte la plus
//!    proche), et le **halo** d'une tile autour, pris dans la zone tampon de
//!    l'extraction IGN, qui sert aux raccords ;
//! 3. **population** : carreaux INSEE → habitants par bâtiment ;
//! 4. **répartition** des objets par tile (boîtes englobantes), découpage des
//!    routes aux bords, ponts, lieux, mairies ;
//! 5. **relief** en flux depuis l'archive 7z ;
//! 6. **passe 1** par tile, en parallèle : bâti 1 m, franchissable 2 m,
//!    occupation, communes, composantes de la zone, encodage des couches ;
//! 7. **fusion** de la zone accessible (union-find sur les bords) ;
//! 8. **passe 2** par tile : zone, cours fermées et passages, relief raccordé,
//!    assemblage et empreinte ;
//! 9. **écriture** des paquets.
//!
//! Déterminisme : aucune étape ne dépend de l'ordre d'exécution des tâches.
//! Les phases parallèles ne travaillent que sur leur tile, à partir de listes
//! triées ; les étapes globales (fusion, population, mairies) parcourent des
//! structures triées.

use crate::erreur::{Contexte, Res};
use crate::geo::{Laea2L93, TileId};
use crate::gpkg::{Geom, Ligne, Polygone};
use crate::mesure::{Etapes, ms};
use crate::objets::{self, Batiment, Donnees};
use crate::raster::{self, Bitmap, Fenetre};
use crate::tuile::{self, Ecrivain, Entete, Sec, Section, q};
use crate::zone;
use crate::{depot, emprise, portes};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

pub struct Params {
    pub gpkg: PathBuf,
    pub dept: String,
    pub sortie: PathBuf,
    pub zone: u8,
    pub relief: Option<PathBuf>,
    pub national: Option<PathBuf>,
    pub insee200: Option<PathBuf>,
    pub insee1k: Option<PathBuf>,
    pub relief_q: i32,
    pub niveau: i32,
    pub murs: bool,
    pub cour_min: u32,
    pub passage_max: u32,
    pub essai: Vec<TileId>,
    pub mer_km: f64,
    pub etiquette: String,
}

/// Les tiles traitées : celles du département et leur halo.
pub struct Monde {
    pub tiles: Vec<TileId>,
    pub pos: HashMap<TileId, usize>,
    pub propre: Vec<bool>,
    pub dist_cote_km: Vec<f32>,
    /// Par tile : cases de 100 m hors commune et à l'étranger (10 × 10 bits).
    pub etranger: Vec<[bool; 100]>,
}

impl Monde {
    pub fn n(&self) -> usize {
        self.tiles.len()
    }
}

/// Code INSEE en entier : « 50129 » → 50129 ; Corse « 2A004 » → 1 000 004,
/// « 2B033 » → 1 100 033.
pub fn code_num(c: &str) -> u32 {
    if let Some(r) = c.strip_prefix("2A") {
        1_000_000 + r.parse::<u32>().unwrap_or(0)
    } else if let Some(r) = c.strip_prefix("2B") {
        1_100_000 + r.parse::<u32>().unwrap_or(0)
    } else {
        c.parse().unwrap_or(0)
    }
}

// ------------------------------------------------------------ 2. Inventaire

pub fn inventaire(d: &Donnees, cotes: &[Ligne], p: &Params) -> Res<Monde> {
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for c in d.communes.iter().filter(|c| c.dept == p.dept) {
        let bb = Geom::Polygones(c.polys.clone()).bbox();
        b = [b[0].min(bb[0]), b[1].min(bb[1]), b[2].max(bb[2]), b[3].max(bb[3])];
    }
    // Les communes des voisins jusqu'à mer_km de plus : le plus proche décide.
    // La terre d'un voisin à mer_km d'une tile de mer doit être vue : sinon,
    // deux départements réclament la même tile (Chausey, 2026-11-26).
    let marge = 2.0 * p.mer_km * 1000.0 + 2000.0;
    let (x0, y0) = (((b[0] - marge) / 1000.0).floor() * 1000.0, ((b[1] - marge) / 1000.0).floor() * 1000.0);
    let (x1, y1) = (((b[2] + marge) / 1000.0).ceil() * 1000.0, ((b[3] + marge) / 1000.0).ceil() * 1000.0);
    let res = 100.0;
    let (nx, ny) = (((x1 - x0) / res) as usize, ((y1 - y0) / res) as usize);
    let depts: Vec<String> = d.communes.iter().map(|c| c.dept.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let mut lab = vec![u8::MAX; nx * ny];
    let w = Fenetre { x0, y0, res, nx, ny };
    let rang: BTreeMap<&str, u8> = depts.iter().enumerate().map(|(k, d)| (d.as_str(), k as u8)).collect();
    for c in &d.communes {
        let k = rang.get(c.dept.as_str()).copied().unwrap_or(u8::MAX);
        for poly in &c.polys {
            raster::polygone(w, poly, |i, j| lab[j * nx + i] = k);
        }
    }
    // Terre proche du trait de côte (300 m) : de là se propage la mer ; du
    // reste de la limite des communes se propage l'étranger.
    let mut cote = vec![false; nx * ny];
    for l in cotes {
        raster::ligne(w, l, 300.0, |i, j| cote[j * nx + i] = true);
    }
    // Distance à la terre la plus proche (chanfrein 1 / √2, deux passes),
    // avec l'étiquette du département le plus proche.
    let mut dist = vec![f32::MAX; nx * ny];
    for k in 0..nx * ny {
        if lab[k] != u8::MAX {
            dist[k] = 0.0;
        }
    }
    let r2 = std::f32::consts::SQRT_2;
    let terre: Vec<bool> = lab.iter().map(|v| *v != u8::MAX).collect();
    let mut mer = cote;
    let mut relax = |dist: &mut Vec<f32>, lab: &mut Vec<u8>, k: usize, kk: usize, w: f32| {
        if dist[kk] + w < dist[k] {
            dist[k] = dist[kk] + w;
            lab[k] = lab[kk];
            mer[k] = mer[kk];
        }
    };
    for j in 0..ny {
        for i in 0..nx {
            let k = j * nx + i;
            if i > 0 {
                relax(&mut dist, &mut lab, k, k - 1, 1.0);
            }
            if j > 0 {
                relax(&mut dist, &mut lab, k, k - nx, 1.0);
                if i > 0 {
                    relax(&mut dist, &mut lab, k, k - nx - 1, r2);
                }
                if i + 1 < nx {
                    relax(&mut dist, &mut lab, k, k - nx + 1, r2);
                }
            }
        }
    }
    for j in (0..ny).rev() {
        for i in (0..nx).rev() {
            let k = j * nx + i;
            if i + 1 < nx {
                relax(&mut dist, &mut lab, k, k + 1, 1.0);
            }
            if j + 1 < ny {
                relax(&mut dist, &mut lab, k, k + nx, 1.0);
                if i + 1 < nx {
                    relax(&mut dist, &mut lab, k, k + nx + 1, r2);
                }
                if i > 0 {
                    relax(&mut dist, &mut lab, k, k + nx - 1, r2);
                }
            }
        }
    }
    let kd = rang.get(p.dept.as_str()).copied().ctx(format!("département {} absent des données", p.dept))?;
    let mut propres = BTreeMap::new();
    for ty in (y0 / 1000.0) as i32..(y1 / 1000.0) as i32 {
        for tx in (x0 / 1000.0) as i32..(x1 / 1000.0) as i32 {
            let t = TileId { ix: tx, iy: ty };
            let (cx, cy) = t.centre();
            let (i, j) = (((cx - x0) / res) as usize, ((cy - y0) / res) as usize);
            let k = j * nx + i;
            let dk = dist[k] * res as f32 / 1000.0;
            // Une tile sans commune et à l'étranger n'est pas du jeu (gris).
            let avec_terre = (0..100).any(|c| terre[(j - 5 + c / 10) * nx + i - 5 + c % 10]);
            if lab[k] == kd && dk as f64 <= p.mer_km && (avec_terre || mer[k]) {
                propres.insert(t, dk);
            }
        }
    }
    if !p.essai.is_empty() {
        propres.retain(|t, _| p.essai.contains(t));
    }
    let mut tous: BTreeMap<TileId, (bool, f32)> = propres.iter().map(|(t, d)| (*t, (true, *d))).collect();
    for t in propres.keys() {
        for dy in -1..=1 {
            for dx in -1..=1 {
                let v = TileId { ix: t.ix + dx, iy: t.iy + dy };
                tous.entry(v).or_insert((false, f32::NAN));
            }
        }
    }
    let tiles: Vec<TileId> = tous.keys().copied().collect();
    let pos = tiles.iter().enumerate().map(|(k, t)| (*t, k)).collect();
    let propre = tous.values().map(|v| v.0).collect();
    let dist_cote_km = tous.values().map(|v| v.1).collect();
    let etranger = tiles
        .iter()
        .map(|t| {
            let (i0, j0) = (((t.x0() - x0) / res).round() as i64, ((t.y0() - y0) / res).round() as i64);
            std::array::from_fn(|c| {
                let (i, j) = (i0 + (c % 10) as i64, j0 + (c / 10) as i64);
                i >= 0 && j >= 0 && i < nx as i64 && j < ny as i64 && {
                    let k = j as usize * nx + i as usize;
                    !terre[k] && !mer[k]
                }
            })
        })
        .collect();
    Ok(Monde { tiles, pos, propre, dist_cote_km, etranger })
}

// ------------------------------------------------------------ 4. Répartition

fn tiles_de(b: [f64; 4], marge: f64) -> impl Iterator<Item = TileId> {
    let (tx0, tx1) = (((b[0] - marge) / 1000.0).floor() as i32, ((b[2] + marge) / 1000.0).floor() as i32);
    let (ty0, ty1) = (((b[1] - marge) / 1000.0).floor() as i32, ((b[3] + marge) / 1000.0).floor() as i32);
    (ty0..=ty1).flat_map(move |iy| (tx0..=tx1).map(move |ix| TileId { ix, iy }))
}

fn bbox_polys(ps: &[Polygone]) -> [f64; 4] {
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in ps {
        for a in p {
            for v in a {
                b = [b[0].min(v[0]), b[1].min(v[1]), b[2].max(v[0]), b[3].max(v[1])];
            }
        }
    }
    b
}

fn bbox_ligne(l: &Ligne) -> [f64; 4] {
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for v in l {
        b = [b[0].min(v[0]), b[1].min(v[1]), b[2].max(v[0]), b[3].max(v[1])];
    }
    b
}

fn ranger(m: &Monde, n: usize, marge: f64, bbox: impl Fn(usize) -> [f64; 4]) -> Vec<Vec<u32>> {
    let mut l = vec![Vec::new(); m.n()];
    for k in 0..n {
        let b = bbox(k);
        if b[0] > b[2] {
            continue;
        }
        for t in tiles_de(b, marge) {
            if let Some(&i) = m.pos.get(&t) {
                l[i].push(k as u32);
            }
        }
    }
    l
}

/// Un morceau de route ou de voie ferrée, découpé aux bords des tiles.
pub struct Morceau {
    pub src: u32,
    pub rang: u16,
    pub pts: Vec<[f64; 3]>,
}

/// Découpe une polyligne aux bords des tiles. Les points de coupe sont
/// calculés une seule fois, en coordonnées globales : les deux tiles ont le
/// même point. Chaque sous-segment va à la tile de son milieu.
pub fn couper(l: &Ligne) -> Vec<(TileId, Vec<[f64; 3]>)> {
    let mut out: Vec<(TileId, Vec<[f64; 3]>)> = Vec::new();
    for s in l.windows(2) {
        let (a, b) = (s[0], s[1]);
        let mut ts = vec![0.0, 1.0];
        for ax in 0..2 {
            let (u, v) = (a[ax], b[ax]);
            if u == v {
                continue;
            }
            let (lo, hi) = (u.min(v), u.max(v));
            let mut k = (lo / 1000.0).floor() as i64 + 1;
            while (k as f64) * 1000.0 < hi {
                let t = ((k as f64) * 1000.0 - u) / (v - u);
                if t > 0.0 && t < 1.0 {
                    ts.push(t);
                }
                k += 1;
            }
        }
        ts.sort_by(f64::total_cmp);
        ts.dedup();
        let p = |t: f64| -> [f64; 3] {
            if t == 0.0 {
                a
            } else if t == 1.0 {
                b
            } else {
                [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1]), a[2] + t * (b[2] - a[2])]
            }
        };
        for w in ts.windows(2) {
            let (pa, pb) = (p(w[0]), p(w[1]));
            let tm = TileId::de((pa[0] + pb[0]) / 2.0, (pa[1] + pb[1]) / 2.0);
            match out.last_mut() {
                Some((t, pts)) if *t == tm && pts.last() == Some(&pa) => pts.push(pb),
                _ => out.push((tm, vec![pa, pb])),
            }
        }
    }
    out
}

pub fn longueur(l: &[[f64; 3]]) -> f64 {
    l.windows(2).map(|s| ((s[1][0] - s[0][0]).powi(2) + (s[1][1] - s[0][1]).powi(2)).sqrt()).sum()
}

/// Pont (B-03) : un ouvrage « Pont » de la BD TOPO et ce qu'on en déduit.
pub struct Pont {
    pub id: u64,
    pub geom: Geom,
    pub tile: TileId,
    pub longueur: f64,
    pub largeur_dm: u16,
    pub importance: u8,
    pub nature_route: u8,
    pub poids_dt: u16,
    pub franchit: u8, // 1 eau, 2 route, 4 voie ferrée
    pub routes: u16,
    pub portees: Vec<u64>, // identifiants des tronçons portés
    pub voies: u16,
    pub toponyme: String,
}

fn echantillons(g: &Geom) -> Vec<[f64; 2]> {
    let mut v = Vec::new();
    let mut ligne = |l: &Ligne| {
        for s in l.windows(2) {
            let d = ((s[1][0] - s[0][0]).powi(2) + (s[1][1] - s[0][1]).powi(2)).sqrt();
            let n = (d / 2.0).ceil().max(1.0) as usize;
            for k in 0..=n {
                let t = k as f64 / n as f64;
                v.push([s[0][0] + t * (s[1][0] - s[0][0]), s[0][1] + t * (s[1][1] - s[0][1])]);
            }
        }
    };
    match g {
        Geom::Lignes(ls) => ls.iter().for_each(&mut ligne),
        Geom::Polygones(ps) => ps.iter().flat_map(|p| p.iter()).for_each(&mut ligne),
        Geom::Point(p) => v.push([p[0], p[1]]),
        Geom::Vide => {}
    }
    v
}

fn dist_seg(p: [f64; 2], l: &Ligne) -> f64 {
    let mut d = f64::MAX;
    for s in l.windows(2) {
        let (ax, ay, bx, by) = (s[0][0], s[0][1], s[1][0], s[1][1]);
        let (vx, vy) = (bx - ax, by - ay);
        let l2 = vx * vx + vy * vy;
        let t = if l2 > 0.0 { (((p[0] - ax) * vx + (p[1] - ay) * vy) / l2).clamp(0.0, 1.0) } else { 0.0 };
        let (dx, dy) = (p[0] - ax - t * vx, p[1] - ay - t * vy);
        d = d.min((dx * dx + dy * dy).sqrt());
    }
    d
}

pub fn dans_poly(p: [f64; 2], ps: &[Polygone]) -> bool {
    let mut dedans = false;
    for poly in ps {
        for a in poly {
            let k = a.len();
            for i in 0..k {
                let (u, v) = (a[i], a[(i + 1) % k]);
                if (u[1] <= p[1]) != (v[1] <= p[1]) {
                    let x = u[0] + (p[1] - u[1]) * (v[0] - u[0]) / (v[1] - u[1]);
                    if x > p[0] {
                        dedans = !dedans;
                    }
                }
            }
        }
    }
    dedans
}

pub struct Repartition {
    pub bat_r: Vec<Vec<u32>>, // bâtiments à rastériser (boîte)
    pub bat_p: Vec<Vec<u32>>, // bâtiments possédés (centroïde)
    pub bat_hors: usize,
    pub routes: Vec<Vec<Morceau>>,
    pub voies: Vec<Vec<Morceau>>,
    pub routes_r: Vec<Vec<u32>>, // routes à rastériser (critère de la zone)
    pub murs: Vec<Vec<u32>>,
    pub veg: Vec<Vec<u32>>,
    pub haies: Vec<Vec<u32>>,
    pub eaux_s: Vec<Vec<u32>>,
    pub eaux_l: Vec<Vec<u32>>,
    pub estran: Vec<Vec<u32>>,
    pub communes: Vec<Vec<u32>>,
    pub lieux: Vec<Vec<u32>>,
    pub ponts: Vec<Vec<Pont>>,
    pub points_pop: Vec<Vec<(f64, f64, u32)>>,
    pub longueur_routes_src: f64,
    pub longueur_routes_morceaux: f64,
    pub ponts_stats: BTreeMap<String, u64>,
}

pub fn repartir(d: &Donnees, m: &Monde, pop_points: &[(f64, f64, u32)]) -> Repartition {
    let bat_r = ranger(m, d.batiments.len(), 0.0, |k| bbox_polys(&d.batiments[k].polys));
    let mut bat_p = vec![Vec::new(); m.n()];
    let mut bat_hors = 0;
    for (k, b) in d.batiments.iter().enumerate() {
        match m.pos.get(&TileId::de(b.cx, b.cy)) {
            Some(&t) => bat_p[t].push(k as u32),
            None => bat_hors += 1,
        }
    }
    // Routes et voies ferrées découpées aux bords.
    let decoupe = |lignes: Vec<&Ligne>| -> (Vec<Vec<Morceau>>, f64, f64) {
        let morceaux: Vec<Vec<(TileId, Vec<[f64; 3]>)>> = lignes.par_iter().map(|l| couper(l)).collect();
        let mut par_tile: Vec<Vec<Morceau>> = (0..m.n()).map(|_| Vec::new()).collect();
        let (mut ls, mut lm) = (0.0, 0.0);
        for (src, ms) in morceaux.into_iter().enumerate() {
            ls += longueur(lignes[src]);
            for (rang, (t, pts)) in ms.into_iter().enumerate() {
                lm += longueur(&pts);
                if let Some(&i) = m.pos.get(&t) {
                    par_tile[i].push(Morceau { src: src as u32, rang: rang as u16, pts });
                }
            }
        }
        (par_tile, ls, lm)
    };
    let (routes, ls, lm) = decoupe(d.routes.iter().map(|r| &r.ligne).collect());
    let (voies, _, _) = decoupe(d.voies.iter().map(|r| &r.ligne).collect());
    let routes_r = ranger(m, d.routes.len(), 2.0, |k| {
        let r = &d.routes[k];
        if r.pos_sol < 0 || r.nature == 9 || r.nature == 11 { [1.0, 1.0, 0.0, 0.0] } else { bbox_ligne(&r.ligne) }
    });
    let murs = ranger(m, d.constructions.len(), 2.0, |k| {
        let c = &d.constructions[k];
        if matches!(c.nature.as_str(), "Mur" | "Mur anti-bruit") { c.geom.bbox() } else { [1.0, 1.0, 0.0, 0.0] }
    });
    let veg = ranger(m, d.vegetation.len(), 0.0, |k| bbox_polys(&d.vegetation[k].polys));
    let haies = ranger(m, d.haies.len(), 5.0, |k| bbox_ligne(&d.haies[k].ligne));
    let eaux_s = ranger(m, d.eaux_surf.len(), 0.0, |k| bbox_polys(&d.eaux_surf[k].polys));
    let eaux_l = ranger(m, d.eaux_lin.len(), 8.0, |k| bbox_ligne(&d.eaux_lin[k].ligne));
    let estran = ranger(m, d.estran.len(), 0.0, |k| bbox_polys(&d.estran[k].polys));
    let communes = ranger(m, d.communes.len(), 0.0, |k| bbox_polys(&d.communes[k].polys));
    let lieux_r = ranger(m, d.lieux.len(), 0.0, |k| {
        let b = d.lieux[k].geom.bbox();
        let (x, y) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        [x, y, x, y]
    });
    let mut points_pop = vec![Vec::new(); m.n()];
    for &(x, y, n) in pop_points {
        if let Some(&t) = m.pos.get(&TileId::de(x, y)) {
            points_pop[t].push((x, y, n));
        }
    }

    // Ponts : un ouvrage « Pont » ; routes dessus (position ≥ 1, à moins de
    // 5 m), ce qu'il franchit (eau, route ou voie au sol, à moins de 3 m).
    let routes_g = ranger(m, d.routes.len(), 0.0, |k| bbox_ligne(&d.routes[k].ligne));
    let voies_g = ranger(m, d.voies.len(), 0.0, |k| bbox_ligne(&d.voies[k].ligne));
    let ouvrages: Vec<usize> = (0..d.constructions.len()).filter(|&k| d.constructions[k].nature == "Pont").collect();
    let ponts_v: Vec<Option<Pont>> = ouvrages
        .par_iter()
        .map(|&k| {
            let c = &d.constructions[k];
            let b = c.geom.bbox();
            let tile = TileId::de((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
            let &ti = m.pos.get(&tile)?;
            let _ = ti;
            let ech = echantillons(&c.geom);
            let mut cand_r = BTreeSet::new();
            let mut cand_v = BTreeSet::new();
            let mut cand_el = BTreeSet::new();
            let mut cand_es = BTreeSet::new();
            for t in tiles_de(b, 5.0) {
                if let Some(&i) = m.pos.get(&t) {
                    cand_r.extend(routes_g[i].iter().copied());
                    cand_v.extend(voies_g[i].iter().copied());
                    cand_el.extend(eaux_l[i].iter().copied());
                    cand_es.extend(eaux_s[i].iter().copied());
                }
            }
            let mut portees = BTreeSet::new();
            let pres = |l: &Ligne, s: f64| ech.iter().any(|p| dist_seg(*p, l) <= s);
            let (mut largeur_dm, mut importance, mut nature_route, mut poids_dt, mut nr, mut nv, mut franchit) =
                (0u16, 9u8, 255u8, 0u16, 0u16, 0u16, 0u8);
            for &r in &cand_r {
                let r = &d.routes[r as usize];
                if r.pos_sol >= 1 && pres(&r.ligne, 5.0) {
                    nr += 1;
                    portees.insert(r.id);
                    largeur_dm = largeur_dm.max(r.largeur_dm);
                    if r.importance < importance {
                        importance = r.importance;
                        nature_route = r.nature;
                    }
                    if r.poids_dt > 0 {
                        poids_dt = if poids_dt == 0 { r.poids_dt } else { poids_dt.min(r.poids_dt) };
                    }
                } else if r.pos_sol == 0 && pres(&r.ligne, 3.0) {
                    franchit |= 2;
                }
            }
            for &v in &cand_v {
                let v = &d.voies[v as usize];
                if v.pos_sol >= 1 && pres(&v.ligne, 5.0) {
                    nv += 1;
                } else if v.pos_sol == 0 && pres(&v.ligne, 3.0) {
                    franchit |= 4;
                }
            }
            for &e in &cand_el {
                if pres(&d.eaux_lin[e as usize].ligne, 3.0) {
                    franchit |= 1;
                }
            }
            if franchit & 1 == 0 {
                for &e in &cand_es {
                    if ech.iter().any(|p| dans_poly(*p, &d.eaux_surf[e as usize].polys)) {
                        franchit |= 1;
                        break;
                    }
                }
            }
            let longueur = match &c.geom {
                Geom::Lignes(ls) => ls.iter().map(|l| longueur(l)).sum(),
                _ => (b[2] - b[0]).max(b[3] - b[1]),
            };
            Some(Pont {
                id: c.id,
                geom: c.geom.clone(),
                tile,
                longueur,
                largeur_dm,
                importance,
                nature_route,
                poids_dt,
                franchit,
                routes: nr,
                portees: portees.into_iter().collect(),
                voies: nv,
                toponyme: c.toponyme.clone(),
            })
        })
        .collect();
    let mut ponts: Vec<Vec<Pont>> = (0..m.n()).map(|_| Vec::new()).collect();
    let mut st: BTreeMap<String, u64> = BTreeMap::new();
    for p in ponts_v.into_iter().flatten() {
        let i = m.pos[&p.tile];
        if m.propre[i] {
            *st.entry("ponts".into()).or_default() += 1;
            *st.entry(format!(
                "franchit_{}",
                ["rien", "eau", "route", "eau+route", "voie", "eau+voie", "route+voie", "tout"][p.franchit as usize]
            ))
            .or_default() += 1;
            *st.entry(format!("routes_dessus_{}", p.routes.min(3))).or_default() += 1;
            if p.voies > 0 {
                *st.entry("porte_voie_ferree".into()).or_default() += 1;
            }
            if p.poids_dt > 0 {
                *st.entry("restriction_poids".into()).or_default() += 1;
            }
            if p.largeur_dm > 0 {
                *st.entry("largeur_connue".into()).or_default() += 1;
            }
            *st.entry(format!("importance_{}", p.importance)).or_default() += 1;
        }
        ponts[i].push(p);
    }
    // Tronçons sur ouvrage sans pont associé.
    for (i, rs) in routes.iter().enumerate() {
        if !m.propre[i] {
            continue;
        }
        for mo in rs {
            let r = &d.routes[mo.src as usize];
            if r.pos_sol >= 1 && mo.rang == 0 {
                *st.entry("troncons_sur_ouvrage".into()).or_default() += 1;
                if r.poids_dt > 0 {
                    *st.entry("troncons_sur_ouvrage_avec_poids".into()).or_default() += 1;
                }
            }
            if mo.rang == 0 {
                *st.entry("troncons".into()).or_default() += 1;
                if r.poids_dt > 0 {
                    *st.entry("troncons_avec_poids".into()).or_default() += 1;
                }
            }
        }
    }
    Repartition {
        bat_r,
        bat_p,
        bat_hors,
        routes,
        voies,
        routes_r,
        murs,
        veg,
        haies,
        eaux_s,
        eaux_l,
        estran,
        communes,
        lieux: lieux_r,
        ponts,
        points_pop,
        longueur_routes_src: ls,
        longueur_routes_morceaux: lm,
        ponts_stats: st,
    }
}

/// Mairie de chaque commune : la zone « Mairie » principale (sans détail ou
/// « Hôtel de ville »), la plus grande si plusieurs ; son bâtiment est le plus
/// grand bâtiment dont le centroïde est dans la zone, sinon le plus proche à
/// moins de 100 m. Renvoie commune → (id du lieu, indice du bâtiment).
pub fn mairies(d: &Donnees, m: &Monde, r: &Repartition) -> BTreeMap<String, (u64, Option<usize>)> {
    let mut par_commune: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (k, l) in d.lieux.iter().enumerate() {
        if l.type_ == 0 && !l.insee.is_empty() {
            par_commune.entry(l.insee.clone()).or_default().push(k);
        }
    }
    let mut out = BTreeMap::new();
    for (code, ks) in par_commune {
        let Some(k) = ks.iter().copied().max_by(|&a, &b| {
            let sa = match &d.lieux[a].geom {
                Geom::Polygones(p) => objets::surface_centroide(p).0,
                _ => 0.0,
            };
            let sb = match &d.lieux[b].geom {
                Geom::Polygones(p) => objets::surface_centroide(p).0,
                _ => 0.0,
            };
            sa.total_cmp(&sb).then(d.lieux[b].id.cmp(&d.lieux[a].id))
        }) else {
            continue;
        };
        let l = &d.lieux[k];
        let b = l.geom.bbox();
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let mut meilleur: Option<(f64, u64, usize)> = None;
        let mut proche: Option<(f64, u64, usize)> = None;
        for t in tiles_de(b, 100.0) {
            if let Some(&i) = m.pos.get(&t) {
                for &bk in &r.bat_p[i] {
                    let bt = &d.batiments[bk as usize];
                    if let Geom::Polygones(ps) = &l.geom
                        && dans_poly([bt.cx, bt.cy], ps)
                    {
                        let c = (bt.emprise, u64::MAX - bt.id, bk as usize);
                        if meilleur.is_none_or(|m| (c.0, c.1) > (m.0, m.1)) {
                            meilleur = Some(c);
                        }
                    }
                    let dd = ((bt.cx - cx).powi(2) + (bt.cy - cy).powi(2)).sqrt();
                    if dd <= 100.0 {
                        let c = (dd, bt.id, bk as usize);
                        if proche.is_none_or(|m| (c.0, c.1) < (m.0, m.1)) {
                            proche = Some(c);
                        }
                    }
                }
            }
        }
        out.insert(code, (l.id, meilleur.or(proche).map(|m| m.2)));
    }
    out
}

// ------------------------------------------------------------ 6. Passe 1

pub const OCC_EAU: u8 = 16;
pub const OCC_ESTRAN: u8 = 32;
pub const OCC_MER: u8 = 64;
pub const OCC_SALEE: u8 = 128;
/// Hors de France (gris, infranchissable) : quartet bas à 15.
pub const OCC_ETRANGER: u8 = 15;

pub const ETAPES_TILE: &[&str] =
    &["bati", "franch", "communes", "occupation", "composantes", "encodage", "zone", "relief", "assemblage"];

pub struct P1 {
    pub franch: Bitmap,
    pub mer: Bitmap,
    pub resume: zone::Resume,
    pub sections: Vec<Section>,
    pub nb_bat: u32,
    pub part_bati: f64,
    pub cases_mer: usize,
    pub cases_hors: usize,
    pub rues: Bitmap,
    pub habitants: u32,
    pub hab_points: u32,
    pub fragments: Vec<(u32, u32, u32)>,
    pub plus_grand: Vec<(u32, f64, u64, usize)>,
    pub adj: BTreeSet<(u32, u32)>,
    pub comm_bords: [Vec<u32>; 4],
    pub murs_cases: usize,
    pub ms: [f64; 9],
}

fn enc_poly(e: &mut Ecrivain, p: &Polygone, cx: f64, cy: f64) {
    e.u(p.len() as u64);
    for a in p {
        // Anneau fermé en GeoPackage : le dernier point répète le premier.
        let n = if a.len() > 1 && a[0] == a[a.len() - 1] { a.len() - 1 } else { a.len() };
        e.u(n as u64);
        let (mut px, mut py) = (0i64, 0i64);
        for v in &a[..n] {
            let (x, y) = (q(v[0], cx), q(v[1], cy));
            e.i(x - px);
            e.i(y - py);
            (px, py) = (x, y);
        }
    }
}

fn enc_ligne(e: &mut Ecrivain, l: &[[f64; 3]], cx: f64, cy: f64, z: bool) {
    e.u(l.len() as u64);
    let (mut px, mut py, mut pz) = (0i64, 0i64, 0i64);
    for v in l {
        let (x, y) = (q(v[0], cx), q(v[1], cy));
        e.i(x - px);
        e.i(y - py);
        (px, py) = (x, y);
        if z {
            let zz = (v[2] * 10.0).round() as i64;
            e.i(zz - pz);
            pz = zz;
        }
    }
}

pub fn enc_geom(e: &mut Ecrivain, g: &Geom, cx: f64, cy: f64) {
    match g {
        Geom::Vide => e.o(0),
        Geom::Point(p) => {
            e.o(1);
            e.i(q(p[0], cx));
            e.i(q(p[1], cy));
        }
        Geom::Lignes(ls) => {
            e.o(2);
            e.u(ls.len() as u64);
            for l in ls {
                enc_ligne(e, l, cx, cy, false);
            }
        }
        Geom::Polygones(ps) => {
            e.o(3);
            e.u(ps.len() as u64);
            for p in ps {
                enc_poly(e, p, cx, cy);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn passe1(
    t: TileId,
    i: usize,
    d: &Donnees,
    r: &Repartition,
    p: &Params,
    hab_bat: &HashMap<usize, u32>,
    codes: &[u32],
    etranger: &[bool; 100],
) -> Res<P1> {
    let mut chrono = [0f64; 9];
    let (x0, y0) = (t.x0(), t.y0());
    let (cx, cy) = t.centre();
    let w1 = Fenetre::tile(x0, y0, 1.0);
    let w2 = Fenetre::tile(x0, y0, 2.0);

    // Bâti à 1 m.
    let t0 = Instant::now();
    let mut bati1 = Bitmap::new(1000);
    for &k in &r.bat_r[i] {
        for poly in &d.batiments[k as usize].polys {
            raster::polygone(w1, poly, |a, b| bati1.set(a, b));
        }
    }
    let part_bati = bati1.compte() as f64 / 1e6;
    chrono[0] = ms(t0);

    // Franchissable à 2 m : majorité, moins les murs.
    let t0 = Instant::now();
    let mut franch = raster::majorite_2m(&bati1);
    let mut murs_cases = 0;
    if p.murs {
        for &k in &r.murs[i] {
            if let Geom::Lignes(ls) = &d.constructions[k as usize].geom {
                for l in ls {
                    raster::ligne(w2, l, 1.5, |a, b| {
                        if franch.get(a, b) {
                            franch.clear(a, b);
                            murs_cases += 1;
                        }
                    });
                }
            }
        }
    }
    drop(bati1);
    chrono[1] = ms(t0);

    // Communes à 2 m : indice dans la liste des communes, u16::MAX = mer.
    let t0 = Instant::now();
    let mut comm = vec![u16::MAX; 250_000];
    for &k in &r.communes[i] {
        for poly in &d.communes[k as usize].polys {
            raster::polygone(w2, poly, |a, b| comm[b * 500 + a] = k as u16);
        }
    }
    let mut mer = Bitmap::new(500);
    let (mut cases_mer, mut cases_hors) = (0, 0);
    let dehors = |k: usize| etranger[k / 500 / 50 * 10 + k % 500 / 50];
    for (k, &c) in comm.iter().enumerate().take(250_000) {
        if c == u16::MAX {
            mer.set(k % 500, k / 500);
            cases_hors += 1;
            if dehors(k) {
                franch.clear(k % 500, k / 500);
            } else {
                cases_mer += 1;
            }
        }
    }
    // Fragments : communes présentes, par nombre de cases.
    let mut frag: BTreeMap<u16, u32> = BTreeMap::new();
    for &c in &comm {
        if c != u16::MAX {
            *frag.entry(c).or_default() += 1;
        }
    }
    let frag_l: Vec<u16> = frag.keys().copied().collect();
    let mut hab_frag = vec![0u32; frag_l.len()];
    let fragment_de = |x: f64, y: f64| -> Option<usize> {
        let (a, b) = (((x - x0) / 2.0).floor() as i64, ((y - y0) / 2.0).floor() as i64);
        let c =
            if (0..500).contains(&a) && (0..500).contains(&b) { comm[b as usize * 500 + a as usize] } else { u16::MAX };
        if c != u16::MAX {
            frag_l.iter().position(|&f| f == c)
        } else if let Some((mx, _)) = frag.iter().max_by_key(|(c, n)| (**n, std::cmp::Reverse(**c))) {
            // Centroïde en mer (digue, quai) : le plus grand fragment.
            frag_l.iter().position(|f| f == mx)
        } else {
            None
        }
    };
    let mut habitants = 0u32;
    let mut plus_grand: BTreeMap<u16, (f64, u64, usize)> = BTreeMap::new();
    for &k in &r.bat_p[i] {
        let b = &d.batiments[k as usize];
        let f = fragment_de(b.cx, b.cy);
        if let Some(f) = f {
            if let Some(&h) = hab_bat.get(&(k as usize)) {
                hab_frag[f] += h;
                habitants += h;
            }
            let c = frag_l[f];
            let e = plus_grand.entry(c).or_insert((0.0, 0, 0));
            if (b.emprise, u64::MAX - b.id) > (e.0, u64::MAX - e.1) {
                *e = (b.emprise, b.id, k as usize);
            }
        }
    }
    let mut hab_points = 0u32;
    for &(x, y, n) in &r.points_pop[i] {
        if let Some(f) = fragment_de(x, y) {
            hab_frag[f] += n;
            habitants += n;
            hab_points += n;
        }
    }
    // Adjacence des communes (cases voisines de communes différentes).
    let mut adj = BTreeSet::new();
    for b in 0..500 {
        for a in 0..500 {
            let c = comm[b * 500 + a];
            if c == u16::MAX {
                continue;
            }
            for (da, db) in [(1usize, 0usize), (0, 1)] {
                let (aa, bb) = (a + da, b + db);
                if aa < 500 && bb < 500 {
                    let c2 = comm[bb * 500 + aa];
                    if c2 != u16::MAX && c2 != c {
                        let (u, v) = (codes[c as usize], codes[c2 as usize]);
                        adj.insert((u.min(v), u.max(v)));
                    }
                }
            }
        }
    }
    let cc = |k: u16| if k == u16::MAX { u32::MAX } else { codes[k as usize] };
    let comm_bords = [
        (0..500).map(|b| cc(comm[b * 500])).collect(),
        (0..500).map(|b| cc(comm[b * 500 + 499])).collect(),
        (0..500).map(|a| cc(comm[a])).collect(),
        (0..500).map(|a| cc(comm[499 * 500 + a])).collect(),
    ];
    chrono[2] = ms(t0);

    // Occupation du sol à 2 m.
    let t0 = Instant::now();
    let mut occ = vec![0u8; 250_000];
    for &k in &r.veg[i] {
        let v = &d.vegetation[k as usize];
        for poly in &v.polys {
            raster::polygone(w2, poly, |a, b| occ[b * 500 + a] = (occ[b * 500 + a] & 0xF0) | v.classe);
        }
    }
    for &k in &r.haies[i] {
        let h = &d.haies[k as usize];
        raster::ligne(w2, &h.ligne, h.demi.max(1.0), |a, b| {
            if occ[b * 500 + a] & 0x0F == 0 {
                occ[b * 500 + a] |= 5;
            }
        });
    }
    for &k in &r.eaux_s[i] {
        let e = &d.eaux_surf[k as usize];
        let bit = if e.classe == 2 { OCC_SALEE } else { OCC_EAU };
        for poly in &e.polys {
            raster::polygone(w2, poly, |a, b| occ[b * 500 + a] |= bit);
        }
    }
    for &k in &r.eaux_l[i] {
        let e = &d.eaux_lin[k as usize];
        raster::ligne(w2, &e.ligne, e.demi.max(1.0), |a, b| occ[b * 500 + a] |= OCC_EAU);
    }
    for &k in &r.estran[i] {
        for poly in &d.estran[k as usize].polys {
            raster::polygone(w2, poly, |a, b| occ[b * 500 + a] |= OCC_ESTRAN);
        }
    }
    for k in 0..250_000 {
        if comm[k] == u16::MAX {
            occ[k] = if dehors(k) { OCC_ETRANGER } else { occ[k] | OCC_MER };
        }
    }
    chrono[3] = ms(t0);

    // Composantes de la zone (passe 1).
    let t0 = Instant::now();
    let mut cand = franch.clone();
    for (m1, m2) in cand.mots.iter_mut().zip(&mer.mots) {
        *m1 &= !m2;
    }
    let mut routes2 = Bitmap::new(500);
    for &k in &r.routes_r[i] {
        raster::ligne(w2, &d.routes[k as usize].ligne, 1.5, |a, b| routes2.set(a, b));
    }
    let mut rues = Bitmap::new(500);
    for &k in &r.routes_r[i] {
        let ro = &d.routes[k as usize];
        if ro.nature <= 6 && ro.pos_sol == 0 {
            raster::ligne(w2, &ro.ligne, 1.5, |a, b| rues.set(a, b));
        }
    }
    let et = zone::etiqueter(&cand);
    let resume = zone::resumer(&et, &routes2);
    drop(et);
    chrono[4] = ms(t0);

    // Encodage des couches.
    let t0 = Instant::now();
    let mut sections = Vec::new();
    // Réseau : routes puis voies ferrées, morceaux triés (identifiant, rang).
    {
        let mut e = Ecrivain::default();
        let mut rs: Vec<&Morceau> = r.routes[i].iter().collect();
        rs.sort_by_key(|m| (d.routes[m.src as usize].id, m.rang));
        e.u(rs.len() as u64);
        let mut pid = 0u64;
        for mo in &rs {
            let ro = &d.routes[mo.src as usize];
            e.i(ro.id as i64 - pid as i64);
            pid = ro.id;
            e.u(mo.rang as u64);
            e.o(ro.nature);
            e.o(ro.importance);
            e.o(ro.pos_sol as u8);
            e.u(ro.largeur_dm as u64);
            e.o(ro.voies);
            e.o(ro.sens);
            e.o((ro.acces_vl & 0x3F) | (ro.fictif as u8) << 6 | (ro.urbain as u8) << 7);
            e.o(ro.vitesse);
            e.u(ro.poids_dt as u64);
            e.u(ro.hauteur_dm as u64);
            e.u(ro.largeur_r_dm as u64);
            enc_ligne(&mut e, &mo.pts, cx, cy, ro.pos_sol != 0);
        }
        let mut vs: Vec<&Morceau> = r.voies[i].iter().collect();
        vs.sort_by_key(|m| (d.voies[m.src as usize].id, m.rang));
        e.u(vs.len() as u64);
        let mut pid = 0u64;
        for mo in &vs {
            let v = &d.voies[mo.src as usize];
            e.i(v.id as i64 - pid as i64);
            pid = v.id;
            e.u(mo.rang as u64);
            e.o(v.nature);
            e.o(v.pos_sol as u8);
            e.o(v.electrifiee as u8);
            e.o(v.voies);
            enc_ligne(&mut e, &mo.pts, cx, cy, v.pos_sol != 0);
        }
        if !rs.is_empty() || !vs.is_empty() {
            sections.push(tuile::compresser(Sec::Reseau, &e.b, p.niveau)?);
        }
    }
    if !r.ponts[i].is_empty() {
        let mut e = Ecrivain::default();
        let mut ps: Vec<&Pont> = r.ponts[i].iter().collect();
        ps.sort_by_key(|p| p.id);
        e.u(ps.len() as u64);
        for po in ps {
            e.u(po.id);
            e.u((po.longueur * 10.0).round() as u64);
            e.u(po.largeur_dm as u64);
            e.o(po.importance);
            e.o(po.nature_route);
            e.u(po.poids_dt as u64);
            e.o(po.franchit);
            e.u(po.routes as u64);
            e.u(po.voies as u64);
            e.s(&po.toponyme);
            e.u(po.portees.len() as u64);
            let mut pr = 0;
            for id in &po.portees {
                e.u(id - pr);
                pr = *id;
            }
            enc_geom(&mut e, &po.geom, cx, cy);
        }
        sections.push(tuile::compresser(Sec::Ponts, &e.b, p.niveau)?);
    }
    sections.push(tuile::compresser(Sec::Occup, &occ, p.niveau)?);
    {
        let mut e = Ecrivain::default();
        e.u(frag_l.len() as u64);
        for (k, c) in frag_l.iter().enumerate() {
            e.u(codes[*c as usize] as u64);
            e.u(frag[c] as u64);
            e.u(hab_frag[k] as u64);
        }
        if frag_l.len() > 1 {
            // Carte des fragments à 2 m (indice local, 255 = mer).
            let carte: Vec<u8> = comm
                .iter()
                .map(
                    |&c| if c == u16::MAX { 255 } else { frag_l.iter().position(|&f| f == c).map_or(255, |k| k as u8) },
                )
                .collect();
            e.b.extend_from_slice(&carte);
        } else if frag_l.len() == 1 && cases_hors > 0 {
            e.b.extend_from_slice(&mer.octets());
        }
        sections.push(tuile::compresser(Sec::Communes, &e.b, p.niveau)?);
    }
    let lieux: Vec<usize> = r.lieux[i].iter().map(|&k| k as usize).collect();
    if !lieux.is_empty() {
        let mut e = Ecrivain::default();
        e.u(lieux.len() as u64);
        for k in lieux {
            let l = &d.lieux[k];
            e.o(l.type_);
            e.u(l.id);
            e.s(&l.nom);
            e.s(&l.detail);
            enc_geom(&mut e, &l.geom, cx, cy);
        }
        sections.push(tuile::compresser(Sec::Lieux, &e.b, p.niveau)?);
    }
    chrono[5] = ms(t0);
    Ok(P1 {
        franch,
        mer,
        resume,
        sections,
        nb_bat: r.bat_p[i].len() as u32,
        part_bati,
        cases_mer,
        cases_hors,
        rues,
        habitants,
        hab_points,
        fragments: frag_l.iter().enumerate().map(|(k, c)| (codes[*c as usize], frag[c], hab_frag[k])).collect(),
        plus_grand: plus_grand.into_iter().map(|(c, v)| (codes[c as usize], v.0, v.1, v.2)).collect(),
        adj,
        comm_bords,
        murs_cases,
        ms: chrono,
    })
}

// ------------------------------------------------------------ 7. Fusion

pub struct Fusion {
    pub base: Vec<u32>,
    pub accessible: Vec<bool>, // par racine
    pub uf: zone::UnionFind,
    pub composantes: usize,
    pub accessibles: usize,
    pub cases_accessibles: u64,
    pub cases_candidates: u64,
}

pub fn fusion(m: &Monde, p1: &[P1]) -> Fusion {
    let mut base = Vec::with_capacity(m.n());
    let mut n = 0u32;
    for x in p1 {
        base.push(n);
        n += x.resume.tailles.len() as u32;
    }
    let mut uf = zone::UnionFind::new(n as usize);
    for (i, t) in m.tiles.iter().enumerate() {
        if let Some(&e) = m.pos.get(&TileId { ix: t.ix + 1, iy: t.iy }) {
            for j in 0..zone::N {
                let (a, b) = (p1[i].resume.est[j], p1[e].resume.ouest[j]);
                if a != zone::AUCUNE && b != zone::AUCUNE {
                    uf.unir(base[i] + a, base[e] + b);
                }
            }
        }
        if let Some(&nn) = m.pos.get(&TileId { ix: t.ix, iy: t.iy + 1 }) {
            for k in 0..zone::N {
                let (a, b) = (p1[i].resume.nord[k], p1[nn].resume.sud[k]);
                if a != zone::AUCUNE && b != zone::AUCUNE {
                    uf.unir(base[i] + a, base[nn] + b);
                }
            }
        }
    }
    let mut taille = vec![0u64; n as usize];
    let mut route = vec![false; n as usize];
    for (i, x) in p1.iter().enumerate() {
        for (l, &s) in x.resume.tailles.iter().enumerate() {
            let r = uf.trouver(base[i] + l as u32) as usize;
            taille[r] += s as u64;
            route[r] |= x.resume.route[l];
        }
    }
    let plus_grande = (0..n as usize).max_by_key(|&k| (taille[k], std::cmp::Reverse(k))).unwrap_or(0);
    let accessible: Vec<bool> = (0..n as usize).map(|k| route[k] || k == plus_grande).collect();
    let mut composantes = 0;
    let mut accessibles = 0;
    let (mut ca, mut cc) = (0u64, 0u64);
    for k in 0..n as usize {
        if uf.p[k] == k as u32 {
            composantes += 1;
            cc += taille[k];
            if accessible[k] {
                accessibles += 1;
                ca += taille[k];
            }
        }
    }
    Fusion { base, accessible, uf, composantes, accessibles, cases_accessibles: ca, cases_candidates: cc }
}

// ------------------------------------------------------------ 8. Passe 2

pub struct Sortie {
    pub octets: Vec<u8>,
    pub tailles: Vec<(Sec, usize, usize)>,
    pub bati: StatBati,
    pub cours: u32,
    pub cours_percees: u32,
    pub cases_exclues: u32,
    pub cases_passages: u32,
    pub cases_zone: u32,
    pub ms: [f64; 3],
}

/// Zone finale d'une tile (toutes, halo compris : les portes en ont besoin).
pub struct Zone2 {
    pub franch: Bitmap,
    pub exclus: Bitmap,
    pub cases: Vec<u32>,
    pub cours: u32,
    pub cours_percees: u32,
    pub nex: u32,
    pub nzone: u32,
    pub ms: f64,
}

pub fn passe2_zone(i: usize, p1: &P1, f: &Fusion, racines: &[u32], p: &Params) -> Zone2 {
    let t0 = Instant::now();
    let mut cand = p1.franch.clone();
    for (m1, m2) in cand.mots.iter_mut().zip(&p1.mer.mots) {
        *m1 &= !m2;
    }
    let et = zone::etiqueter(&cand);
    let dans: Vec<bool> =
        (0..et.tailles.len()).map(|l| f.accessible[racines[(f.base[i] + l as u32) as usize] as usize]).collect();
    // Obstacles perçables : ni candidats, ni mer (bâti, murs).
    let mut obst = Bitmap::new(500);
    for k in 0..obst.mots.len() {
        obst.mots[k] = !cand.mots[k] & !p1.mer.mots[k];
    }
    let (cases, relie) = zone::percer(&et.lab, &dans, &et.tailles, &obst, p.cour_min, p.passage_max);
    let mut franch = p1.franch.clone();
    for &k in &cases {
        franch.set(k as usize % 500, k as usize / 500);
    }
    let mut exclus = Bitmap::new(500);
    let (mut nex, mut nzone) = (0u32, 0u32);
    for (k, &l) in et.lab.iter().enumerate().take(250_000) {
        if l == zone::AUCUNE {
            continue;
        }
        if dans[l as usize] || relie[l as usize] {
            nzone += 1;
        } else {
            exclus.set(k % 500, k / 500);
            nex += 1;
        }
    }
    let cours = (0..et.tailles.len()).filter(|&l| !dans[l] && et.tailles[l] >= p.cour_min).count() as u32;
    let cours_percees = relie.iter().filter(|x| **x).count() as u32;
    Zone2 { franch, exclus, cases, cours, cours_percees, nex, nzone, ms: ms(t0) }
}

#[derive(Default, Clone, Copy)]
pub struct StatBati {
    pub nettoyes: u32,
    pub vides: u32,
    pub portes: [u32; 4],
    pub sans_porte: u32,
    pub toit: u32,
    pub materiaux: u32,
}

/// Bâtiments possédés, triés par identifiant : attributs, porte, emprise.
fn enc_bati(t: TileId, i: usize, d: &Donnees, r: &Repartition, g: &portes::Grille) -> (Vec<u8>, StatBati) {
    let (cx, cy) = t.centre();
    let mut st = StatBati::default();
    let mut bats: Vec<&Batiment> = r.bat_p[i].iter().map(|&k| &d.batiments[k as usize]).collect();
    bats.sort_by_key(|b| b.id);
    let mut e = Ecrivain::default();
    e.u(bats.len() as u64);
    let mut pid = 0u64;
    for b in &bats {
        let (polys, net) = emprise::quantifier(&b.polys, cx, cy);
        st.nettoyes += net as u32;
        st.vides += polys.iter().all(|p| p.first().is_none_or(|a| a.len() < 3)) as u32;
        let porte = portes::choisir(&polys, [b.cx - cx, b.cy - cy], g);
        match porte {
            Some(p) => st.portes[p.2 as usize] += 1,
            None => st.sans_porte += 1,
        }
        st.toit += b.toit_dm.is_some() as u32;
        st.materiaux += b.materiaux.is_some() as u32;
        e.u(b.id - pid);
        pid = b.id;
        e.o(b.nature);
        e.o(b.usage1);
        e.o(b.usage2);
        e.o(b.leger as u8
            | (b.hauteur_dm.is_some() as u8) << 1
            | (b.etages.is_some() as u8) << 2
            | (b.logements.is_some() as u8) << 3
            | (b.toit_dm.is_some() as u8) << 4
            | (b.materiaux.is_some() as u8) << 5
            | (porte.is_some() as u8) << 6);
        for v in [b.hauteur_dm, b.etages, b.logements, b.toit_dm].into_iter().flatten() {
            e.u(v as u64);
        }
        if let Some((m, to)) = b.materiaux {
            e.o(m);
            e.o(to);
        }
        if let Some((a, s, _)) = porte {
            e.u(a as u64);
            e.u(s as u64);
        }
        e.u(polys.len() as u64);
        for poly in &polys {
            e.u(poly.len() as u64);
            for a in poly {
                e.u(a.len() as u64);
                let (mut px, mut py) = (0i64, 0i64);
                for v in a {
                    e.i(v[0] - px);
                    e.i(v[1] - py);
                    (px, py) = (v[0], v[1]);
                }
            }
        }
    }
    (e.b, st)
}

#[allow(clippy::too_many_arguments)]
pub fn passe2(
    t: TileId,
    i: usize,
    m: &Monde,
    d: &Donnees,
    r: &Repartition,
    p1: &[P1],
    sections: Vec<Section>,
    z2: &[Zone2],
    rel: &BTreeMap<TileId, depot::ReliefTile>,
    p: &Params,
) -> Res<Sortie> {
    let t0 = Instant::now();
    let (x, z) = (&p1[i], &z2[i]);
    let mut sections = sections;
    let mut e = Ecrivain::default();
    e.o(!z.exclus.vide() as u8);
    if !z.exclus.vide() {
        e.b.extend_from_slice(&z.exclus.octets());
    }
    e.u(z.cases.len() as u64);
    let mut pk = 0u32;
    for &k in &z.cases {
        e.u((k - pk) as u64);
        pk = k;
    }
    sections.push(tuile::compresser(Sec::Zone, &e.b, p.niveau)?);
    sections.push(tuile::compresser(Sec::Franch, &z.franch.octets(), p.niveau)?);
    let mut bati = StatBati::default();
    if x.nb_bat > 0 {
        let v: [Option<portes::Vue>; 9] = std::array::from_fn(|k| {
            let j = *m.pos.get(&TileId { ix: t.ix + k as i32 % 3 - 1, iy: t.iy + k as i32 / 3 - 1 })?;
            Some(portes::Vue { franch: &z2[j].franch, exclus: &z2[j].exclus, hors: &p1[j].mer, rues: &p1[j].rues })
        });
        let (b, st) = enc_bati(t, i, d, r, &portes::grille(&v));
        bati = st;
        sections.push(tuile::compresser(Sec::Bati, &b, p.niveau)?);
    }
    let ms_zone = ms(t0) + z.ms;

    let t0 = Instant::now();
    let mut flags = 0u8;
    if x.cases_hors < 250_000 {
        flags |= 1;
    }
    if x.cases_mer > 0 {
        flags |= 2;
    }
    if x.cases_hors > x.cases_mer {
        flags |= 8;
    }
    if let Some(b) = rel.get(&t) {
        flags |= 4 | b.stat.source() << 4;
        let m2 = 501usize * 501;
        let brut = |z: &[u8], max: usize| zstd::bulk::decompress(z, max).map(|v| v.len()).ctx("relief : décompression");
        sections.push(Section { sec: Sec::Relief, brut: brut(&b.base, 16 + 7 * m2)?, donnees: b.base.clone() });
        if let Some(f) = &b.fin {
            sections.push(Section { sec: Sec::Relief1m, brut: brut(f, 2 + 2 * 1001 * 1001)?, donnees: f.clone() });
        }
    }
    let ms_rel = ms(t0);

    let t0 = Instant::now();
    sections.sort_by_key(|s| s.sec);
    let ent = Entete {
        ix: t.ix,
        iy: t.iy,
        zone: p.zone,
        flags,
        part_bati: (x.part_bati * 65535.0).round() as u16,
        habitants: x.habitants,
        nb_bat: x.nb_bat,
    };
    let octets = tuile::assembler(&ent, &sections);
    let tailles = sections.iter().map(|s| (s.sec, s.brut, s.donnees.len())).collect();
    let ms_ass = ms(t0);
    Ok(Sortie {
        octets,
        tailles,
        bati,
        cours: z.cours,
        cours_percees: z.cours_percees,
        cases_exclues: z.nex,
        cases_passages: z.cases.len() as u32,
        cases_zone: z.nzone,
        ms: [ms_zone, ms_rel, ms_ass],
    })
}

// ------------------------------------------------------------ Classement

/// Type de tile pour l'extrapolation : par habitants, la mer à part.
pub fn type_tile(terre: bool, mer: bool, habitants: u32, part_bati: f64) -> &'static str {
    if !terre {
        return "mer";
    }
    let base = match habitants {
        0 => "vide",
        1..=99 => "rural",
        100..=999 => "bourg",
        1000..=4999 => "urbain",
        _ => "dense",
    };
    if mer && base == "vide" && part_bati < 0.001 {
        return "cote";
    }
    base
}

pub fn classe_hab(h: f64) -> &'static str {
    match h as u64 {
        0 => "vide",
        1..=99 => "rural",
        100..=999 => "bourg",
        1000..=4999 => "urbain",
        _ => "dense",
    }
}

// ------------------------------------------------------------ Exécution complète

pub fn executer(p: &Params) -> Res<()> {
    let mut et = Etapes::new();
    std::fs::create_dir_all(&p.sortie)?;
    let dossier_paquets = p.sortie.join("paquets");
    let _ = std::fs::remove_dir_all(&dossier_paquets);
    std::fs::create_dir_all(&dossier_paquets)?;

    // 1. Lecture.
    let mut d = objets::lire(&p.gpkg)?;
    let mut cotes = Vec::new();
    if let Some(n) = &p.national {
        // Communes de la référence nationale : celles du département et des
        // alentours (assez loin pour attribuer les tiles de mer).
        let n = crate::national::lire(n)?;
        let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for c in n.communes.iter().filter(|c| c.dept == p.dept) {
            let x = bbox_polys(&c.polys);
            b = [b[0].min(x[0]), b[1].min(x[1]), b[2].max(x[2]), b[3].max(x[3])];
        }
        let mg = 2.0 * p.mer_km * 1000.0 + 4000.0;
        let pres = |x: [f64; 4]| x[0] < b[2] + mg && x[2] > b[0] - mg && x[1] < b[3] + mg && x[3] > b[1] - mg;
        d.communes = n.communes.into_iter().filter(|c| pres(bbox_polys(&c.polys))).collect();
        cotes = n.cotes.into_iter().filter(|l| pres(bbox_ligne(l))).collect();
        if !d.communes.iter().any(|c| c.dept == p.dept) {
            return Err(format!("département {} absent de la référence nationale", p.dept).into());
        }
    }
    et.fin("lecture");
    eprintln!(
        "  {} bâtiments ({} écartés), {} routes, {} voies, {} constructions, {} végétation, {} haies, {} eaux, {} communes, {} lieux",
        d.batiments.len(),
        d.bat_ecartes,
        d.routes.len(),
        d.voies.len(),
        d.constructions.len(),
        d.vegetation.len(),
        d.haies.len(),
        d.eaux_surf.len() + d.eaux_lin.len(),
        d.communes.len(),
        d.lieux.len()
    );

    // 2. Inventaire.
    let m = inventaire(&d, &cotes, p)?;
    let nprop = m.propre.iter().filter(|x| **x).count();
    et.fin("inventaire");
    eprintln!("  {} tiles du département, {} de halo", nprop, m.n() - nprop);

    // 3. Population.
    let conv = Laea2L93::new();
    let (rep_pop, carreaux_lus, carreaux_total) = match &p.insee200 {
        Some(c) => {
            let (Some(premiere), Some(derniere)) = (m.tiles.first(), m.tiles.last()) else {
                return Err("aucune tile à produire".into());
            };
            let (xa, ya) = (premiere.x0(), m.tiles.iter().map(|t| t.y0()).fold(f64::MAX, f64::min));
            let (xb, yb) = (derniere.x0() + 1000.0, m.tiles.iter().map(|t| t.y0()).fold(f64::MIN, f64::max) + 1000.0);
            let mut bl = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            for (x, y) in [(xa, ya), (xa, yb), (xb, ya), (xb, yb)] {
                let (lat, lon) = conv.l93.inverse(x, y);
                let (e, n) = conv.laea.avant(lat, lon);
                bl = [bl[0].min(e), bl[1].min(n), bl[2].max(e), bl[3].max(n)];
            }
            let (car, total) =
                crate::population::lire_200m(c, [bl[0] - 2000.0, bl[1] - 2000.0, bl[2] + 2000.0, bl[3] + 2000.0])?;
            let n = car.len();
            (Some(crate::population::repartir(&car, &d.batiments, Some(&conv))), n, total)
        }
        None => match &p.insee1k {
            Some(c) => {
                let car = crate::population::lire_1km(c)?;
                let n = car.len();
                (Some(crate::population::repartir(&car, &d.batiments, None)), n, n)
            }
            None => (Some(crate::population::par_commune(&d.communes, &p.dept, &d.batiments)), 0, 0),
        },
    };
    et.fin("population");
    let vide = HashMap::new();
    let hab_bat = rep_pop.as_ref().map(|r| &r.par_bat).unwrap_or(&vide);

    // 4. Répartition.
    let r = repartir(&d, &m, rep_pop.as_ref().map(|r| r.points.as_slice()).unwrap_or(&[]));
    let codes: Vec<u32> = d.communes.iter().map(|c| code_num(&c.code)).collect();
    let mairies = mairies(&d, &m, &r);
    et.fin("repartition");

    // 5. Relief.
    let dep = match &p.relief {
        Some(r) => depot::Depot::ouvrir(r, p.zone)?,
        None => depot::Depot::default(),
    };
    let propres: Vec<TileId> = m.tiles.iter().zip(&m.propre).filter(|x| *x.1).map(|x| *x.0).collect();
    let rel = depot::calculer(&dep, &propres, p.relief_q, p.niveau)?;
    et.fin("relief");
    eprintln!("  {} tiles avec relief ; sources {}", rel.len(), dep.sources());

    // 6. Passe 1.
    let mut p1: Vec<P1> = m
        .tiles
        .par_iter()
        .enumerate()
        .map(|(i, &t)| {
            let mut x = passe1(t, i, &d, &r, p, hab_bat, &codes, &m.etranger[i])?;
            if !m.propre[i] {
                x.sections = Vec::new(); // le halo ne sert qu'aux raccords
            }
            Ok(x)
        })
        .collect::<Res<_>>()?;
    et.fin("passe1");

    // 7. Fusion de la zone.
    let fu = fusion(&m, &p1);
    et.fin("fusion");
    eprintln!(
        "  zone : {} composantes, {} accessibles ; {:.1} % des cases libres",
        fu.composantes,
        fu.accessibles,
        100.0 * fu.cases_accessibles as f64 / fu.cases_candidates.max(1) as f64
    );

    // 8. Passe 2 (tiles du département seulement). L'union-find est
    // compressée au préalable (chaque nœud pointe sur sa racine) et partagée
    // en lecture seule.
    let mut uf = zone::UnionFind { p: fu.uf.p.clone() };
    for k in 0..uf.p.len() as u32 {
        uf.trouver(k);
    }
    let racines = uf.p.clone();
    let z2: Vec<Zone2> = p1.par_iter().enumerate().map(|(i, x)| passe2_zone(i, x, &fu, &racines, p)).collect();
    et.fin("passe2-zone");
    let secs: Vec<Vec<Section>> = p1.iter_mut().map(|x| std::mem::take(&mut x.sections)).collect();
    let sorties: Vec<Option<Sortie>> = secs
        .into_par_iter()
        .enumerate()
        .map(|(i, s)| m.propre[i].then(|| passe2(m.tiles[i], i, &m, &d, &r, &p1, s, &z2, &rel, p)).transpose())
        .collect::<Res<_>>()?;
    drop(z2);
    et.fin("passe2");

    // 9. Écriture des paquets.
    let mut blocs: BTreeMap<(i32, i32), Vec<(TileId, usize)>> = BTreeMap::new();
    for (i, t) in m.tiles.iter().enumerate() {
        if sorties[i].is_some() {
            blocs.entry(tuile::bloc(*t)).or_default().push((*t, i));
        }
    }
    let mut empreintes = Vec::new();
    let mut taille_paquets = 0u64;
    for ((bx, by), ts) in &blocs {
        let mut v: Vec<(TileId, &[u8])> =
            ts.iter().filter_map(|(t, i)| sorties[*i].as_ref().map(|s| (*t, s.octets.as_slice()))).collect();
        v.sort_by_key(|(t, _)| (t.iy, t.ix));
        let pq = tuile::paquet(&v);
        taille_paquets += pq.len() as u64;
        std::fs::write(dossier_paquets.join(format!("bloc_x{bx}_y{by}.p07")), &pq)?;
        for (t, b) in v {
            empreintes.push((t, blake3::hash(b)));
        }
    }
    empreintes.sort_by_key(|(t, _)| *t);
    let mut h = blake3::Hasher::new();
    for (t, e) in &empreintes {
        h.update(&t.ix.to_le_bytes());
        h.update(&t.iy.to_le_bytes());
        h.update(e.as_bytes());
    }
    // Fichier global des communes : code, nom, population, mairie, voisines.
    let mut adj: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut plus_grand: BTreeMap<u32, (f64, u64)> = BTreeMap::new();
    for (i, x) in p1.iter().enumerate() {
        adj.extend(x.adj.iter().copied());
        for &(c, e, id, _) in &x.plus_grand {
            let v = plus_grand.entry(c).or_insert((0.0, 0));
            if (e, u64::MAX - id) > (v.0, u64::MAX - v.1) {
                *v = (e, id);
            }
        }
        let t = m.tiles[i];
        for (dx, dy, a, b) in [(1, 0, 1usize, 0usize), (0, 1, 3, 2)] {
            if let Some(&nb) = m.pos.get(&TileId { ix: t.ix + dx, iy: t.iy + dy }) {
                for k in 0..500 {
                    let (u, v) = (x.comm_bords[a][k], p1[nb].comm_bords[b][k]);
                    if u != u32::MAX && v != u32::MAX && u != v {
                        adj.insert((u.min(v), u.max(v)));
                    }
                }
            }
        }
    }
    let mut fc = std::fs::File::create(p.sortie.join("communes.csv"))?;
    writeln!(fc, "code,nom,departement,population_bdtopo,habitants_insee,mairie,batiment_mairie,voisines")?;
    let mut hab_commune: BTreeMap<u32, u64> = BTreeMap::new();
    for (i, x) in p1.iter().enumerate() {
        if m.propre[i] {
            for &(c, _, h) in &x.fragments {
                *hab_commune.entry(c).or_default() += h as u64;
            }
        }
    }
    let mut st_mairies = [0usize; 3];
    let mut blob = Ecrivain::default();
    let communes_dept: Vec<&objets::Commune> = d.communes.iter().filter(|c| c.dept == p.dept).collect();
    blob.u(communes_dept.len() as u64);
    for c in &communes_dept {
        let cn = code_num(&c.code);
        let voisines: Vec<u32> = adj
            .iter()
            .filter_map(|&(a, b)| {
                if a == cn {
                    Some(b)
                } else if b == cn {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        let (mairie, bat) = match mairies.get(&c.code) {
            Some((_id, Some(b))) => {
                st_mairies[0] += 1;
                ("mairie".to_string(), d.batiments[*b].id)
            }
            Some((_, None)) => {
                st_mairies[1] += 1;
                ("mairie_sans_batiment".to_string(), plus_grand.get(&cn).map(|v| v.1).unwrap_or(0))
            }
            None => {
                st_mairies[2] += 1;
                ("plus_grand_batiment".to_string(), plus_grand.get(&cn).map(|v| v.1).unwrap_or(0))
            }
        };
        writeln!(
            fc,
            "{},\"{}\",{},{},{},{},{},{}",
            c.code,
            c.nom,
            c.dept,
            c.population,
            hab_commune.get(&cn).copied().unwrap_or(0),
            mairie,
            bat,
            voisines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ")
        )?;
        blob.u(cn as u64);
        blob.s(&c.nom);
        blob.u(bat);
        blob.u(voisines.len() as u64);
        for v in voisines {
            blob.u(v as u64);
        }
    }
    let communes_bin = crate::zst(&blob.b, p.niveau)?;
    std::fs::write(p.sortie.join("communes.bin"), &communes_bin)?;
    h.update(&communes_bin);
    let empreinte = h.finalize();
    et.fin("ecriture");

    // ------------------------------------------------ Rapports
    let total_s = et.t0.elapsed().as_secs_f64();
    let rss = crate::mesure::rss_max_mo();
    eprintln!(
        "total {:.1} s, mémoire max {:.0} Mo, paquets {:.1} Mo, empreinte {}",
        total_s,
        rss,
        taille_paquets as f64 / 1e6,
        empreinte.to_hex()
    );

    // tiles.csv
    let mut ft = std::fs::File::create(p.sortie.join("tiles.csv"))?;
    let noms: Vec<&str> = tuile::SECTIONS.iter().map(|s| s.1).collect();
    write!(
        ft,
        "ix,iy,type,terre,mer,part_bati,nb_bat,habitants,fragments,dist_cote_km,murs_cases,cours,cours_percees,cases_exclues,cases_passages,cases_zone,octets"
    )?;
    for n in &noms {
        write!(ft, ",{n}_brut,{n}_z")?;
    }
    for n in ETAPES_TILE {
        write!(ft, ",ms_{n}")?;
    }
    writeln!(ft, ",empreinte")?;
    for (i, t) in m.tiles.iter().enumerate() {
        let Some(s) = &sorties[i] else { continue };
        let x = &p1[i];
        let terre = x.cases_hors < 250_000;
        let ty = type_tile(terre, x.cases_mer > 0, x.habitants, x.part_bati);
        write!(
            ft,
            "{},{},{},{},{},{:.5},{},{},{},{:.2},{},{},{},{},{},{},{}",
            t.ix,
            t.iy,
            ty,
            terre as u8,
            (x.cases_mer > 0) as u8,
            x.part_bati,
            x.nb_bat,
            x.habitants,
            x.fragments.len(),
            m.dist_cote_km[i],
            x.murs_cases,
            s.cours,
            s.cours_percees,
            s.cases_exclues,
            s.cases_passages,
            s.cases_zone,
            s.octets.len()
        )?;
        for (sec, _) in tuile::SECTIONS {
            let v = s.tailles.iter().find(|x| x.0 == *sec);
            write!(ft, ",{},{}", v.map_or(0, |v| v.1), v.map_or(0, |v| v.2))?;
        }
        let rs = 0.0;
        let mst = [x.ms[0], x.ms[1], x.ms[2], x.ms[3], x.ms[4], x.ms[5], s.ms[0], s.ms[1] + rs, s.ms[2]];
        for v in mst {
            write!(ft, ",{v:.3}")?;
        }
        writeln!(ft, ",{}", &blake3::hash(&s.octets).to_hex()[..16])?;
    }

    // relief.csv : sources par tile et raccords.
    let mut fr = std::fs::File::create(p.sortie.join("relief.csv"))?;
    writeln!(
        fr,
        "ix,iy,source,rge1m,rge5m,sans,rge1_rge5_points,rge1_rge5_moy_cm,rge1_rge5_max_cm,marche_cm,octets_base,octets_fin"
    )?;
    for (t, x) in &rel {
        let s = &x.stat;
        write!(fr, "{},{},{},{},{},{}", t.ix, t.iy, s.source(), s.part[0], s.part[1], s.part[2])?;
        let c = s.raccord;
        write!(fr, ",{},{:.1},{}", c.0, c.1 as f64 / c.0.max(1) as f64, c.2)?;
        writeln!(fr, ",{},{},{}", s.marche, x.base.len(), x.fin.as_ref().map_or(0, |f| f.len()))?;
    }

    // etapes.csv
    let mut fe = std::fs::File::create(p.sortie.join("etapes.csv"))?;
    writeln!(fe, "etiquette,etape,s,rss_mo,rss_max_mo")?;
    for (n, s, r1, r2) in &et.lignes {
        writeln!(fe, "{},{n},{s:.3},{r1:.0},{r2:.0}", p.etiquette)?;
    }
    for (n, s, c) in &d.ms {
        writeln!(fe, "{},lecture:{n},{:.3},{c},", p.etiquette, s / 1e3)?;
    }

    // controles.txt
    let mut fk = std::fs::File::create(p.sortie.join("controles.txt"))?;
    let mut ids = BTreeSet::new();
    let mut doublons = 0;
    let mut bat_propres = 0usize;
    let mut bat_halo = 0usize;
    for (i, _) in m.tiles.iter().enumerate() {
        for &k in &r.bat_p[i] {
            if m.propre[i] {
                bat_propres += 1;
                if !ids.insert(d.batiments[k as usize].id) {
                    doublons += 1;
                }
            } else {
                bat_halo += 1;
            }
        }
    }
    let hab_propres: u64 = (0..m.n()).filter(|&i| m.propre[i]).map(|i| p1[i].habitants as u64).sum();
    let hab_halo: u64 = (0..m.n()).filter(|&i| !m.propre[i]).map(|i| p1[i].habitants as u64).sum();
    let hab_frag: u64 =
        (0..m.n()).filter(|&i| m.propre[i]).flat_map(|i| p1[i].fragments.iter().map(|f| f.2 as u64)).sum();
    let pop_dept: u64 = d.communes.iter().filter(|c| c.dept == p.dept).map(|c| c.population as u64).sum();
    let longueur_propres: f64 =
        (0..m.n()).filter(|&i| m.propre[i]).flat_map(|i| r.routes[i].iter().map(|mo| longueur(&mo.pts))).sum();
    writeln!(fk, "etiquette {}", p.etiquette)?;
    writeln!(fk, "threads {}", rayon::current_num_threads())?;
    writeln!(fk, "empreinte {}", empreinte.to_hex())?;
    writeln!(fk, "tiles_departement {nprop}")?;
    writeln!(fk, "tiles_halo {}", m.n() - nprop)?;
    writeln!(fk, "octets_paquets {taille_paquets}")?;
    writeln!(fk, "paquets {}", blocs.len())?;
    writeln!(fk, "batiments_lus {}", d.batiments.len())?;
    writeln!(fk, "batiments_tiles_departement {bat_propres}")?;
    writeln!(fk, "batiments_tiles_halo {bat_halo}")?;
    writeln!(fk, "batiments_hors_tiles {}", r.bat_hors)?;
    writeln!(fk, "batiments_somme_ok {}", (bat_propres + bat_halo + r.bat_hors == d.batiments.len()) as u8)?;
    writeln!(fk, "batiments_doublons {doublons}")?;
    writeln!(fk, "routes_longueur_source_km {:.3}", r.longueur_routes_src / 1e3)?;
    writeln!(fk, "routes_longueur_morceaux_km {:.3}", r.longueur_routes_morceaux / 1e3)?;
    writeln!(
        fk,
        "routes_ecart_relatif {:.3e}",
        (r.longueur_routes_morceaux - r.longueur_routes_src).abs() / r.longueur_routes_src.max(1.0)
    )?;
    writeln!(fk, "routes_longueur_departement_km {:.3}", longueur_propres / 1e3)?;
    if let Some(rp) = &rep_pop {
        writeln!(fk, "carreaux_fichier {carreaux_total}")?;
        writeln!(fk, "carreaux_zone {carreaux_lus}")?;
        writeln!(fk, "insee_somme_ind {:.1}", rp.somme_ind)?;
        writeln!(fk, "insee_somme_entiers {}", rp.somme_entiers)?;
        let pts: u64 = rp.points.iter().map(|p| p.2 as u64).sum();
        let bats: u64 = rp.par_bat.values().map(|v| *v as u64).sum();
        writeln!(fk, "habitants_sur_batiments {bats}")?;
        writeln!(fk, "habitants_sur_points {pts}")?;
        writeln!(fk, "carreaux_residentiel {}", rp.sur_residentiel)?;
        writeln!(fk, "carreaux_autres_batiments {}", rp.sur_autres)?;
        writeln!(fk, "carreaux_points {}", rp.sur_points)?;
        writeln!(fk, "habitants_tiles_departement {hab_propres}")?;
        writeln!(fk, "habitants_fragments_departement {hab_frag}")?;
        let hp: u64 = (0..m.n()).filter(|&i| m.propre[i]).map(|i| p1[i].hab_points as u64).sum();
        writeln!(fk, "habitants_departement_sur_points {hp}")?;
        writeln!(fk, "habitants_tiles_halo {hab_halo}")?;
        let hors = bats + pts - hab_propres - hab_halo;
        writeln!(fk, "habitants_hors_tiles {hors}")?;
        writeln!(
            fk,
            "population_conservee {}",
            (hab_frag == hab_propres && hab_propres + hab_halo + hors == rp.somme_entiers) as u8
        )?;
    }
    writeln!(fk, "population_bdtopo_departement {pop_dept}")?;
    writeln!(fk, "zone_composantes {}", fu.composantes)?;
    writeln!(fk, "zone_accessibles {}", fu.accessibles)?;
    writeln!(fk, "zone_part_cases {:.5}", fu.cases_accessibles as f64 / fu.cases_candidates.max(1) as f64)?;
    writeln!(fk, "mairies_avec_batiment {}", st_mairies[0])?;
    writeln!(fk, "mairies_sans_batiment {}", st_mairies[1])?;
    writeln!(fk, "communes_sans_mairie {}", st_mairies[2])?;
    writeln!(fk, "communes_paires_voisines {}", adj.len())?;
    writeln!(fk, "communes_bin_octets {}", communes_bin.len())?;
    let mut sb = [0u64; 9];
    for s in sorties.iter().flatten() {
        let b = &s.bati;
        for (k, v) in
            [b.nettoyes, b.vides, b.portes[0], b.portes[1], b.portes[2], b.portes[3], b.sans_porte, b.toit, b.materiaux]
                .into_iter()
                .enumerate()
        {
            sb[k] += v as u64;
        }
    }
    for (n, v) in [
        "emprises_nettoyees",
        "emprises_vides",
        "portes_1m",
        "portes_2m",
        "portes_3m",
        "portes_centroide",
        "sans_porte",
        "toit_connu",
        "materiaux_connus",
    ]
    .iter()
    .zip(sb)
    {
        writeln!(fk, "bati:{n} {v}")?;
    }
    let mut src = [0u64; 4];
    for (t, i) in &m.pos {
        if m.propre[*i] && p1[*i].cases_hors < 250_000 {
            src[rel.get(t).map_or(0, |r| r.stat.source()) as usize] += 1;
        }
    }
    writeln!(fk, "relief_terre:sans {}\nrelief_terre:rge1m {}\nrelief_terre:rge5m {}", src[0], src[2], src[3])?;
    writeln!(
        fk,
        "tiles_avec_etranger {}",
        (0..m.n()).filter(|&i| m.propre[i] && p1[i].cases_hors > p1[i].cases_mer).count()
    )?;
    writeln!(fk, "relief_sources {}", dep.sources())?;
    for (k, v) in &r.ponts_stats {
        writeln!(fk, "ponts:{k} {v}")?;
    }
    for (k, v) in &d.inconnus {
        writeln!(fk, "inconnu:{k} {v}")?;
    }
    writeln!(fk, "total_s {total_s:.2}")?;
    writeln!(fk, "rss_max_mo {rss:.0}")?;

    // lieux.csv (tiles du département)
    let mut fl = std::fs::File::create(p.sortie.join("lieux.csv"))?;
    writeln!(fl, "type,id,nom,detail,ix,iy")?;
    for (i, t) in m.tiles.iter().enumerate() {
        if !m.propre[i] {
            continue;
        }
        for &k in &r.lieux[i] {
            let l = &d.lieux[k as usize];
            writeln!(
                fl,
                "\"{}\",{},\"{}\",\"{}\",{},{}",
                objets::TYPES_LIEUX[l.type_ as usize],
                l.id,
                l.nom.replace('"', "'"),
                l.detail.replace('"', "'"),
                t.ix,
                t.iy
            )?;
        }
    }
    Ok(())
}

pub fn bbox_poly_pub(p: &Polygone) -> [f64; 4] {
    bbox_polys(std::slice::from_ref(p))
}
