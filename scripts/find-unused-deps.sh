#!/usr/bin/env bash

# http://redsymbol.net/articles/unofficial-bash-strict-mode/
set -euo pipefail

echo "Current directory: $(pwd)"
if [[ ! -d "./chain-alerter" ]]; then
  echo "Changing to the root of the repository:"
  cd "$(dirname "$0")/.."
  echo "Current directory: $(pwd)"
  if [[ ! -d "./chain-alerter" ]]; then
    echo "Missing ./chain-alerter directory"
    echo "This script must be run from the base of an autonomys/subspace-chain-alerts repository checkout"
    exit 1
  fi
fi

# Show commands before executing them
set -x

echo "Checking for unused dependencies..."
cargo udeps --workspace --all-features --all-targets --locked

# Stop showing executed commands
set +x

echo
echo "============================================"
echo "Successfully checked for unused dependencies"
echo "============================================"
echo