#!/usr/bin/env bash
# Wait for Garage's S3 API and then upload everything under .dev/seed/.
# Idempotent — re-runs just overwrite existing keys.
set -euo pipefail

EP="${GARAGE_ENDPOINT:-http://garage:3900}"
BUCKET="${RAWDB_BUCKET:-rawdb}"

echo "[seed] waiting for S3 endpoint $EP..."
for i in $(seq 1 60); do
    # ListBuckets requires auth; HEAD on root returns 403 *with* signed creds
    # missing, but the TCP connect tells us the daemon is listening.
    if aws --endpoint-url "$EP" s3 ls "s3://$BUCKET" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
aws --endpoint-url "$EP" s3 ls "s3://$BUCKET" >/dev/null

cd /seed
# Walk the seed tree and upload each file under its full relative path,
# preserving spaces and the maker/model/category structure.
find . -type f | while read -r f; do
    rel="${f#./}"
    aws --endpoint-url "$EP" s3 cp "$f" "s3://$BUCKET/$rel"
done

echo "[seed] done"
