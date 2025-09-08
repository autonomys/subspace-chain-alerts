# Subspace Chain Alerts (PoC)

Important event alerts for Subspace blockchains.

## Status: Proof of Concept

- This repository is in a PoC state. Interfaces, configuration, and behavior may change without notice.
- Hardcoded settings are used for speed of iteration (Slack channel, WebSocket URL, thresholds, etc.).

## What it does currently

- Connects to a local Subspace node over WebSocket (`ws://127.0.0.1:9944`).
- Keeps runtime metadata up to date via a background updater.
- Posts applicable alerts to a Slack channel.

## Security notes

- The Slack OAuth token is loaded from a file and must be readable only by the current user.
  - On Unix, the process enforces `0400` or `0600` permissions. Other modes cause a panic at startup.
- The token is wrapped and zeroized on drop to reduce in-memory exposure.
- Do not commit or log the token. The `.gitignore` and `Debug` impl handle this by default.

## Limitations (PoC)

- Hardcoded Slack channel, workspace ID, and thresholds.
- Minimal decoding/validation for extrinsics and events; fields are parsed best-effort.
- Minimal stateful aggregation (e.g., summing multiple related transfers)
- No alert deduplication when multiple instances are running.
- Limited CLI configuration.
- No persistent storage, no metrics, no dashboards.
- Basic error handling: logs warnings on transient decode/metadata mismatches and continues.
- Limited testing, some tests are still manual.

## Getting started

1. Prerequisites

   - Rust (edition 2024 compatible)
   - A Slack bot token with permission to post to the target channel in the Autonomys workspace
   - A running Subspace node (local dev is fine)

2. Prepare Slack secret file

   - Save the bot token to a file named `slack-secret` in the repository root
   - Restrict permissions (Unix):
     - `chmod 400 slack-secret` (or `chmod 600 slack-secret`)

3. Optional: Run a local Subspace node

   - Follow the Subspace monorepo docs to build/run a local node, or run a dev node suitable for testing.
     See the Subspace reference implementation for details: [Subspace monorepo](https://github.com/autonomys/subspace).

4. Build and run
   - `cargo run -- --name "My Test Bot" --icon "warning" --node-rpc-url wss://rpc.mainnet.subspace.foundation/ws`
     - public node URLs are [listed in subspace.rs](https://github.com/autonomys/subspace-chain-alerts/blob/ac33ed7d200a1fdc3b92c1919f7b9cfacfba37c6/chain-alerter/src/subspace.rs#L43-L49)
   - On first observed block, you should see a Slack message in `#chain-alerts-test` summarizing connection and block info.
   - All arguments are optional. The default node is localhost, and the default icon is the instance external IP address country flag.
   - `RUST_LOG` can be used to filter logs, see:
     <https://docs.rs/tracing-subscriber/0.3.19/tracing_subscriber/filter/struct.EnvFilter.html#directives>

## Project structure

- Crate `chain-alerter`
  - `chain_fork_monitor.rs`: reassembles received blocks into a coherent set of forks
  - `alerts.rs`: checks for alerts in each block and extrinsic
    - `farming_monitor.rs`: unique farmer vote count monitoring
    - `slot_time_monitor.rs`: slot production rate monitoring for slot alerts
  - `subspace.rs`: Uses `subxt` and `scale-value` for chain interaction
  - `slack.rs`: Uses `slack-morphism` to send messages to Slack
  - `main.rs`: main process logic and run loop
    - Depends on `subspace-process` for process handling utility functions

### References

- Subspace Protocol reference implementation (node): [autonomys/subspace](https://github.com/autonomys/subspace)
- Tracking discussion for alerting PoC scope and follow-ups: [Issue #3](https://github.com/autonomys/subspace-chain-alerts/issues/3)
