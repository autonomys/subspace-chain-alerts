#!/usr/bin/env bash

# Check that the chain-alerter binary starts up and connects to a public RPC node.
# This is a sanity check to ensure that the binary is working.

# http://redsymbol.net/articles/unofficial-bash-strict-mode/
set -euo pipefail
set -x

if [[ "$#" -gt 1 ]] || [[ "${1:-}" == "-h" ]] || [[ "${1:-}" == "--help" ]]; then
  echo "Usage: $0 [NODE_URL]"
  echo "Or set NODE_URL in the environment to the node's RPC URL"
  exit 1
fi

if [[ "$#" -ge 1 ]]; then
    echo "Using node from command line"
    NODE_URL="$1"
elif [[ -n "${NODE_URL:-}" ]]; then
    echo "Using node from environment"
elif [[ $(($RANDOM % 2)) == 0 ]]; then
    echo "Using foundation node"
    NODE_URL=wss://rpc.mainnet.subspace.foundation/ws
else
    echo "Using labs node"
    NODE_URL=wss://rpc-0.mainnet.autonomys.xyz/ws
fi

cargo run --locked -- --slack=false --test-startup --node-rpc-url="$NODE_URL" \
    | rg --fixed-strings --passthru "**Launched and connected to the node**"

# Stop showing executed commands
set +x

echo
echo "=============================================================="
echo "Success: chain-alerter started and loaded blocks from the node"
echo "=============================================================="
echo
