# beta1-datachain

Chaîne de données de la beta 1 de **La Bataille de France** : elle transforme les données ouvertes de l'IGN (BD TOPO, RGE ALTI 1 m) et de l'INSEE (carreaux de population) en **tiles** de 1 km, que le serveur publie et que le client télécharge à la demande.

**En construction** : le code est porté des prototypes P-07 et P-26, étape par étape.

| Étape | État |
|---|---|
| Manifeste des sources et téléchargement | fait |
| Chaîne (un processus par département), jeu d'essai de Brest en CI | fait |
| Graphe routier national (CCH), pyramide de la carte | à porter |
| Publication adressée par ses entrées (manifeste + digest de l'image) | à porter |
| Image et Job du chart `game` (PVC `sources` et `tiles`, [H-06](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#h-06--diffusion-des-tiles)) | à faire |

Format des tiles (en-tête, sections, relief, codes) et géographie : [beta1-common](https://github.com/labatailledefrance/beta1-common) `tile`, partagé avec le serveur et le client ; la chaîne n'y ajoute que la compression zstd. `chaine controle` relit chaque tile avec les décodeurs du jeu.

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

France entière : 414 fichiers, 718 Go (BD TOPO, RGE ALTI 1 m et 5 m, INSEE). Pas de LiDAR HD ([B-01](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#b-01--chaîne-de-traitement-des-données-ign)).

## Chaîne

Portée de P-26 : mêmes octets que le prototype (Finistère entier : 18 323 tiles, empreinte identique ; 220 s et 9,7 Go de mémoire sur 16 threads).

```sh
target/release/chaine extraire --source …/BDTOPO_…_D029_….7z --sortie D029.gpkg
target/release/chaine relief --source …/RGEALTI_2-0_1M_ASC_…_D029_….7z --sortie relief/z0_1m_D029.rel
target/release/chaine national --source D029.gpkg … --sortie national-z0.bin     # communes et côtes de la zone
target/release/chaine departement --gpkg D029.gpkg --dept 29 --zone 0 \
  --national national-z0.bin --relief relief --insee200 carreaux-200m.parquet --sortie tiles-D029
target/release/chaine controle tiles-D029                                         # décodage, doublons, raccords
```

Le relief vient du RGE ALTI 1 m seul, le 5 m en secours ([B-01](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#b-01--chaîne-de-traitement-des-données-ign)).

### Jeu d'essai de Brest

[`essais/brest.sh`](essais/brest.sh) produit 25 tiles autour de Brest depuis les vraies sources du Finistère et compare leur empreinte à [`essais/brest.empreinte`](essais/brest.empreinte). La CI le lance à chaque PR ; les entrées préparées (3,2 Go) sont en cache, l'IGN n'est sollicité qu'au changement d'édition. Une modification voulue de la sortie met à jour l'empreinte dans la même PR.

```sh
essais/brest.sh preparer /tmp/essai   # ~5 Go téléchargés, 3 min
essais/brest.sh verifier /tmp/essai   # 12 s, 2,8 Go de mémoire
```

## Licences

Données : Licence Ouverte Etalab 2.0 (IGN, INSEE). Code : AGPL-3.0-or-later. Contribution : [CONTRIBUTING.md](https://github.com/labatailledefrance/.github/blob/main/CONTRIBUTING.md).
