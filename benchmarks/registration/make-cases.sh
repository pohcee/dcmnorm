#!/usr/bin/env bash
# Builds the registration benchmark's derived series (tcia-cases.json) from the six public TCIA series.
#
#   benchmarks/registration/make-cases.sh <root>
#
# <root> must hold the six source series as directories A, B, P1, P2, M1 and M2 (README.md lists their
# SeriesInstanceUIDs; download them from TCIA with the NBIA Data Retriever or its REST API). Writes the derived series
# (<root>/<name>/) and their true moving -> fixed matrices (<root>/<name>.truth.json) with perturb.py: a known rigid
# move applied through ImagePositionPatient / ImageOrientationPatient (the pixels are untouched), slices dropped from
# the top or bottom, and a smaller circular reconstruction field of view. Needs python3 with numpy and pydicom.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${1:?usage: make-cases.sh <root>}"
for s in A B P1 P2 M1 M2; do [[ -d "$root/$s" ]] || { echo "missing $root/$s" >&2; exit 1; }; done
p() { python3 -I "$here/perturb.py" "$@" >/dev/null; }
# CT, RIDER Lung CT repeat pair (fixed A, moving B; B -> A from ref-BtoA.json)
p "$root/B" "$root/B-fov" --fov 250 --truth-in "$here/ref-BtoA.json" --truth-out "$root/B-fov.truth.json"
p "$root/B" "$root/B-crop" --drop-top 60 --truth-in "$here/ref-BtoA.json" --truth-out "$root/B-crop.truth.json"
p "$root/B" "$root/B-tr" --t 15,-20,40 --truth-in "$here/ref-BtoA.json" --truth-out "$root/B-tr.truth.json"
p "$root/B" "$root/B-rot" --rot 3,2,4 --t 15,-20,40 --truth-in "$here/ref-BtoA.json" --truth-out "$root/B-rot.truth.json"
p "$root/B" "$root/B-s1" --rot 3,2,4 --t 15,-20,40 --drop-top 60 --fov 250 --truth-in "$here/ref-BtoA.json" --truth-out "$root/B-s1.truth.json"
p "$root/A" "$root/A-self-s1" --rot 3,2,4 --t 15,-20,40 --drop-top 60 --truth-out "$root/A-self-s1.truth.json"
p "$root/A" "$root/A-cropbot40" --drop-bottom 40 --truth-out "$root/A-cropbot40.truth.json"
# PT, RIDER Lung PET-CT, one patient 6 months apart (fixed P1, moving P2; P2 -> P1 from ref-PT.json)
p "$root/P1" "$root/P1-self" --rot 2,-3,3 --t 20,-15,60 --drop-top 150 --truth-out "$root/P1-self.truth.json"
p "$root/P2" "$root/P2-s1" --rot 2,-3,3 --t 20,-15,60 --drop-top 150 --truth-in "$here/ref-PT.json" --truth-out "$root/P2-s1.truth.json"
p "$root/P1" "$root/P1-cropbot" --drop-bottom 200 --truth-out "$root/P1-cropbot.truth.json"
# MR, QIN-PROSTATE-Repeatability T2 axial pair (fixed M1, moving M2; M2 -> M1 from ref-MR.json)
p "$root/M1" "$root/M1-self" --rot 3,2,-4 --t 10,-10,20 --drop-bottom 18 --truth-out "$root/M1-self.truth.json"
p "$root/M2" "$root/M2-s1" --rot 3,2,-4 --t 10,-10,20 --drop-bottom 18 --truth-in "$here/ref-MR.json" --truth-out "$root/M2-s1.truth.json"
echo "derived series written under $root"
