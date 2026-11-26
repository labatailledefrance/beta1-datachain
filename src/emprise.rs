//! Nettoyage des emprises auto-sécantes (P-08) : règle pair-impair, en entiers
//! au 1/16 m ; une emprise simple n'est pas touchée.

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::single::SingleIntOverlay;
use i_overlay::i_float::int::point::IntPoint;

type P = [i64; 2];

fn orient(a: P, b: P, c: P) -> i64 {
    ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).signum()
}

/// Un anneau se coupe-t-il (croisement franc entre deux arêtes) ?
pub fn auto_secant(a: &[P]) -> bool {
    let n = a.len();
    for i in 0..n {
        let (p, q) = (a[i], a[(i + 1) % n]);
        for j in i + 2..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            let (r, s) = (a[j], a[(j + 1) % n]);
            if p[0].max(q[0]) < r[0].min(s[0])
                || r[0].max(s[0]) < p[0].min(q[0])
                || p[1].max(q[1]) < r[1].min(s[1])
                || r[1].max(s[1]) < p[1].min(q[1])
            {
                continue;
            }
            if orient(p, q, r) * orient(p, q, s) < 0 && orient(r, s, p) * orient(r, s, q) < 0 {
                return true;
            }
        }
    }
    false
}

/// Polygones (anneaux ouverts) nettoyés ; `None` si rien à faire.
pub fn nettoyer(poly: &[Vec<P>]) -> Option<Vec<Vec<Vec<P>>>> {
    if !poly.iter().any(|a| auto_secant(a)) {
        return None;
    }
    let s: Vec<Vec<IntPoint>> =
        poly.iter().map(|a| a.iter().map(|p| IntPoint::new(p[0] as i32, p[1] as i32)).collect()).collect();
    let vide: Vec<Vec<IntPoint>> = Vec::new();
    let r = s.overlay(&vide, OverlayRule::Subject, FillRule::EvenOdd);
    Some(
        r.into_iter()
            .map(|s| s.into_iter().map(|c| c.into_iter().map(|p| [p.x as i64, p.y as i64]).collect()).collect())
            .collect(),
    )
}

/// Arrondi au 1/16 m, anneaux ouverts sans points répétés, nettoyés.
pub fn quantifier(polys: &[crate::gpkg::Polygone], cx: f64, cy: f64) -> (Vec<Vec<Vec<P>>>, bool) {
    let mut out = Vec::new();
    let mut change = false;
    for poly in polys {
        let p: Vec<Vec<P>> = poly
            .iter()
            .map(|a| {
                let mut v: Vec<P> = a.iter().map(|p| [crate::tuile::q(p[0], cx), crate::tuile::q(p[1], cy)]).collect();
                v.dedup();
                while v.len() > 1 && v[0] == v[v.len() - 1] {
                    v.pop();
                }
                v
            })
            .collect();
        match nettoyer(&p) {
            Some(n) if !n.is_empty() => {
                change = true;
                out.extend(n);
            }
            _ => out.push(p),
        }
    }
    (out, change)
}
