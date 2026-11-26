#!/usr/bin/env bash
# Jeu d'essai de Brest : 25 tiles (5 × 5 km) autour du centre-ville, produites
# depuis les vraies sources du Finistère, et leur empreinte comparée à
# essais/brest.empreinte. La CI le lance à chaque PR.
#
#   essais/brest.sh preparer DOSSIER   sources → entrées (GeoPackage, relief réduit, national) ; ~5 Go téléchargés
#   essais/brest.sh verifier DOSSIER   entrées → tiles, comparées à l'empreinte attendue
#
# Une modification voulue de la sortie de la chaîne met à jour
# essais/brest.empreinte dans la même PR, comme un test de référence.
set -euo pipefail

ICI=$(cd "$(dirname "$0")" && pwd)
RACINE=$(dirname "$ICI")
BIN=${BIN:-$RACINE/target/release}
ETAPE=${1:?preparer ou verifier}
D=${2:?dossier de travail}
# Boîte du relief gardé (km Lambert-93) : les tiles d'essai et une marge de 15 km.
BOITE=129,6821,163,6855
TILES=$(for x in 144 145 146 147 148; do for y in 6836 6837 6838 6839 6840; do printf 'x%s_y%s,' "$x" "$y"; done; done)
TILES=${TILES%,}

case $ETAPE in
  preparer)
    mkdir -p "$D/entrees/relief"
    "$BIN/sources" --manifeste "$RACINE/sources/manifeste.csv" telecharger --destination "$D/sources" \
      --zones D029,FRANCE --editions BDTOPO,RGEALTI,FILOSOFI_2021_CARREAUX_200M
    "$BIN/chaine" extraire --source "$D"/sources/BDTOPO_*_D029_*/*.7z --sortie "$D/entrees/D029.gpkg"
    for pas in 1M 5M; do
      p=$(tr M m <<<"$pas")
      "$BIN/chaine" relief --source "$D"/sources/RGEALTI_2-0_${pas}_ASC_*_D029_*/*.7z \
        --sortie "$D/entrees/relief/z0_${p}_D029.rel" --boite "$BOITE"
    done
    "$BIN/chaine" national --source "$D/entrees/D029.gpkg" --sortie "$D/entrees/national-z0.bin"
    cp "$D"/sources/FILOSOFI_2021_CARREAUX_200M/*.parquet "$D/entrees/carreaux-200m.parquet"
    # Les archives ne servent plus : place libérée sur le runner.
    rm -rf "$D/sources"
    ;;
  verifier)
    rm -rf "$D/sortie"
    "$BIN/chaine" departement --gpkg "$D/entrees/D029.gpkg" --dept 29 --zone 0 \
      --national "$D/entrees/national-z0.bin" --relief "$D/entrees/relief" \
      --insee200 "$D/entrees/carreaux-200m.parquet" \
      --sortie "$D/sortie" --etiquette essai-brest --essai "$TILES"
    "$BIN/chaine" controle "$D/sortie"
    obtenue=$(awk '$1 == "empreinte" { print $2 }' "$D/sortie/controles.txt")
    attendue=$(tr -d '[:space:]' < "$ICI/brest.empreinte")
    echo "empreinte obtenue : $obtenue"
    echo "empreinte attendue : $attendue"
    test "$obtenue" = "$attendue"
    ;;
  *)
    echo "étape inconnue : $ETAPE (preparer ou verifier)" >&2
    exit 2
    ;;
esac
