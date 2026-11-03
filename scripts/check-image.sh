#!/usr/bin/env bash
# Run a built image as a deployment would, against a Postgres of its own:
# `migrate`, then a worker under the required sandbox until its startup
# probes pass, then SIGTERM. What the Dockerfile assembled is what this
# exercises: the image's configuration file, the apt packages and fonts,
# the helpers on PATH, the writable directories and, in the office
# variant, LibreOffice and pandoc.
#
#   scripts/check-image.sh <image>
set -euo pipefail

image=${1:?usage: check-image.sh <image>}
run=agrafe-check-$$
postgres=${run}-postgres
worker=${run}-worker

cleanup() {
  docker rm --force "$postgres" "$worker" >/dev/null 2>&1 || true
  docker network rm "$run" >/dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
  echo "::error::$image: $1" >&2
  docker logs "$worker" 2>&1 | tail -n 50 >&2 || true
  exit 1
}

user=$(docker image inspect --format '{{.Config.User}}' "$image")
[[ $user == 10001:10001 ]] || fail "it runs as '$user', not 10001:10001"

docker network create "$run" >/dev/null
docker run --detach --name "$postgres" --network "$run" \
  --env POSTGRES_PASSWORD=check --env POSTGRES_DB=agrafe postgres:17 >/dev/null
# Over TCP: while the entrypoint initialises the database, its server
# listens on the socket alone, and restarts after.
for _ in $(seq 60); do
  docker exec "$postgres" pg_isready --quiet --host 127.0.0.1 --username postgres && break
  sleep 1
done
docker exec "$postgres" pg_isready --quiet --host 127.0.0.1 --username postgres \
  || fail "postgres did not start"

environment=(
  --env "AGRAFE_DATABASE__URL=postgres://postgres:check@$postgres/agrafe"
  --env "AGRAFE_SIGNING__CURRENT_KEY=k1"
  --env "AGRAFE_SIGNING__KEYS__K1=$(openssl rand -base64 32)"
  --env "AGRAFE_ENCRYPTION__CURRENT_KEK=kek1"
  --env "AGRAFE_ENCRYPTION__KEKS__KEK1=$(openssl rand -base64 32)"
  --env "AGRAFE_SERVER__PUBLIC_BASE_URL=https://files.example.org"
  --env "AGRAFE_MEDIA__SANDBOX__ISOLATION=required"
)

docker run --rm --network "$run" "${environment[@]}" "$image" migrate

docker run --detach --name "$worker" --network "$run" "${environment[@]}" \
  "$image" serve --role worker >/dev/null

expected=("sandbox: " "renderer probe passed" "document compiler probe passed")
for _ in $(seq 180); do
  [[ $(docker inspect --format '{{.State.Running}}' "$worker") == true ]] \
    || fail "the worker stopped during startup"
  logs=$(docker logs "$worker" 2>&1)
  missing=0
  for line in "${expected[@]}"; do
    [[ $logs == *"$line"* ]] || missing=1
  done
  [[ $missing == 0 ]] && break
  sleep 1
done
[[ $missing == 0 ]] || fail "the startup probes did not pass in three minutes"

docker exec "$worker" touch /var/lib/agrafe/data/.check /var/tmp/agrafe/.check \
  || fail "the data directories are not writable"

docker stop --timeout 30 "$worker" >/dev/null
status=$(docker inspect --format '{{.State.ExitCode}}' "$worker")
[[ $status == 0 ]] || fail "the worker exited with $status on SIGTERM"
echo "$image: migrate, the startup probes and SIGTERM passed"
