#!/bin/bash
# Runs inside the dcmnorm-bench Docker image (see Dockerfile). Benchmarks dcmtk, dcm4che, and
# dcmnorm on equivalent parse/render/transcode operations across a representative fixture set,
# using hyperfine for warmup + statistically stable timing. Writes one JSON result file per
# (operation, fixture, tool) combination plus a combined markdown summary.
set -euo pipefail

FIXTURES_DIR="/fixtures"
OUT_DIR="/results"
mkdir -p "$OUT_DIR"

DCMTK_DCM2JSON=/usr/bin/dcm2json
DCMTK_DCMJ2PNM=/usr/bin/dcmj2pnm
DCMTK_DCMCONV=/usr/bin/dcmconv
DCMTK_DCMDJPEG=/usr/bin/dcmdjpeg
DCM4CHE_DCM2JSON=/opt/dcm4che/bin/dcm2json
DCM4CHE_DCM2JPG=/opt/dcm4che/bin/dcm2jpg
DCM4CHE_DCM2DCM=/opt/dcm4che/bin/dcm2dcm
DCMNORM=/repo/target/release/dcmnorm

EXPLICIT_VR_LE="1.2.840.10008.1.2.1"

HF="hyperfine --warmup 2 --min-runs 8 --export-json"

summary_row() {
  # args: operation fixture tool json_file
  local op="$1" fixture="$2" tool="$3" json="$4"
  local mean stddev median min max
  mean=$(jq -r '.results[0].mean * 1000' "$json")
  stddev=$(jq -r '.results[0].stddev * 1000' "$json")
  median=$(jq -r '.results[0].median * 1000' "$json")
  min=$(jq -r '.results[0].min * 1000' "$json")
  max=$(jq -r '.results[0].max * 1000' "$json")
  printf '%s\t%s\t%s\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f\n' \
    "$op" "$fixture" "$tool" "$mean" "$stddev" "$median" "$min" "$max" >> "$OUT_DIR/summary.tsv"
}

echo -e "operation\tfixture\ttool\tmean_ms\tstddev_ms\tmedian_ms\tmin_ms\tmax_ms" > "$OUT_DIR/summary.tsv"

run() {
  # args: operation fixture tool cmd...
  local op="$1" fixture="$2" tool="$3"; shift 3
  local json="$OUT_DIR/${op}_${fixture}_${tool}.json"
  echo ">>> $op / $fixture / $tool"
  $HF "$json" "$*" > "$OUT_DIR/${op}_${fixture}_${tool}.log" 2>&1 || {
    echo "    FAILED - see $OUT_DIR/${op}_${fixture}_${tool}.log"
    return 0
  }
  summary_row "$op" "$fixture" "$tool" "$json"
}

# Fixture set: representative sizes across the three transfer-syntax families dcmnorm's own
# codec ownership work this session actually touched.
FIXTURES=(mr us2 wsi ct dx2)

for fx in "${FIXTURES[@]}"; do
  src="$FIXTURES_DIR/$fx.dcm"

  # --- Parse (dump to JSON) ---
  # dcmtk's dcm2json has no bulkdata-reference/exclude option in this build (no equivalent to
  # dcm4che's -B/--no-bulkdata or dcmnorm's default bulkData:uri mode) - it always inlines
  # PixelData, and fails outright ("JSON InlineBinary encoding not supported for compressed
  # pixel data") on any compressed source. Not a benchmark-harness bug - confirmed by running it
  # directly outside hyperfine. Skip it for compressed fixtures rather than recording a failure.
  if [[ "$fx" != "ct" && "$fx" != "dx2" && "$fx" != "wsi" ]]; then
    run parse "$fx" dcmtk "$DCMTK_DCM2JSON $src"
  fi
  run parse "$fx" dcm4che "$DCM4CHE_DCM2JSON $src"
  run parse "$fx" dcmnorm "$DCMNORM $src"

  # --- Render (decode pixel data to PNG) ---
  out="$OUT_DIR/render_${fx}"
  # dcmtk's apt-packaged build has no JPEG2000 decoder (no dcmdjp2k, dcmj2pnm errors on .90/.91) -
  # skip it for those fixtures rather than recording a bogus/failed timing.
  if [[ "$fx" != "ct" && "$fx" != "dx2" ]]; then
    run render "$fx" dcmtk "$DCMTK_DCMJ2PNM --write-png $src ${out}_dcmtk.png"
  fi
  run render "$fx" dcm4che "$DCM4CHE_DCM2JPG -F png $src ${out}_dcm4che.png"
  run render "$fx" dcmnorm "$DCMNORM $src ${out}_dcmnorm.png"

  # --- Transcode (to Explicit VR Little Endian - decompresses JPEG/JPEG2000 sources) ---
  out="$OUT_DIR/transcode_${fx}"
  if [[ "$fx" == "ct" || "$fx" == "dx2" ]]; then
    # Same JPEG2000-codec gap as above - dcmconv can't decompress these, skip dcmtk.
    :
  elif [[ "$fx" == "wsi" ]]; then
    run transcode "$fx" dcmtk "$DCMTK_DCMDJPEG $src ${out}_dcmtk.dcm"
  else
    run transcode "$fx" dcmtk "$DCMTK_DCMCONV +te $src ${out}_dcmtk.dcm"
  fi
  run transcode "$fx" dcm4che "$DCM4CHE_DCM2DCM -t $EXPLICIT_VR_LE $src ${out}_dcm4che.dcm"
  run transcode "$fx" dcmnorm "$DCMNORM $src ${out}_dcmnorm.dcm --transfer-syntax $EXPLICIT_VR_LE"
done

echo
echo "=== Summary (ms) ==="
cat "$OUT_DIR/summary.tsv"
