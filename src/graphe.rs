//! Graphe routier national (B-03, P-09), porté de P-26 : construit depuis les
//! tiles publiées, ordre CCH compris, écrit au format de
//! [`beta1_common::graph`].
//!
//! 1. **Lecture** : morceaux de route carrossables et ponts de chaque tile, en
//!    coordonnées globales entières au 1/16 m (position locale + centre de la
//!    tile) : deux tiles voisines donnent exactement le même entier pour un
//!    même point de coupe.
//! 2. **Recousure** : les morceaux d'un même tronçon (même identifiant BD
//!    TOPO) sont triés par rang et mis bout à bout quand les rangs se suivent
//!    et que les extrémités coïncident. Un trou dans les rangs (partie hors des
//!    tiles lues) laisse deux fragments. La même fonction recoud des morceaux
//!    ou des fragments de départements : f(A ∪ B) = f(f(A) ∪ f(B)). Analogie :
//!    le réassemblage IP des fragments par (identifiant, décalage).
//! 3. **Nœuds** : la BD TOPO n'a pas de table de nœuds ; les tronçons sont
//!    coupés aux intersections, et deux tronçons qui se touchent partagent une
//!    extrémité. Un nœud est une extrémité de fragment, identifiée par ses
//!    coordonnées entières.
//! 4. **Numérotation canonique** : nœuds triés par (y, x), arêtes par
//!    (identifiant, rang) : le graphe ne dépend ni de l'ordre de lecture ni du
//!    nombre de threads.
//! 5. **Ponts** associés aux arêtes au-dessus du sol, charge limite par
//!    heuristique ; puis **ordre CCH** (dissection imbriquée par flot inertiel).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use beta1_common::geo::TileId;
use beta1_common::graph::{RoadAttr, RoadGraph, Structure};
use beta1_common::tile::sections::{self, Geometry};
use rayon::prelude::*;

use crate::erreur::{Contexte, Res};
use crate::tuile::{self, Sec};

/// Quantification : 1/16 m.
const Q: i64 = 16;

/// Centre d'une tile, en 1/16 m globaux.
fn centre_q(t: TileId) -> (i64, i64) {
    ((t.ix as i64 * 1000 + 500) * Q, (t.iy as i64 * 1000 + 500) * Q)
}

/// Tile d'un point global.
fn tile_de_q(x: i64, y: i64) -> TileId {
    TileId { ix: x.div_euclid(1000 * Q) as i32, iy: y.div_euclid(1000 * Q) as i32 }
}

/// Coordonnée locale (m, exacte en f32 au 1/16 m) → entier global.
fn global(v: f32, centre: i64) -> i64 {
    (v as f64 * Q as f64).round() as i64 + centre
}

/// Morceau de route d'une tile (points globaux ; z en dm, 0 au sol).
#[derive(Clone, Debug)]
pub struct Morceau {
    pub id: u64,
    pub rang: u16,
    pub attr: RoadAttr,
    pub pts: Vec<[i64; 3]>,
}

/// Pont (B-03), tel que la chaîne l'écrit dans les tiles.
#[derive(Clone, Debug)]
pub struct Pont {
    pub tile: TileId,
    pub id: u64,
    pub longueur_dm: u32,
    pub largeur_dm: u16,
    /// Importance de la route portée la plus importante ; 9 = aucune.
    pub importance: u8,
    pub nature_route: u8,
    pub poids_dt: u16,
    /// 1 eau, 2 route, 4 voie ferrée.
    pub franchit: u8,
    /// Sommets de l'emprise, globaux.
    pub pts: Vec<[i64; 2]>,
}

/// Réseau carrossable : ce qu'une classe de véhicule peut emprunter
/// (sentiers, escaliers, pistes cyclables et bacs exclus de la topologie).
pub fn carrossable(a: &RoadAttr) -> bool {
    a.nature <= 6
}

