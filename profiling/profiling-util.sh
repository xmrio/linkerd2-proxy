# Configuration env vars and their defaults:
HIDE="${HIDE-1}"
export ITERATIONS="${ITERATIONS-1}"
export DURATION="${DURATION-10s}"
export CONNECTIONS="${CONNECTIONS-4}"
export GRPC_STREAMS="${GRPC_STREAMS-4}"
export HTTP_RPS="${HTTP_RPS-4000 7000}"
export GRPC_RPS="${GRPC_RPS-4000 6000}"
export REQ_BODY_LEN="${REQ_BODY_LEN-10 200}"
export TCP="${TCP-1}"
export HTTP="${HTTP-1}"
export GRPC="${GRPC-1}"

export PROXY_PORT_OUTBOUND=4140
export PROXY_PORT_INBOUND=4143

export ID=$(date +"%Y%h%d_%Hh%Mm%Ss")

if [ "$HIDE" -eq "1" ]; then
  export LOG=/dev/null
else
  export LOG=/dev/stdout
fi

BRANCH_NAME=$(git symbolic-ref -q HEAD)
BRANCH_NAME=${BRANCH_NAME##refs/heads/}
BRANCH_NAME=${BRANCH_NAME:-HEAD}
BRANCH_NAME=$(echo $BRANCH_NAME | sed -e 's/\//-/g')
GIT_REF=$(git rev-parse --short HEAD)
export RUN_NAME="$BRANCH_NAME $ID Iter: $ITERATIONS Dur: $DURATION Conns: $CONNECTIONS Streams: $GRPC_STREAMS"

run_tcp_benchmark() {
  SERVER="iperf" docker-compose up -d &> "$LOG"
  IPERF=$( ( (docker-compose exec iperf \
      linkerd-await \
      --uri="http://proxy:4191/ready" \
      -- \
      iperf -t 6 -p "$PROXY_PORT" -c proxy) | tee "$OUT_DIR/$NAME.txt") 2>&1 | tee "$LOG")

  if [[ $? -ne 0 ]]; then
    err "iperf failed:" "$IPERF"
    exit 1
  fi
}

run_fortio_benchmark () {
  docker-compose up -d &> "$LOG"
  docker-compose kill iperf &> "$LOG"

  RPS="$RPS"
  GRPC=""
  if [ "$MODE" = "gRPC" ]; then,
    GRPC="-grpc -s $GRPC_STREAMS"
  fi

  case "$DIRECTION" in
  "in")
    PROXY_PORT=$PROXY_PORT_INBOUND
    ;;
  "out")
    PROXY_PORT=$PROXY_PORT_OUTBOUND
    ;;
  *)
    echo "Invalid DIRECTION=$DIRECTION" >&2
    exit 1;
  esac

  FORTIO=$( (docker-compose exec fortio \
    linkerd-await \
    --uri="http://proxy:4191/ready" \
    -- \
    fortio load \
    ${GRPC} \
    -c="$CONNECTIONS" \
    -t="$DURATION" \
    -keepalive=${KEEPALIVE:-false} \
    -payload-size="$PAYLOAD_SIZE" \
    -qps="$RPS" \
    -labels="direction=${DIRECTION},proto=$MODE" \
    -json="out/${NAME}_$r-rps.json" \
    -H "Host: transparency.test.svc.cluster.local" \
    -resolve localhost \
    "http://127.0.0.1:${PROXY_PORT}") 2>&1 | tee "$LOG")

  if [[ $? -ne 0 ]]; then
      err "fortio failed:" "$FORTIO"
      exit 1
  fi

  docker-compose exec proxy bash -c 'echo F | netcat 127.0.0.1 7777'
  if [ "$(docker wait profiling_proxy_1)" -gt 0 ]; then
    err "proxy failed! log output:" "$(docker logs profiling_proxy_1 2>&1)"
    exit 1
  fi
}

teardown() {
  (docker-compose down -t 5 > "$LOG") || err "teardown failed"
}
