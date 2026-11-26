//! Empreintes d'un fichier (MD5 publié par l'IGN, SHA-256 calculé par le projet).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use md5::{Digest, Md5};
use sha2::Sha256;

/// Empreintes hexadécimales (minuscules) d'un fichier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Empreintes {
    pub md5: String,
    pub sha256: String,
}

/// Lit le fichier une fois et calcule les deux empreintes.
pub fn calculer(chemin: &Path) -> io::Result<Empreintes> {
    let mut f = File::open(chemin)?;
    let (mut a, mut b) = (Md5::new(), Sha256::new());
    let mut tampon = vec![0u8; 1 << 22];
    loop {
        let n = f.read(&mut tampon)?;
        if n == 0 {
            break;
        }
        a.update(&tampon[..n]);
        b.update(&tampon[..n]);
    }
    Ok(Empreintes { md5: hex(&a.finalize()), sha256: hex(&b.finalize()) })
}

fn hex(octets: &[u8]) -> String {
    octets.iter().map(|o| format!("{o:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vecteurs_connus() {
        let d = std::env::temp_dir().join(format!("beta1-empreinte-{}", std::process::id()));
        std::fs::write(&d, b"abc").unwrap();
        let e = calculer(&d).unwrap();
        std::fs::remove_file(&d).unwrap();
        assert_eq!(e.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(e.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
