//! Écriture des tiles et des paquets. Le format lui-même (en-tête, sections,
//! empreinte, paquets, relief) est dans `beta1_common::tile`, partagé avec le
//! serveur et le client ; ici, seulement la compression (bibliothèque zstd,
//! plus rapide et mieux compressée que l'encodeur en Rust pur) et la lecture
//! d'une sortie complète pour les contrôles.

pub use beta1_common::codec::Writer as Ecrivain;
pub use beta1_common::tile::{Header as Entete, QUANT, Section, SectionId as Sec, decode as lire};

use crate::erreur::{Contexte, Res};
use crate::geo::TileId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Compresse une section au niveau zstd donné.
pub fn compresser(sec: Sec, brut: &[u8], niveau: i32) -> Res<Section> {
    Ok(Section { id: sec, raw_len: brut.len(), data: crate::zst(brut, niveau)? })
}

/// Assemble une tile.
pub fn assembler(e: &Entete, sections: &[Section]) -> Vec<u8> {
    beta1_common::tile::encode(e, sections)
}

/// Bloc (paquet) d'une tile.
pub fn bloc(t: TileId) -> (i32, i32) {
    beta1_common::tile::pack_of(t.ix, t.iy)
}

/// Écrit un paquet ; tiles dans l'ordre (iy, ix).
pub fn paquet(tiles: &[(TileId, &[u8])]) -> Vec<u8> {
    let v: Vec<(i32, i32, &[u8])> = tiles.iter().map(|(t, b)| (t.ix, t.iy, *b)).collect();
    beta1_common::tile::encode_pack(&v)
}

/// Index d'un paquet : (tile, décalage, taille).
pub fn index_paquet(b: &[u8]) -> Res<Vec<(TileId, u64, usize)>> {
    Ok(beta1_common::tile::decode_pack_index(b)
        .ctx("paquet invalide")?
        .into_iter()
        .map(|e| (TileId { ix: e.ix, iy: e.iy }, e.offset, e.len as usize))
        .collect())
}

/// Quantification d'une coordonnée au 1/16 m, relative au centre de la tile.
pub fn q(v: f64, centre: f64) -> i64 {
    ((v - centre) * QUANT).round() as i64
}

/// Index de tous les paquets d'une sortie : tile → (fichier, décalage, taille).
pub struct Paquets {
    pub index: BTreeMap<TileId, (PathBuf, u64, usize)>,
}

pub fn paquets(sortie: &Path) -> Res<Paquets> {
    let dossier = sortie.join("paquets");
    let mut index = BTreeMap::new();
    let mut fichiers = std::fs::read_dir(&dossier)
        .ctx(dossier.display())?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .ctx(dossier.display())?;
    fichiers.sort();
    for f in fichiers {
        let b = std::fs::read(&f).ctx(f.display())?;
        for (t, off, n) in index_paquet(&b).ctx(f.display())? {
            index.insert(t, (f.clone(), off, n));
        }
    }
    Ok(Paquets { index })
}
