# beta1-datachain

Chaîne de données de la beta 1 de **La Bataille de France** : elle transforme les données ouvertes de l'IGN (BD TOPO, LiDAR HD, RGE ALTI) et de l'INSEE (carreaux de population) en **tiles** de 1 km, que le serveur publie et que le client télécharge à la demande.

**En construction** : le code sera porté des prototypes P-07 et P-26.

## Ce que fera le dépôt

- un **manifeste des sources** (URL, taille, empreinte) et un téléchargement avec reprise et contrôle d'intégrité : chacun retélécharge les données chez l'IGN et l'INSEE, le dépôt n'en contient aucune ;
- la **chaîne** en Rust, un processus par département, reproductible (mêmes octets à chaque exécution) ;
- le **graphe routier** national (CCH) et la pyramide de la carte ;
- la **publication** immuable des tiles (un dossier par département et par hash).

Format des tiles et géographie : [beta1-common](https://github.com/labatailledefrance/beta1-common).

Données : Licence Ouverte Etalab (IGN, INSEE). Code : AGPL-3.0-or-later. Contribution : [CONTRIBUTING.md](https://github.com/labatailledefrance/.github/blob/main/CONTRIBUTING.md).
