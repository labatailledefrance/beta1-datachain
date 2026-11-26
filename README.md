# beta1-datachain

Chaîne de données de la beta 1 de **La Bataille de France** : elle transforme les données ouvertes de l'IGN (BD TOPO, LiDAR HD, RGE ALTI) et de l'INSEE (carreaux de population) en **tiles** de 1 km, que le serveur publie et que le client télécharge à la demande.

**En construction** : le code est porté des prototypes P-07 et P-26, étape par étape.

| Étape | État |
|---|---|
| Manifeste des sources et téléchargement | fait |
| LiDAR HD (WMS, blocs de 5 km) | à porter (P-26 `outils/lidar.py`) |
| Chaîne (un processus par département), jeu d'essai de Brest en CI | à porter (P-26 `chaine/`) |
| Graphe routier national (CCH), pyramide de la carte | à porter |
| Publication immuable des tiles (un dossier par département et par hash) | à porter |

Format des tiles et géographie : [beta1-common](https://github.com/labatailledefrance/beta1-common).

## Sources

Le dépôt ne contient **aucune donnée** : chacun les retélécharge chez l'IGN et l'INSEE. [`sources/manifeste.csv`](sources/manifeste.csv) fige la liste des fichiers, comme un lockfile : URL, taille, MD5 publié par l'IGN, SHA-256 calculé par le projet. Un fichier n'est rendu visible sous son nom définitif qu'après contrôle de sa taille et de ses empreintes ; un téléchargement coupé reprend où il s'était arrêté (plage HTTP). URL en HTTPS, sur les seuls hôtes `data.geopf.fr`, `static.data.gouv.fr` et `www.insee.fr`.

```sh
cargo build --release
# Sources de Brest (BD TOPO et RGE ALTI du Finistère) et carreaux de l'INSEE
target/release/sources telecharger --destination data/sources --zones D029,FRANCE
# Recalculer les empreintes des fichiers présents
target/release/sources verifier --destination data/sources --zones D029,FRANCE
```

Filtres : `--zones` (départements `D029`, `D02A`, `D974`…, ou `FRANCE`), `--ressources` (`BDTOPO`, `RGEALTI`, `INSEE`), `--editions` (morceau du nom de l'édition, par exemple `_5M_`). Codes de sortie : 0 tout est là et vérifié, 1 fichiers manquants ou faux, 2 erreur.

Mise à jour du manifeste (nouvelle édition de la BD TOPO…), relue en PR comme une mise à jour de dépendance :

```sh
target/release/sources inventaire              # flux Atom de la Géoplateforme, métadonnées seulement
target/release/sources empreintes --destination data/sources   # SHA-256 des fichiers présents dont le MD5 est bon
```

France entière : 414 fichiers, 718 Go (BD TOPO, RGE ALTI 1 m et 5 m, INSEE), sans le LiDAR HD.

## Licences

Données : Licence Ouverte Etalab 2.0 (IGN, INSEE). Code : AGPL-3.0-or-later. Contribution : [CONTRIBUTING.md](https://github.com/labatailledefrance/.github/blob/main/CONTRIBUTING.md).
