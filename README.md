# Subspace Chain Alerts (PoC)

Important event alerts for Subspace blockchains.

## Status: Proof of Concept

- This repository is in a PoC state. Interfaces, configuration, and behavior may change without notice.
- Hardcoded settings are used for speed of iteration (Slack channel, WebSocket URL, thresholds, etc.).

## What it does currently

- Connects to a Subspace node over WebSocket (`ws://127.0.0.1:9944`).
- Subscribes to notifications for best blocks (and all blocks for fork detection).
- Keeps runtime metadata up to date via a background updater.
- Detects block gaps and forks, filling in missing blocks as needed.
- After filling in gaps and rationalising forks, runs alert detection on the best fork (or all forks).
- Posts applicable alerts to a Slack channel.

### How fork detection works

Blocks can be received in any order, and block notifications can be skipped, particularly during bulk syncing.
When a block is received from the best or all/any blocks subscription:

1. it is checked to see if it is the best block (only needed for the "all blocks" subscription)
2. the fork monitor connects it to the existing chain, fetching missing parent blocks  needed
  a. if there are too many missing blocks, it is treated as a disconnected new fork
3. new and missing blocks on the best fork are checked for alerts
  <!-- TODO: a. some alerts also check side forks -->

### Core block operations

A new block can:

- start a new chain (the first block always starts a new chain)
- extend an existing chain tip
- fork from a block behind a chain tip

A reorg happens when:

- a best block forks behind any chain tip
- a best block extends a chain tip, and the previous best block was on a different fork
  - if blocks are skipped, the previous best block can be an ancestor of the new best block, but not its parent

### Which blocks are checked for alerts?

All blocks on a best fork are checked for alerts, including blocks missed by subscriptions.
If there is a reorg, all unchecked blocks on the new best fork are checked for alerts.

For example:

Here is a typical chain fork:

```text
A - F - C1 - D1
      \ C2 - D2 - E2
```

If the local node receives all the blocks up to D1 first, then all the blocks up to E2, it will reorg from D1 to E2.
The chain fork monitor will check the blocks in this order:
`A - F - C1 - D1` then `C2 - D2 - E2`.
Some alerts check against the parent block, which is provided for each listed block.
Other alerts check a larger context, and need to manage it carefully during reorgs.

It is possible (but unlikely) for the chain to reorg from a higher to a lower height,
if the solution range after an era transition is significantly different on the two forks.

For details of the fork choice algorithm, see [the subspace protocol specification](https://subspace.github.io/protocol-specs/docs/decex/workflow#fork-choice-rule).

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
    - `transfer.rs`: amount transfer and address alerts
  - `subspace.rs`: Uses `subxt` and `scale-value` for chain interaction
  - `slack.rs`: Uses `slack-morphism` to send messages to Slack
  - `main.rs`: main process logic and run loop
    - Depends on `subspace-process` for process handling utility functions

### References

- Subspace Protocol reference implementation (node): [autonomys/subspace](https://github.com/autonomys/subspace)
- Tracking discussion for alerting PoC scope and follow-ups: [Issue #3](https://github.com/autonomys/subspace-chain-alerts/issues/3)
