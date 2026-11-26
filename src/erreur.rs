//! Erreurs de la chaîne : un message, avec le contexte ajouté en remontant
//! (« D029.gpkg : table batiment : … »). La chaîne s'arrête à la première
//! erreur ; le message doit dire où chercher.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erreur(pub String);

pub type Res<T> = Result<T, Erreur>;

impl fmt::Display for Erreur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Erreur {}

impl From<String> for Erreur {
    fn from(s: String) -> Self {
        Erreur(s)
    }
}

impl From<&str> for Erreur {
    fn from(s: &str) -> Self {
        Erreur(s.to_owned())
    }
}

macro_rules! depuis {
    ($($t:ty),*) => {
        $(impl From<$t> for Erreur {
            fn from(e: $t) -> Self {
                Erreur(e.to_string())
            }
        })*
    };
}

depuis!(
    std::io::Error,
    rusqlite::Error,
    parquet::errors::ParquetError,
    sevenz_rust2::Error,
    std::num::ParseIntError,
    std::num::ParseFloatError
);

/// `.ctx("…")` : convertit une `Option` ou un `Result` en `Res`, avec contexte.
pub trait Contexte<T> {
    fn ctx(self, c: impl fmt::Display) -> Res<T>;
}

impl<T> Contexte<T> for Option<T> {
    fn ctx(self, c: impl fmt::Display) -> Res<T> {
        self.ok_or_else(|| Erreur(c.to_string()))
    }
}

impl<T, E: fmt::Display> Contexte<T> for Result<T, E> {
    fn ctx(self, c: impl fmt::Display) -> Res<T> {
        self.map_err(|e| Erreur(format!("{c} : {e}")))
    }
}

/// Lit un entier petit-boutien à `p` dans `b`, ou une erreur si `b` est trop court.
pub fn le<const N: usize>(b: &[u8], p: usize) -> Res<[u8; N]> {
    b.get(p..p + N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| Erreur(format!("données tronquées : {N} octets attendus en {p}, {} disponibles", b.len())))
}