/// Routes carrossables et ponts d'une tile.
fn lire_tile(b: &[u8]) -> Res<(Vec<Morceau>, Vec<Pont>)> {
    let t = tuile::lire(b, true).ctx("tile illisible")?;
    let id = t.header.tile().ctx("zone de la tile")?;
    let (cx, cy) = centre_q(id);
    let mut ms = Vec::new();
    if let Some(s) = t.section(Sec::Network).ctx("réseau")? {
        for r in sections::network(&s).ctx("réseau")?.roads {
            let attr = RoadAttr {
                nature: r.nature,
                importance: r.importance,
                level: r.level,
                lanes: r.lanes,
                direction: r.direction,
                access: r.access,
                urban: r.urban,
                speed: r.speed,
                width_dm: r.width_dm as u16,
                weight_dt: r.weight_dt as u16,
                height_dm: r.height_dm as u16,
                width_limit_dm: r.width_limit_dm as u16,
            };
            if !carrossable(&attr) {
                continue;
            }
            let pts = r
                .points
                .iter()
                .map(|p| [global(p[0], cx), global(p[1], cy), (p[2] as f64 * 10.0).round() as i64])
                .collect();
            ms.push(Morceau { id: r.id, rang: r.rank as u16, attr, pts });
        }
    }
    let mut ps = Vec::new();
    if let Some(s) = t.section(Sec::Bridges).ctx("ponts")? {
        for p in sections::bridges(&s).ctx("ponts")? {
            let mut pts = Vec::new();
            let mut pousser = |q: &[f32]| pts.push([global(q[0], cx), global(q[1], cy)]);
            match &p.geometry {
                Geometry::Empty => {}
                Geometry::Point(q) => pousser(q),
                Geometry::Lines(ls) => ls.iter().flatten().for_each(|q| pousser(q)),
                Geometry::Polygons(ps) => ps.iter().flatten().flatten().for_each(|q| pousser(q)),
            }
            ps.push(Pont {
                tile: id,
                id: p.id,
                longueur_dm: p.length_dm,
                largeur_dm: p.width_dm as u16,
                importance: p.importance,
                nature_route: p.road_nature,
                poids_dt: p.weight_dt as u16,
                franchit: p.crosses,
                pts,
            });
        }
    }
    Ok((ms, ps))
}

/// Toutes les tiles des paquets d'un département (ordre indifférent : le
/// graphe est canonique).
fn lire_departement(dossier: &Path) -> Res<(Vec<Morceau>, Vec<Pont>)> {
    let pq = tuile::paquets(dossier)?;
    let mut par_fichier: BTreeMap<&PathBuf, Vec<(u64, usize)>> = BTreeMap::new();
    for (f, off, n) in pq.index.values() {
        par_fichier.entry(f).or_default().push((*off, *n));
    }
    let lus = par_fichier
        .into_par_iter()
        .map(|(f, v)| {
            let b = std::fs::read(f).ctx(f.display())?;
            v.iter()
                .map(|&(off, n)| {
                    let t =
                        b.get(off as usize..off as usize + n).ctx(format!("{} : tile hors du paquet", f.display()))?;
                    lire_tile(t).ctx(f.display())
                })
                .collect::<Res<Vec<_>>>()
        })
        .collect::<Res<Vec<_>>>()?;
    let (mut ms, mut ps) = (Vec::new(), Vec::new());
    for (m, p) in lus.into_iter().flatten() {
        ms.extend(m);
        ps.extend(p);
    }
    Ok((ms, ps))
}

/// Fragment recousu d'un tronçon : rangs r0..=r1, points globaux.
#[derive(Clone, Debug)]
pub struct Fragment {
    pub id: u64,
    pub r0: u16,
    pub r1: u16,
    pub attr: RoadAttr,
    pub pts: Vec<[i64; 3]>,
}

