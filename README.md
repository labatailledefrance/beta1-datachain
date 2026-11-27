# beta1-datachain

Chaîne de données de la beta 1 de **La Bataille de France** : elle transforme les données ouvertes de l'IGN (BD TOPO, RGE ALTI 1 m) et de l'INSEE (carreaux de population) en **tiles** de 1 km, que le serveur publie et que le client télécharge à la demande.

**En construction** : le code est porté des prototypes P-07 et P-26, étape par étape.

| Étape | État |
|---|---|
| Manifeste des sources et téléchargement | fait |
| Chaîne (un processus par département), jeu d'essai de Brest en CI | fait |
| Graphe routier national (ordre CCH) | fait |
| Pyramide de la carte | à porter |
| Publication adressée par ses entrées (manifeste + digest de l'image), binaire `datachain`, image signée | fait |
| Job du chart `game` (PVC `sources` et `tiles`, [H-06](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#h-06--diffusion-des-tiles)) | à faire |

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
target/release/chaine graphe --sortie graphe.bin tiles-D029 …                     # graphe routier, ordre CCH
```

### Graphe routier national

`chaine graphe` lit les routes carrossables et les ponts de toutes les tiles des départements donnés, recoud les tronçons coupés aux bords des tiles et des départements, associe les ponts (charge limite par heuristique, [B-03](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#b-03--charge-limite-des-ponts)), puis calcule l'ordre CCH (dissection imbriquée par flot inertiel, crate `cch`, [A-07](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#a-07--recherche-de-chemin)). Sortie : `graphe.bin`, au format de [beta1-common](https://github.com/labatailledefrance/beta1-common) `graph`, relu avant d'être écrit. Finistère : 246 661 nœuds, 294 574 arêtes, 15,3 Mo, 8 s ; mêmes octets que P-26, à 1 comme à 16 threads.

Le relief vient du RGE ALTI 1 m seul, le 5 m en secours ([B-01](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#b-01--chaîne-de-traitement-des-données-ign)).

### Jeu d'essai de Brest

[`essais/brest.sh`](essais/brest.sh) produit 25 tiles autour de Brest depuis les vraies sources du Finistère et compare leur empreinte à [`essais/brest.empreinte`](essais/brest.empreinte), puis celle de leur graphe routier à [`essais/brest-graphe.empreinte`](essais/brest-graphe.empreinte). La CI le lance à chaque PR ; les entrées préparées (3,2 Go) sont en cache, l'IGN n'est sollicité qu'au changement d'édition. Une modification voulue de la sortie met à jour l'empreinte dans la même PR.

```sh
essais/brest.sh preparer /tmp/essai   # ~5 Go téléchargés, 3 min
essais/brest.sh verifier /tmp/essai   # 12 s, 2,8 Go de mémoire
```

## Publication complète : `datachain`

Ce que lance le Job du chart `game` ([H-06](https://github.com/labatailledefrance/documentation/blob/main/OPEN-PROBLEMS.md#h-06--diffusion-des-tiles)) : sources (téléchargement avec reprise), dérivés (GeoPackage, paquets de relief, carreaux de 1 km), référence nationale par zone, un processus `chaine` par département, contrôle croisé (raccords entre départements), graphe routier national, puis publication. Le manifeste est **compilé dans le binaire** : le digest de l'image fixe les sources.

```sh
datachain --sources /donnees/sources --tiles /donnees/tiles --publication <id> [--zones D029,D974] [--processus 2 --threads 8]
```

```text
sources/<édition>/<fichier>            fichiers du manifeste (PVC « sources »)
sources/derives/{gpkg,relief,insee}/   dérivés, réutilisés d'une publication à l'autre
tiles/<id>.partiel/                    publication en cours (PVC « tiles »)
tiles/<id>/departements/D029/…         publication terminée, avec graphe.bin, MANIFESTE (empreinte par département et du graphe) et COMPLET
```

**Départements repris d'une publication à l'autre** : chaque département produit porte une clé (`CLE`, hash de ses sources, du relief et de la référence nationale de sa zone, des carreaux de l'INSEE, de la version de sa production et de l'empreinte de l'essai de Brest). Un département de même clé dans une autre publication du volume, complète ou interrompue, est repris par liens physiques : une nouvelle image qui ne change que le graphe ne refait que le graphe (Finistère : 36 s au lieu de 314). Une modification de la sortie d'un département change l'empreinte de Brest ; si Brest ne la voit pas (outre-mer, montagne…), monter `reutilisation::VERSION`. `datachain --adopter --publication <id>` écrit les clés d'une publication produite par une image antérieure au même code de département.

Une fois la publication complète (ou déjà là), le Job **supprime les anciennes** : il garde la courante et la plus récente des autres (`--garder 1` : les pods de la version précédente la servent encore pendant la bascule), supprime les autres publications complètes et les `.partiel` sans écriture depuis 24 h ; une production en cours n'est jamais touchée.

Chaque résultat est écrit à part puis renommé : un pod tué reprend où il en était ; une publication complète relancée sort aussitôt. Finistère seul : 351 s, 7,8 Go de mémoire (2 processus de 8 threads).

Image `ghcr.io/labatailledefrance/beta1-datachain` (48 Mo), publiée et signée par la CI après fusion : base `dhi.io/static`, non root, système de fichiers en lecture seule possible ; HTTPS par rustls et les certificats racines embarqués.

## Licences

Données : Licence Ouverte Etalab 2.0 (IGN, INSEE). Code : AGPL-3.0-or-later. Contribution : [CONTRIBUTING.md](https://github.com/labatailledefrance/.github/blob/main/CONTRIBUTING.md).
