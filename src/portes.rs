//! Porte unique d'un bâtiment (règle de P-08), calculée par la chaîne : arête
//! (rang parmi les anneaux extérieurs, dans l'ordre stocké) et abscisse en
//! huitièmes de mètre. Voir P-08 pour la règle.

use crate::raster::Bitmap;
use std::collections::VecDeque;

pub const MARGE: i32 = 100;
pub const N: usize = 500 + 2 * MARGE as usize;
const LOIN: u16 = u16::MAX;

pub struct Grille {
    acc: Vec<bool>,
    dist: Vec<u16>,
}

/// Une tile vue par la grille : franchissable final, cases exclues, rues.
pub struct Vue<'a> {
    pub franch: &'a Bitmap,
    pub exclus: &'a Bitmap,
    pub hors: &'a Bitmap, // mer et étranger
    pub rues: &'a Bitmap,
}

pub fn grille(v: &[Option<Vue>; 9]) -> Grille {
    let mut acc = vec![false; N * N];
    let mut dist = vec![LOIN; N * N];
    let mut f = VecDeque::new();
    for j in 0..N {
        for i in 0..N {
            let (gi, gj) = (i as i32 - MARGE, j as i32 - MARGE);
            let (tx, ty) = (gi.div_euclid(500), gj.div_euclid(500));
            let Some(t) = &v[((ty + 1) * 3 + tx + 1) as usize] else { continue };
            let (a, b) = (gi.rem_euclid(500) as usize, gj.rem_euclid(500) as usize);
            if t.franch.get(a, b) && !t.exclus.get(a, b) && !t.hors.get(a, b) {
                let k = j * N + i;
                acc[k] = true;
                if t.rues.get(a, b) {
                    dist[k] = 0;
                    f.push_back(k);
                }
            }
        }
    }
    while let Some(k) = f.pop_front() {
        let (i, j, d) = (k % N, k / N, dist[k] + 1);
        let mut v = |kk: usize| {
            if acc[kk] && dist[kk] == LOIN {
                dist[kk] = d;
                f.push_back(kk);
            }
        };
        if i > 0 {
            v(k - 1);
        }
        if i + 1 < N {
            v(k + 1);
        }
        if j > 0 {
            v(k - N);
        }
        if j + 1 < N {
            v(k + N);
        }
    }
    Grille { acc, dist }
}

fn indice(p: [f64; 2]) -> Option<usize> {
    let (i, j) = (((p[0] + 500.0) / 2.0).floor() as i32 + MARGE, ((p[1] + 500.0) / 2.0).floor() as i32 + MARGE);
    (i >= 0 && j >= 0 && (i as usize) < N && (j as usize) < N).then(|| j as usize * N + i as usize)
}

/// (arête, abscisse en 1/8 m, rang : 0 à 2 au pied du mur, 3 = centroïde).
pub fn choisir(polys: &[Vec<Vec<[i64; 2]>>], centre: [f64; 2], g: &Grille) -> Option<(u32, u32, u8)> {
    let m = |p: [i64; 2]| [p[0] as f64 / 16.0, p[1] as f64 / 16.0];
    let mut best: Option<((u16, u8, u32, u32), u32)> = None;
    let mut aretes = Vec::new();
    for poly in polys {
        let Some(ext) = poly.first() else { continue };
        let n = ext.len();
        let aire: i64 = (0..n).map(|i| ext[i][0] * ext[(i + 1) % n][1] - ext[(i + 1) % n][0] * ext[i][1]).sum();
        let sens = if aire >= 0 { 1.0 } else { -1.0 };
        for i in 0..n {
            let (a, b) = (m(ext[i]), m(ext[(i + 1) % n]));
            let ka = aretes.len() as u32;
            aretes.push((a, b));
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1.6 {
                continue;
            }
            let (t, nn) = ([dx / len, dy / len], [sens * dy / len, -sens * dx / len]);
            let ns = ((len - 1.6) / 0.5).floor() as u32 + 1;
            let s0 = (len - (ns - 1) as f64 * 0.5) / 2.0;
            for ks in 0..ns {
                let s = s0 + ks as f64 * 0.5;
                let p = [a[0] + t[0] * s, a[1] + t[1] * s];
                for (r, off) in [1.0f64, 2.0, 3.0].iter().enumerate() {
                    let Some(k) = indice([p[0] + nn[0] * off, p[1] + nn[1] * off]) else { continue };
                    if !g.acc[k] {
                        continue;
                    }
                    let cle = (g.dist[k], r as u8, ka, ks);
                    if best.as_ref().is_none_or(|x| cle < x.0) {
                        best = Some((cle, (s * 8.0).round() as u32));
                    }
                    break;
                }
            }
        }
    }
    if let Some((c, s)) = best {
        return Some((c.2, s, c.1));
    }
    // Enclavé : case accessible la plus proche du centroïde (120 m au plus),
    // puis point du mur le plus proche de cette case.
    let ci = (((centre[0] + 500.0) / 2.0).floor() as i32, ((centre[1] + 500.0) / 2.0).floor() as i32);
    let mut pr: Option<(f64, i32, i32)> = None;
    for dj in -60..=60 {
        for di in -60..=60 {
            let (i, j) = (ci.0 + di, ci.1 + dj);
            let p = [i as f64 * 2.0 - 499.0, j as f64 * 2.0 - 499.0];
            if indice(p).is_some_and(|k| g.acc[k]) {
                let d = (p[0] - centre[0]).powi(2) + (p[1] - centre[1]).powi(2);
                if pr.is_none_or(|b| (d, j, i) < (b.0, b.1, b.2)) {
                    pr = Some((d, j, i));
                }
            }
        }
    }
    let (_, j, i) = pr?;
    let q = [i as f64 * 2.0 - 499.0, j as f64 * 2.0 - 499.0];
    let mut bp: Option<(f64, u32, u32)> = None;
    for (k, (a, b)) in aretes.iter().enumerate() {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let l2 = dx * dx + dy * dy;
        if l2 == 0.0 {
            continue;
        }
        let t = (((q[0] - a[0]) * dx + (q[1] - a[1]) * dy) / l2).clamp(0.0, 1.0);
        let d = (a[0] + t * dx - q[0]).powi(2) + (a[1] + t * dy - q[1]).powi(2);
        if bp.is_none_or(|x| d < x.0) {
            bp = Some((d, k as u32, (t * l2.sqrt() * 8.0).round() as u32));
        }
    }
    bp.map(|x| (x.1, x.2, 3))
}
