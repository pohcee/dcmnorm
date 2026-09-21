#!/usr/bin/env bash
# Builds the test-website Docker image and deploys it to Cloud Run as a public
# (--allow-unauthenticated) service. Safe to run from any directory - it resolves the repo root
# itself, since the image's Docker build context must be the dcmnorm repo root (see Dockerfile's
# own top comment for why).
#
# Prerequisites:
#   - gcloud CLI, authenticated (`gcloud auth login`) against a GCP project with the
#     Artifact Registry and Cloud Run APIs enabled.
#   - Docker, logged in for pushes (this script runs `gcloud auth configure-docker` for you).
#
# Usage:
#   ./deploy-cloudrun.sh <PROJECT_ID> [REGION] [SERVICE_NAME]
#   PROJECT_ID=my-gcp-project ./deploy-cloudrun.sh
#
# Config (env vars, all optional except PROJECT_ID):
#   PROJECT_ID    GCP project id (required; or pass as $1)
#   REGION        Cloud Run region (default: us-central1)
#   SERVICE_NAME  Cloud Run service name (default: dcmnorm-test-website)
#   REPOSITORY    Artifact Registry repo name (default: dcmnorm)
#   MAX_UPLOAD_BYTES  Cap on uploaded file size the app will accept (default: 25000000, ~25MB -
#                     Cloud Run's own hard request-body limit is 32MB, so this stays under it).
set -euo pipefail

PROJECT_ID="${1:-${PROJECT_ID:-}}"
REGION="${2:-${REGION:-us-central1}}"
SERVICE_NAME="${3:-${SERVICE_NAME:-dcmnorm-test-website}}"
REPOSITORY="${REPOSITORY:-dcmnorm}"
MAX_UPLOAD_BYTES="${MAX_UPLOAD_BYTES:-25000000}"

if [[ -z "$PROJECT_ID" ]]; then
  echo "Usage: $0 <PROJECT_ID> [REGION] [SERVICE_NAME]" >&2
  echo "  (or set PROJECT_ID, and optionally REGION/SERVICE_NAME/REPOSITORY, as env vars)" >&2
  exit 1
fi

for cmd in gcloud docker; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: '$cmd' is required but not found on PATH" >&2
    exit 1
  fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
DOCKERFILE="$SCRIPT_DIR/Dockerfile"

IMAGE="${REGION}-docker.pkg.dev/${PROJECT_ID}/${REPOSITORY}/${SERVICE_NAME}"
TAG="$(date -u +%Y%m%dT%H%M%SZ)"

echo "==> Repo root:   $REPO_ROOT"
echo "==> Dockerfile:  $DOCKERFILE"
echo "==> Image:       ${IMAGE}:${TAG}"
echo "==> Project:     $PROJECT_ID"
echo "==> Region:      $REGION"
echo "==> Service:     $SERVICE_NAME"
echo

echo "==> Ensuring Artifact Registry repository '${REPOSITORY}' exists in ${REGION}..."
if ! gcloud artifacts repositories describe "$REPOSITORY" \
  --project="$PROJECT_ID" --location="$REGION" >/dev/null 2>&1; then
  gcloud artifacts repositories create "$REPOSITORY" \
    --project="$PROJECT_ID" \
    --location="$REGION" \
    --repository-format=docker \
    --description="Images for dcmnorm test/demo services"
fi

echo "==> Configuring Docker auth for ${REGION}-docker.pkg.dev..."
gcloud auth configure-docker "${REGION}-docker.pkg.dev" --quiet --project="$PROJECT_ID"

echo "==> Building image (context: repo root, see Dockerfile comment)..."
docker build -f "$DOCKERFILE" -t "${IMAGE}:${TAG}" -t "${IMAGE}:latest" "$REPO_ROOT"

echo "==> Pushing image..."
docker push "${IMAGE}:${TAG}"
docker push "${IMAGE}:latest"

echo "==> Deploying to Cloud Run (public - --allow-unauthenticated)..."
gcloud run deploy "$SERVICE_NAME" \
  --project="$PROJECT_ID" \
  --region="$REGION" \
  --image="${IMAGE}:${TAG}" \
  --platform=managed \
  --allow-unauthenticated \
  --memory=1Gi \
  --cpu=1 \
  --max-instances=3 \
  --set-env-vars="MAX_UPLOAD_BYTES=${MAX_UPLOAD_BYTES}"

SERVICE_URL="$(gcloud run services describe "$SERVICE_NAME" \
  --project="$PROJECT_ID" --region="$REGION" \
  --format="value(status.url)")"

echo
echo "==> Deployed: $SERVICE_URL"
