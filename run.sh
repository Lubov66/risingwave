#!/usr/bin/env bash

set -euo pipefail

f() {
  psql -h localhost -p 4566 -d dev -U root -f "$@"
}

./risedev k
./risedev clean-data
RUST_LOG="risingwave_stream=trace" ./risedev d full

f queries.sql </dev/null

sleep 100

echo -n "number of delete_ranges: "
cat .risingwave/log/compute-node*.log | rg "state_table: delete range" | wc -l

echo "^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"
echo "After implementing dynamic-filter cache, this number should be 0."
