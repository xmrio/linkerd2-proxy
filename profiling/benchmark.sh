#!/bin/sh

set -eu

FORTIO_IMAGE="${FORTIO_IMAGE:-fortio/fortio:latest}"

run_id=$(tr -dc a-z0-9 </dev/urandom |dd bs=5 count=1 2>/dev/null)

server_id=$(docker create --name "server-${run_id}" \
  "${FORTIO_IMAGE}" server)

target_port=8080
client_id=$(docker create  --name "client-${run_id}" \
  --network "container:server-${run_id}" \
  "${FORTIO_IMAGE}" load \
    -qps 10000 \
    -t 10s \
    "http://localhost:${target_port}")

docker start "$server_id" >/dev/null

docker start -a "$client_id"
docker rm "$client_id"

docker stop "$server_id" >/dev/null
docker rm "$server_id" >/dev/null
