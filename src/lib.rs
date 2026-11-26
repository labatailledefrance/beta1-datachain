//! Sources et chaîne de production des tiles de La Bataille de France.
//!
//! - Sources : le **manifeste** ([`manifeste`]), l'équivalent d'un lockfile
//!   pour les données (URL, taille, empreintes), son rafraîchissement depuis le
//!   service de l'IGN ([`inventaire`]) et le téléchargement avec reprise et
//!   contrôle d'intégrité ([`telechargement`]).
//! - Chaîne (portée de P-26) : GeoPackage de la BD TOPO ([`gpkg`]), paquets de
//!   relief du RGE ALTI ([`relief`], [`depot`]), référence nationale des
//!   communes et des côtes ([`national`]), production d'un département
//!   ([`chaine`]) et contrôle des raccords entre départements ([`controle`]).

pub mod empreinte;
pub mod erreur;
pub mod inventaire;
pub mod manifeste;
pub mod telechargement;

pub mod chaine;
pub mod controle;
pub mod decodage;
pub mod depot;
pub mod emprise;
pub mod geo;
pub mod gpkg;
pub mod mesure;
pub mod national;
pub mod objets;
pub mod population;
pub mod portes;
pub mod raster;
pub mod relief;
pub mod tuile;
pub mod zone;

#[cfg(test)]
mod tests;

/// Rend la place réservée en trop : `zstd::bulk::compress` réserve la taille
/// du pire cas (2 Mo pour un relief comprimé à 130 ko) et la garde.
pub fn serre(mut v: Vec<u8>) -> Vec<u8> {
    v.shrink_to_fit();
    v
}

/// Compresse en zstd au niveau donné, sans place réservée en trop.
pub fn zst(brut: &[u8], niveau: i32) -> erreur::Res<Vec<u8>> {
    use erreur::Contexte;
    Ok(serre(zstd::bulk::compress(brut, niveau).ctx("compression zstd")?))
}