/// Bilan d'une recousure.
#[derive(Default, Debug, Clone)]
pub struct StatsRecousure {
    pub entree: usize,
    pub sortie: usize,
    pub jointures: usize,
    /// Rangs déjà couverts par le fragment précédent (même tile lue deux
    /// fois, départements qui se recouvrent) : retirés.
    pub doublons: usize,
    /// Rangs qui débordent en partie du fragment précédent : incohérence des
    /// données, gardés à part (deux arêtes).
    pub chevauchements: usize,
    /// Rangs consécutifs mais extrémités différentes.
    pub ruptures: usize,
    /// Rangs non consécutifs (partie hors des tiles).
    pub trous: usize,
    pub attr_differents: usize,
}

pub fn de_morceaux(ms: Vec<Morceau>) -> Vec<Fragment> {
    ms.into_iter().map(|m| Fragment { id: m.id, r0: m.rang, r1: m.rang, attr: m.attr, pts: m.pts }).collect()
}

/// Recoud des fragments : résultat canonique (trié par identifiant, rang).
pub fn recoudre(mut fs: Vec<Fragment>) -> (Vec<Fragment>, StatsRecousure) {
    let mut st = StatsRecousure { entree: fs.len(), ..Default::default() };
    fs.sort_by(|a, b| (a.id, a.r0, &a.pts).cmp(&(b.id, b.r0, &b.pts)));
    let mut out: Vec<Fragment> = Vec::with_capacity(fs.len());
    for f in fs {
        if let Some(p) = out.last_mut()
            && p.id == f.id
        {
            if f.r1 <= p.r1 {
                st.doublons += 1;
                continue;
            } else if f.r0 <= p.r1 {
                st.chevauchements += 1;
            } else if f.r0 == p.r1 + 1 {
                if p.pts.last().map(|q| [q[0], q[1]]) == f.pts.first().map(|q| [q[0], q[1]]) {
                    if p.attr != f.attr {
                        st.attr_differents += 1;
                    }
                    st.jointures += 1;
                    p.r1 = f.r1;
                    p.pts.extend_from_slice(&f.pts[1..]);
                    continue;
                }
                st.ruptures += 1;
            } else {
                st.trous += 1;
            }
        }
        out.push(f);
    }
    st.sortie = out.len();
    (out, st)
}

fn longueur_m(pts: &[[i64; 3]]) -> f64 {
    let mut l = 0.0;
    for s in pts.windows(2) {
        let (dx, dy) = ((s[1][0] - s[0][0]) as f64, (s[1][1] - s[0][1]) as f64);
        l += (dx * dx + dy * dy).sqrt();
    }
    l / Q as f64
}

/// Graphe en construction : [`RoadGraph`] et fragment de chaque arête.
struct Construction {
    g: RoadGraph,
    frag: Vec<u32>,
}

/// Graphe à partir des fragments. Les boucles (u = v) restent des arêtes :
/// elles ne servent jamais à un plus court chemin.
fn construire(fs: &[Fragment]) -> Res<Construction> {
    let mut cles: Vec<[i64; 2]> = Vec::with_capacity(fs.len() * 2);
    for f in fs {
        let (a, b) = (f.pts.first().ctx("fragment vide")?, f.pts.last().ctx("fragment vide")?);
        cles.push([a[1], a[0]]);
        cles.push([b[1], b[0]]);
    }
    cles.sort_unstable();
    cles.dedup();
    let idx = |p: &[i64; 3]| cles.binary_search(&[p[1], p[0]]).map(|k| k as u32).ok().ctx("nœud introuvable");
    let mut g = RoadGraph { nodes: cles.iter().map(|c| [c[1], c[0]]).collect(), ..Default::default() };
    let mut frag = Vec::with_capacity(fs.len());
    for (k, f) in fs.iter().enumerate() {
        // Les fragments ont au moins un point (vérifié ci-dessus).
        g.u.push(idx(&f.pts[0])?);
        g.v.push(idx(&f.pts[f.pts.len() - 1])?);
        g.attrs.push(f.attr);
        g.length_mm.push((longueur_m(&f.pts) * 1000.0) as u32);
        g.ids.push(f.id);
        g.first_rank.push(f.r0);
        g.structure_of.push(None);
        frag.push(k as u32);
    }
    Ok(Construction { g, frag })
}

