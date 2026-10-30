#!/usr/bin/env bash
# End-to-end demo of the simplified Hyperliquid flow on a local devnet:
#   core trading (order book) -> bridge core->EVM -> EVM transfer -> contract
#   deploy -> contract trades on the core book via CoreWriter -> bridge EVM->core
#   -> supply check -> replay of the block log reproduces every state hash.
#
# Prerequisite: `cargo run --release --bin hc-devnet` running (API on 127.0.0.1:8080).
set -euo pipefail
cd "$(dirname "$0")/.."

API=${API:-http://127.0.0.1:8080}
RPC="$API/evm"
CLI=./target/release/hc-cli
BRIDGE=0x2222222222222222222222222222222222222222

info() { curl -s "$API/info" -H 'content-type: application/json' -d "$1"; }
step() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
show() { python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps($1, indent=1))"; }
core_summary() { info "{\"type\":\"clearinghouseState\",\"user\":\"$1\"}" | show "{'accountValue': d['crossMarginSummary']['accountValue'], 'rawUsd': d['crossMarginSummary']['totalRawUsd'], 'positions': [(p['position']['coin'], p['position']['szi'], p['position']['entryPx']) for p in d['assetPositions']]}"; }
evm_balance() { cast from-wei "$(cast balance "$1" --rpc-url "$RPC")"; }
# Whole-dollar BTC price at mark * factor (prices follow the market, so the
# demo works on a quiet chain and with the market simulator running).
btc_px() { info '{"type":"metaAndAssetCtxs"}' | python3 -c "import json,sys; print(round(float(json.load(sys.stdin)[1][0]['markPx']) * $1))"; }

key() { $CLI --user "$1" key | python3 -c "import json,sys; print(json.load(sys.stdin)['$2'])"; }
ALICE=$(key alice address); ALICE_PK=$(key alice privateKey)
BOB=$(key bob address)
CAROL=$(key carol address); CAROL_PK=$(key carol privateKey)
echo "alice=$ALICE bob=$BOB carol=$CAROL"

step "1. Core order book: alice quotes, bob takes (latency measured end-to-end, submit -> finalized -> executed)"
echo "(with the market simulator on, bob's market order also fills against simulated makers)"
t0=$(ms); $CLI --user alice order BTC sell "$(btc_px 1.0003)" 0.5 --tif gtc | show "d['response']['data']['statuses']"; t1=$(ms)
$CLI --user alice order BTC sell "$(btc_px 1.0006)" 0.5 --tif alo | show "d['response']['data']['statuses']"
t2=$(ms); $CLI --user bob market BTC buy 0.7 | show "d['response']['data']['statuses']"; t3=$(ms)
echo "latency: limit order $((t1 - t0)) ms, market order $((t3 - t2)) ms (includes CLI process start + WS connect)"
echo "bob:";   core_summary "$BOB"
echo "alice:"; core_summary "$ALICE"

step "2. Bridge 5000 USDC alice core -> alice EVM (same address, same key)"
echo "alice EVM balance before: $(evm_balance "$ALICE")"
$CLI --user alice send-to-evm 5000 | show "d"
echo "alice EVM balance after:  $(evm_balance "$ALICE") USDC"

step "3. Plain EVM transfer with cast: alice -> carol 1000 USDC"
cast send "$CAROL" --value 1000ether --private-key "$ALICE_PK" --rpc-url "$RPC" --gas-price 0 --legacy | grep -E "status|transactionHash|blockNumber"
echo "carol EVM balance: $(evm_balance "$CAROL") USDC"

step "4. Deploy PerpBot (reads core mark price via precompile 0x...0806, trades via CoreWriter 0x3333...3333)"
BOT=$(cd crates/hc-evm/contracts && forge create PerpBot.sol:PerpBot --private-key "$ALICE_PK" --rpc-url "$RPC" --gas-price 0 --legacy --broadcast | awk '/Deployed to:/ {print $3}')
[ -n "$BOT" ] || { echo "deploy failed"; exit 1; }
echo "PerpBot deployed at $BOT"
echo "bot.mark(0) = $(cast call "$BOT" 'mark(uint32)(uint64)' 0 --rpc-url "$RPC") (µUSD; precompile reading core state)"

step "5. Fund the bot's core account: bot.withdrawToCore{value: 2000 USDC} -> bridge -> bot core balance"
cast send "$BOT" 'withdrawToCore()' --value 2000ether --private-key "$ALICE_PK" --rpc-url "$RPC" --gas-price 0 --legacy | grep -E "^status"
echo "bot core:"; core_summary "$BOT"

BOT_PX=$(btc_px 1.01)
step "6. Contract buys on the core book: 0.01 BTC, limit $BOT_PX (mark + 1%), GTC via CoreWriter action 1; it crosses the best ask"
cast send "$BOT" 'placeLimit(uint32,bool,uint64,uint64,uint8)' 0 true "${BOT_PX}00000000" 1000000 2 --private-key "$ALICE_PK" --rpc-url "$RPC" --gas-price 0 --legacy | grep -E "^status"
echo "CoreWriter orders run on core a few seconds after their EVM block; waiting 4 s"
sleep 4
info "{\"type\":\"explorerAddress\",\"user\":\"$BOT\"}" | show "[(a['kind'], a['detail'].get('dir') or a['detail']) for a in d['activity'][:3]]"
echo "bot core:"; core_summary "$BOT"

step "7. Bridge back EVM -> core: carol sends 500 USDC to the bridge address"
echo "carol core before:"; core_summary "$CAROL"
cast send "$BRIDGE" --value 500ether --private-key "$CAROL_PK" --rpc-url "$RPC" --gas-price 0 --legacy | grep -E "^status"
echo "carol core after:";  core_summary "$CAROL"
echo "carol EVM balance: $(evm_balance "$CAROL") USDC"

step "8. Supply check: core balances - position cost basis + EVM = minted, to the µUSDC"
info '{"type":"supply"}' | show "d"

step "9. On-chain activity (explorer data) for alice"
info "{\"type\":\"explorerAddress\",\"user\":\"$ALICE\"}" | show "{k: sum(1 for a in d['activity'] if a['kind'] == k) for k in sorted({a['kind'] for a in d['activity']})}"
echo "Trading UI: $API/   Explorer: $API/explorer#/address/$ALICE   (bot: $API/explorer#/address/$BOT)"

step "10. Replay the finalized block log (from genesis, or from the last state snapshot) and compare every state hash"
LOG=${LOG:-devnet-blocks.jsonl}
# Copy the snapshot first: a rotation in between would leave a log without its snapshot.
cp "$LOG.snapshot" /tmp/hc-demo-replay.jsonl.snapshot 2>/dev/null || rm -f /tmp/hc-demo-replay.jsonl.snapshot
head -n "$(( $(wc -l < "$LOG") - 1 ))" "$LOG" > /tmp/hc-demo-replay.jsonl
./target/release/hc-replay /tmp/hc-demo-replay.jsonl
info '{"type":"exchangeStatus"}' | show "d"
