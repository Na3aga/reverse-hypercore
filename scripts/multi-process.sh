#!/usr/bin/env bash
# One validator per process, on this machine: four `hc-validator` processes,
# each with its own consensus validator, mempool, executor, state, block log
# and API, talking over authenticated p2p (consensus and transaction gossip).
#
#   scripts/multi-process.sh start [n]   # new chain with n validators (default 4)
#   scripts/multi-process.sh resume      # restart all nodes, keeping their chains (a node
#                                        # that was behind first downloads the blocks it missed)
#   scripts/multi-process.sh check       # do all nodes have the same state hash?
#   scripts/multi-process.sh kill <i>    # stop one node: the rest keep finalizing (n >= 3f + 1)
#   scripts/multi-process.sh revive <i>  # restart it while the others run: it catches up from
#                                        # its peers and rejoins from the last block's certificate
#   scripts/multi-process.sh stop
#
# Node i: API http://127.0.0.1:$((API_BASE + i)), p2p 127.0.0.1:$((P2P_BASE + i)),
# files in run/multi/. Node 0 also runs the simulated market.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=./target/release/hc-validator
DIR=run/multi
API_BASE=${API_BASE:-8180}
P2P_BASE=${P2P_BASE:-9100}
SIM=${SIM:-200}
SNAPSHOT=${SNAPSHOT:-2000}   # blocks between state snapshots (each node keeps only the log since its last one)

n_nodes() { cat "$DIR/n" 2>/dev/null || echo 4; }
addrs() { local n=$1 base=$2 out=""; for ((i = 0; i < n; i++)); do out+="${out:+,}127.0.0.1:$((base + i))"; done; echo "$out"; }

launch() { # index, extra args
  local i=$1; shift
  local n; n=$(n_nodes)
  local sim=0; [[ $i == 0 ]] && sim=$SIM
  nohup "$BIN" run --index "$i" --genesis "$DIR/genesis.json" --peers "$(addrs "$n" $P2P_BASE)" --peer-apis "$(addrs "$n" $API_BASE)" \
    --api "127.0.0.1:$((API_BASE + i))" --log "$DIR/blocks-$i.jsonl" --sim-trades-per-block "$sim" --snapshot-blocks "$SNAPSHOT" "$@" \
    >"$DIR/node-$i.log" 2>&1 &
  echo $! >"$DIR/node-$i.pid"
  echo "node $i: pid $!, API http://127.0.0.1:$((API_BASE + i)), log $DIR/node-$i.log"
}

stop_node() {
  local f="$DIR/node-$1.pid"
  [[ -f $f ]] || return 0
  kill "$(cat "$f")" 2>/dev/null || true
  for _ in $(seq 1 40); do kill -0 "$(cat "$f")" 2>/dev/null || break; sleep 0.25; done
  rm -f "$f"
}

state_hash() { # api port, [height]
  local body="{\"type\":\"stateHash\"${2:+,\"height\":$2}}"
  curl -s "http://127.0.0.1:$1/info" -H 'content-type: application/json' -d "$body"
}

case "${1:-}" in
  start)
    n=${2:-4}
    for f in "$DIR"/node-*.pid; do [[ -e $f ]] && stop_node "$(basename "$f" .pid | cut -d- -f2)"; done
    rm -rf "$DIR" && mkdir -p "$DIR" && echo "$n" >"$DIR/n"
    cargo build --release -q -p hc-node --bin hc-validator
    "$BIN" genesis --out "$DIR/genesis.json" --validators "$n" 2>/dev/null
    for ((i = 0; i < n; i++)); do launch "$i"; done
    ;;
  resume)
    n=$(n_nodes)
    for ((i = 0; i < n; i++)); do stop_node "$i"; done
    for ((i = 0; i < n; i++)); do launch "$i" --resume; done
    ;;
  kill) stop_node "$2"; echo "node $2 stopped" ;;
  revive) launch "$2" --resume --rejoin ;;
  stop) for ((i = 0; i < $(n_nodes); i++)); do stop_node "$i"; done; echo stopped ;;
  check)
    n=$(n_nodes)
    # The lowest latest height across running nodes, then every node's hash there.
    min=""
    for ((i = 0; i < n; i++)); do
      h=$(state_hash $((API_BASE + i)) | python3 -c 'import json,sys; print(json.load(sys.stdin).get("latest") or "")' 2>/dev/null || true)
      [[ -n $h ]] && { [[ -z $min || $h -lt $min ]] && min=$h; printf 'node %d at height %s\n' "$i" "$h"; } || printf 'node %d not answering\n' "$i"
    done
    [[ -n $min ]] || { echo "no node answering"; exit 1; }
    hashes=()
    for ((i = 0; i < n; i++)); do
      x=$(state_hash $((API_BASE + i)) "$min" | python3 -c 'import json,sys; print(json.load(sys.stdin)["stateHash"])' 2>/dev/null || true)
      [[ -n $x ]] && { hashes+=("$x"); echo "node $i state hash at $min: $x"; }
    done
    if [[ $(printf '%s\n' "${hashes[@]}" | sort -u | wc -l) -eq 1 ]]; then echo "all ${#hashes[@]} answering nodes agree at height $min"; else echo "NODES DISAGREE at height $min"; exit 1; fi
    ;;
  *) sed -n '2,18p' "$0"; exit 1 ;;
esac
