#!/bin/bash
set -o nounset
set -o pipefail

PROFDIR=$(dirname "$0")

cd "$PROFDIR"

source "profiling-util.sh"

if [[ "$OSTYPE" == "darwin"* ]]; then
  echo "Warning: `perf` may not work properly in Docker for Mac." 2>&1
else
  PARANOIA=$(cat /proc/sys/kernel/perf_event_paranoid)
  if [ "$PARANOIA" -eq "-1" ]; then
    echo "you may not have permission to collect stats" "Consider tweaking /proc/sys/kernel/perf_event_paranoid:
 -1 - Not paranoid at all
  0 - Disallow raw tracepoint access for unpriv
  1 - Disallow cpu events for unpriv
  2 - Disallow kernel profiling for unpriv

You can adjust this value with:
  echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid

Current value is $PARANOIA" 2>&1
    exit 1
  fi
fi

# Cleanup background processes when script is canceled
trap '{ teardown; }' EXIT

if [ "$TCP" -eq "1" ]; then
  MODE=TCP DIRECTION=out run_tcp_benchmark
  MODE=TCP DIRECTION=in run_tcp_benchmark
fi
if [ "$HTTP" -eq "1" ]; then
  export MODE=HTTP
  DIRECTION=out run_fortio_benchmark
  DIRECTION=in run_fortio_benchmark
fi
if [ "$GRPC" -eq "1" ]; then
  export MODE=gRPC
  DIRECTION=out run_fortio_benchmark
  DIRECTION=in run_fortio_benchmark
fi

teardown
