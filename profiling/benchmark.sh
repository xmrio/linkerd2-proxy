#!/bin/sh

set -eu

PROXY_IMAGE="${PROXY_IMAGE:-proxy}"
MOCK_DST_IMAGE="${MOCK_DST_IMAGE:-mock-dst}"
FORTIO_IMAGE="${FORTIO_IMAGE:-fortio/fortio:latest}"

run_id=$(tr -dc a-z0-9 </dev/urandom |dd bs=5 count=1 2>/dev/null)

mock_dst_id=$(docker create \
  --name="mock-dst-${run_id}" \
  --network=host \
  --env RUST_LOG="linkerd=debug,warn" \
  "${MOCK_DST_IMAGE}" "server.test.example.com:80=127.0.0.1:8123")
echo "mock-dst-${run_id}\t$mock_dst_id"

server_id=$(docker create \
  --name="server-${run_id}" \
  --network=host \
  "${FORTIO_IMAGE}" server -http-port 8123)
echo "server-${run_id}\t$server_id"

proxy_id=$(docker create \
  --name="proxy-${run_id}" \
  --network="host" \
  --env LINKERD2_PROXY_MOCK_ORIG_DST="127.0.0.1:8123" \
  --env LINKERD2_PROXY_DESTINATION_SVC_ADDR="127.0.0.1:8086" \
  --env LINKERD2_PROXY_DESTINATION_GET_SUFFIXES="test.example.com." \
  --env LINKERD2_PROXY_DESTINATION_PROFILE_SUFFIXES="test.example.com." \
  "${PROXY_IMAGE}")
echo "proxy-${run_id}\t$proxy_id"

mkdir -p target/reports
client_id=$(docker create \
  --name="client-${run_id}" \
  --network=host \
  -v "$PWD/target/reports:/reports" \
  "${FORTIO_IMAGE}" load \
      -json "/reports/${run_id}.json" \
      -n 1000000 \
      -qps 10000 \
      -resolve localhost \
      -H "Host: server.test.example.com" \
      "http://127.0.0.1:4140")
echo "client-${run_id}\t$client_id"

for id in "$mock_dst_id" "$proxy_id" "$server_id" ; do
  docker start "$id" >/dev/null
done

if ! docker start -a "$client_id" ; then
  echo "client failed" >&2
  echo "" >&2
  docker logs "$mock_dst_id" 2>&1 |sed -e "s/^/mock-dst\t/"
  docker logs "$proxy_id"    2>&1 |sed -e "s/^/proxy\t/"
fi

for id in "$client_id" "$server_id" "$proxy_id" "$mock_dst_id" ; do
  docker stop "$id" >/dev/null
done

sleep 1

for id in "$client_id" "$server_id" "$proxy_id" "$mock_dst_id" ; do
  docker rm -f "$id" >/dev/null
done