/// Distance d'un point à une polyligne, en 1/16 m.
fn dist_pt_ligne(p: [i64; 2], l: &[[i64; 3]]) -> f64 {
    let mut best = f64::MAX;
    let (px, py) = (p[0] as f64, p[1] as f64);
    for s in l.windows(2) {
        let (ax, ay, bx, by) = (s[0][0] as f64, s[0][1] as f64, s[1][0] as f64, s[1][1] as f64);
        let (dx, dy) = (bx - ax, by - ay);
        let d2 = dx * dx + dy * dy;
        let t = if d2 == 0.0 { 0.0 } else { (((px - ax) * dx + (py - ay) * dy) / d2).clamp(0.0, 1.0) };
        let (qx, qy) = (ax + t * dx - px, ay + t * dy - py);
        best = best.min((qx * qx + qy * qy).sqrt());
    }
    if let [q] = l {
        let (qx, qy) = (q[0] as f64 - px, q[1] as f64 - py);
        best = (qx * qx + qy * qy).sqrt();
    }
    best
}

/// Bilan de l'association des ponts.
#[derive(Default, Debug)]
pub struct StatsPonts {
    pub ponts: usize,
    pub ponts_associes: usize,
    pub aretes_sur_ouvrage: usize,
    pub aretes_pont: usize,
    pub aretes_implicites: usize,
}

/// Associe les ponts aux arêtes au-dessus du sol (règle de P-07 : sommet du
/// pont à moins de 5 m du tronçon), puis calcule la charge de chaque ouvrage.
/// Une arête portée par plusieurs ponts prend la plus faible charge.
fn associer_ponts(c: &mut Construction, fs: &[Fragment], ponts: &[Pont]) -> StatsPonts {
    let g = &mut c.g;
    let m = g.edge_count();
    let mut st = StatsPonts { ponts: ponts.len(), ..Default::default() };
    // Index des arêtes sur ouvrage par tile de leurs sommets.
    let mut par_tile: BTreeMap<TileId, Vec<u32>> = BTreeMap::new();
    for e in 0..m {
        if g.attrs[e].level >= 1 {
            st.aretes_sur_ouvrage += 1;
            let mut ts: Vec<TileId> = fs[c.frag[e] as usize].pts.iter().map(|p| tile_de_q(p[0], p[1])).collect();
            ts.sort();
            ts.dedup();
            for t in ts {
                par_tile.entry(t).or_default().push(e as u32);
            }
        }
    }
    let seuil = 5.0 * Q as f64;
    let mut porte: Vec<Vec<u32>> = vec![Vec::new(); m];
    for (k, p) in ponts.iter().enumerate() {
        let mut cand: Vec<u32> = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(v) = par_tile.get(&TileId { ix: p.tile.ix + dx, iy: p.tile.iy + dy }) {
                    cand.extend(v);
                }
            }
        }
        cand.sort_unstable();
        cand.dedup();
        let mut ok = false;
        for e in cand {
            let l = &fs[c.frag[e as usize] as usize].pts;
            if p.pts.iter().any(|q| dist_pt_ligne(*q, l) <= seuil) {
                porte[e as usize].push(k as u32);
                ok = true;
            }
        }
        if ok {
            st.ponts_associes += 1;
        }
    }
    for (e, porte) in porte.iter().enumerate() {
        if g.attrs[e].level < 1 {
            continue;
        }
        let a = g.attrs[e];
        let mut o = Structure { bridge: None, load_t: charge(None, &a, g.length_mm[e]), width_dm: a.width_dm };
        if porte.is_empty() {
            st.aretes_implicites += 1;
        } else {
            st.aretes_pont += 1;
            let mut best: Option<(u16, u64, Structure)> = None;
            for &k in porte {
                let p = &ponts[k as usize];
                let t = charge(Some(p), &a, g.length_mm[e]);
                let lr = if p.largeur_dm > 0 { p.largeur_dm } else { a.width_dm };
                if best.as_ref().is_none_or(|b| (t, p.id) < (b.0, b.1)) {
                    best = Some((t, p.id, Structure { bridge: Some(p.id), load_t: t, width_dm: lr }));
                }
            }
            if let Some(b) = best {
                o = b.2;
            }
        }
        g.structure_of[e] = Some(g.structures.len() as u32);
        g.structures.push(o);
    }
    st
}

