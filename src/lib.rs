//! Sources et chaîne de production des tiles de La Bataille de France.
//!
//! Étape 1 : le **manifeste des sources** ([`manifeste`]), l'équivalent d'un
//! lockfile pour les données (URL, taille, empreintes), son rafraîchissement
//! depuis le service de l'IGN ([`inventaire`]) et le téléchargement avec reprise
//! et contrôle d'intégrité ([`telechargement`]).

pub mod empreinte;
pub mod inventaire;
pub mod manifeste;
pub mod telechargement;