/// Charge limite d'un ouvrage, en tonnes (heuristique B-03).
///
/// Paliers inspirés des convois du règlement français des ponts routiers
/// (convois militaires Mc 80 et Mc 120 sur le réseau principal, à
/// vérifier) : 120, 80, 44, 26, 12, 3,5 t.
pub const PALIERS: [u16; 6] = [120, 80, 44, 26, 12, 4];

fn palier(importance: u8) -> usize {
    match importance {
        1 | 2 => 0,
        3 => 1,
        4 => 2,
        5 => 3,
        _ => 4,
    }
}

/// Charge d'un ouvrage : restriction réelle de la BD TOPO, sinon palier selon
/// l'importance de la route, corrigé par la largeur, la longueur et ce que
/// l'ouvrage franchit.
pub fn charge(p: Option<&Pont>, a: &RoadAttr, long_mm: u32) -> u16 {
    let reel = p.map(|p| p.poids_dt).filter(|&v| v > 0).or(Some(a.weight_dt).filter(|&v| v > 0));
    if let Some(dt) = reel {
        return dt.div_ceil(10);
    }
    let imp =
        p.map(|p| if p.importance < 9 { p.importance.min(a.importance) } else { a.importance }).unwrap_or(a.importance);
    let mut k = palier(imp) as i32;
    let largeur = p.map(|p| p.largeur_dm).filter(|&l| l > 0).unwrap_or(a.width_dm);
    let longueur_dm = p.map(|p| p.longueur_dm).filter(|&l| l > 0).unwrap_or(long_mm / 100);
    let franchit = p.map(|p| p.franchit).unwrap_or(0);
    // La largeur de la BD TOPO est une classe (3 m pour une voie, 4 m pour
    // deux). Sous 2,5 m : passerelle ; une seule voie : un palier de moins.
    if largeur > 0 && largeur < 25 && !matches!(a.nature, 0 | 1 | 3) {
        return PALIERS[5];
    }
    let separee = matches!(a.nature, 0 | 1 | 3) || p.is_some_and(|p| matches!(p.nature_route, 0 | 1 | 3));
    if largeur > 0 && largeur < 35 && !separee {
        k += 1;
    }
    // Grand ouvrage sur l'eau d'une route secondaire (ponts suspendus et
    // anciens ouvrages) : un palier de moins.
    if longueur_dm >= 1000 && franchit & 1 != 0 && imp >= 4 {
        k += 1;
    }
    // Ponceau (moins de 10 m) : un palier de plus.
    if longueur_dm > 0 && longueur_dm < 100 {
        k -= 1;
    }
    // Passage supérieur sur une route ou une voie ferrée (et pas sur l'eau) :
    // construit à une norme récente, au moins 44 t.
    if franchit & 6 != 0 && franchit & 1 == 0 {
        k = k.min(2);
    }
    PALIERS[k.clamp(0, 4) as usize]
}

/// Arcs (2e : u → v, 2e + 1 : v → u) rangés par nœud de départ, comme le
/// serveur les donne à la CCH : (début par nœud, arc à chaque position, tête de chaque arc).
fn arcs(g: &RoadGraph) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let (n, m) = (g.node_count(), g.edge_count());
    let mut tete = vec![0u32; 2 * m];
    let mut queue = vec![0u32; 2 * m];
    for e in 0..m {
        (queue[2 * e], tete[2 * e]) = (g.u[e], g.v[e]);
        (queue[2 * e + 1], tete[2 * e + 1]) = (g.v[e], g.u[e]);
    }
    let mut deb = vec![0u32; n + 1];
    for &q in &queue {
        deb[q as usize + 1] += 1;
    }
    for k in 0..n {
        deb[k + 1] += deb[k];
    }
    let mut cur = deb.clone();
    let mut arc = vec![0u32; 2 * m];
    for (a, &q) in queue.iter().enumerate() {
        arc[cur[q as usize] as usize] = a as u32;
        cur[q as usize] += 1;
    }
    (deb, arc, tete)
}

/// Ordre CCH par dissection imbriquée (flot inertiel), sur les arcs rangés.
fn ordre_cch(g: &RoadGraph) -> Vec<u32> {
    let n = g.node_count();
    let (deb, arc, tete) = arcs(g);
    let tail: Vec<u32> = (0..n).flat_map(|x| std::iter::repeat_n(x as u32, (deb[x + 1] - deb[x]) as usize)).collect();
    let head: Vec<u32> = arc.iter().map(|&a| tete[a as usize]).collect();
    let lat: Vec<f32> = g.nodes.iter().map(|p| (p[1] as f64 / 16000.0) as f32).collect();
    let lon: Vec<f32> = g.nodes.iter().map(|p| (p[0] as f64 / 16000.0) as f32).collect();
    cch::inertial_order(n as u32, &tail, &head, &lat, &lon)
}

/// Construit le graphe des départements donnés et l'écrit dans `sortie`.
/// Rend l'empreinte du fichier (16 premiers chiffres hexadécimaux du BLAKE3).
pub fn executer(dossiers: &[PathBuf], sortie: &Path) -> Res<String> {
    let t0 = std::time::Instant::now();
    let dire = |m: &str| println!("{:7.1} s  {m}", t0.elapsed().as_secs_f64());
    let (mut frags, mut ponts) = (Vec::new(), Vec::new());
    for d in dossiers {
        let (ms, ps) = lire_departement(d)?;
        // Recousu par département, puis en tout.
        frags.extend(recoudre(de_morceaux(ms)).0);
        ponts.extend(ps);
    }
    dire(&format!("{} fragments, {} ponts", frags.len(), ponts.len()));
    let (frags, st) = recoudre(frags);
    dire(&format!(
        "recousu : {} fragments ; entre départements {} jointures, {} doublons retirés, {} chevauchements, {} ruptures, {} trous, {} attributs différents",
        frags.len(),
        st.jointures,
        st.doublons,
        st.chevauchements,
        st.ruptures,
        st.trous,
        st.attr_differents
    ));
    let mut c = construire(&frags)?;
    let sp = associer_ponts(&mut c, &frags, &ponts);
    drop(ponts);
    dire(&format!(
        "ponts : {} sur {} associés ; {} arêtes sur ouvrage, dont {} sans pont (implicites)",
        sp.ponts_associes, sp.ponts, sp.aretes_sur_ouvrage, sp.aretes_implicites
    ));
    let Construction { mut g, frag } = c;
    g.geometry_start.push(0);
    for &k in &frag {
        let pts = &frags[k as usize].pts;
        for p in pts {
            let xy = [i32::try_from(p[0]).ctx("coordonnée hors de 32 bits")?, i32::try_from(p[1]).ctx("coordonnée")?];
            g.points.push(xy);
        }
        g.geometry_start.push(u32::try_from(g.points.len()).ctx("plus de 2³² points")?);
    }
    drop(frags);
    dire(&format!("graphe : {} nœuds, {} arêtes, {} points", g.node_count(), g.edge_count(), g.points.len()));
    g.order = ordre_cch(&g);
    dire("ordre CCH calculé");
    let erreur = std::cell::RefCell::new(None);
    let b = g
        .encode(|s| {
            crate::zst(s, 6).unwrap_or_else(|e| {
                erreur.borrow_mut().get_or_insert(e);
                Vec::new()
            })
        })
        .ctx("graphe incohérent")?;
    if let Some(e) = erreur.into_inner() {
        return Err(e);
    }
    let empreinte = blake3::hash(&b).to_hex()[..16].to_string();
    dire(&format!("codé : {:.1} Mo, empreinte {empreinte}", b.len() as f64 / 1e6));
    // Relu comme le fera le serveur.
    let relu = RoadGraph::decode(&b).ctx("relecture du graphe")?;
    if relu != g {
        return Err("graphe relu différent".into());
    }
    let partiel = sortie.with_extension("partiel");
    std::fs::write(&partiel, &b).ctx(partiel.display())?;
    std::fs::rename(&partiel, sortie).ctx(sortie.display())?;
    Ok(empreinte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn morceau(id: u64, rang: u16, pts: &[[i64; 2]]) -> Morceau {
        Morceau { id, rang, attr: RoadAttr::default(), pts: pts.iter().map(|p| [p[0], p[1], 0]).collect() }
    }

    #[test]
    fn recousure_associative() {
        // Tronçon 1 coupé en trois (rangs 0, 1, 2), tronçon 2 avec un trou (rangs 0 et 2), un doublon.
        let ms = vec![
            morceau(1, 2, &[[20, 0], [30, 0]]),
            morceau(1, 0, &[[0, 0], [10, 0]]),
            morceau(2, 2, &[[5, 5], [6, 6]]),
            morceau(1, 1, &[[10, 0], [20, 0]]),
            morceau(2, 0, &[[0, 5], [1, 5]]),
            morceau(2, 0, &[[0, 5], [1, 5]]),
        ];
        // Deux départements qui ont lu la même tile : chacun a le morceau (2, 0).
        let (a, b) = (&ms[..5], &ms[4..]);
        let mut f = recoudre(de_morceaux(a.to_vec())).0;
        f.extend(recoudre(de_morceaux(b.to_vec())).0);
        let (croise, sc) = recoudre(f);
        assert_eq!((croise.len(), sc.doublons), (3, 1));
        let (tout, st) = recoudre(de_morceaux(ms.clone()));
        assert_eq!((tout.len(), st.jointures, st.trous, st.doublons), (3, 2, 1, 1));
        assert_eq!(tout[0].pts.len(), 4);
        // Par moitiés (deux « départements »), puis ensemble : même résultat.
        let (a, b) = ms.split_at(3);
        let mut f = recoudre(de_morceaux(a.to_vec())).0;
        f.extend(recoudre(de_morceaux(b.to_vec())).0);
        let deux = recoudre(f).0;
        let cle = |v: &[Fragment]| v.iter().map(|f| (f.id, f.r0, f.r1, f.pts.clone())).collect::<Vec<_>>();
        assert_eq!(cle(&deux), cle(&tout));
        assert_eq!(cle(&croise), cle(&tout));
        // Morceau dont le rang est couvert par un fragment déjà recousu : retiré.
        let mut f = tout.clone();
        f.extend(de_morceaux(vec![morceau(1, 1, &[[10, 0], [20, 0]])]));
        let (r, s) = recoudre(f);
        assert_eq!((cle(&r), s.doublons), (cle(&tout), 1));
        // Chevauchement partiel (rangs 1 à 3 après 0 à 2) : gardé à part, compté.
        let mut c = de_morceaux(vec![morceau(1, 1, &[[10, 0], [40, 0]])]);
        c[0].r1 = 3;
        let mut f = tout.clone();
        f.extend(c);
        let (r, s) = recoudre(f);
        assert_eq!((r.len(), s.chevauchements), (4, 1));
    }

    #[test]
    fn charges() {
        let a = RoadAttr { nature: 2, importance: 4, width_dm: 60, ..Default::default() };
        // Restriction réelle : prioritaire (dt arrondis au-dessus).
        assert_eq!(charge(None, &RoadAttr { weight_dt: 191, ..a }, 50_000), 20);
        // Importance 4 : 44 t ; passerelle sous 2,5 m : 3,5 t.
        assert_eq!(charge(None, &a, 50_000), 44);
        assert_eq!(charge(None, &RoadAttr { width_dm: 20, ..a }, 50_000), 4);
    }
}
