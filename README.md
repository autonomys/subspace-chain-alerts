# Subspace Chain Alerts

Important event alerts for Subspace blockchains.

## Quick Start

To get up and running quickly:

- Save the Slack token to a file named `slack-secret`
- Restrict permissions (Unix): `chmod 400 slack-secret`
- `docker run --mount type=bind,source=/path/to/slack-secret,target=/slack-secret,readonly ghcr.io/autonomys/chain-alerter [--production]`

## Status: Proof of Concept

- This repository is a minimum viable product.
- Interfaces, configuration, and behavior may change without notice.
- Hardcoded settings are used for speed of iteration (Slack channel, wallet addresses, thresholds, etc.).

## What it does currently

- Connects to Subspace nodes over WebSocket (`ws://127.0.0.1:9944` by default).
- Subscribes to notifications for best blocks (and all blocks across all nodes for fork detection).
- Keeps runtime metadata up to date for each node via a background updater.
- Detects block gaps and forks, filling in missing blocks as needed.
- After filling in gaps and rationalising forks, runs alert detection on the best fork.
- Posts applicable alerts to a Slack channel, with links to [subscan.io](https://autonomys.subscan.io)

### Known issues

- [Subscan.io](https://autonomys.subscan.io/block) shows blocks as "finalized" when they are 6 blocks behind the best tip. This is not the same as the Subspace network "finalized" status.
  - Block data should not be trusted until the block is shown as "finalized" on Subscan.
- Subscan only indexes the best chain, and [deletes blocks](https://github.com/subscan-explorer/subscan-issue-tracker/issues/156#issuecomment-3355554868)
  that are reorged away from.
  - Links to blocks that have been reorged away from will give a "No related data found" error on Subscan.
  - If "finalized" blocks are likely to have changed, the alerter will issue a reorg alert, and any open Subscan links should be refreshed.

### How fork detection works

Blocks can be received in any order, and block notifications can be skipped, particularly during bulk syncing.

When a block is received from the all blocks subscription on the primary node:

1. it is checked to see if it is the best block (only if it is a recent block)
  a. backwards reorgs are rare and unlikely due to the fork rules, so we can skip this check for most blocks

For all other subscriptions:

1. The block is either assumed to be the best block (best blocks subscription on the primary node) or assumed not to be (all blocks on secondary nodes)
  a. a separate block stall alerter is run for each connected node
2. the fork monitor connects the block to the existing chain, fetching missing parent blocks as needed
  a. if there are too many missing blocks, the block is treated as a disconnected new fork
3. new and missing blocks on the best fork are checked for alerts
  a. the side chain alerts also check for side forks on all connected nodes

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

## Alert De-duplication

### By Alert Kind

Alerts about the same extrinsic or event are deduplicated, using this priority order:

- sudo/sudid
- large balance transfer
- force balance transfer
- important address transfer
- important address (any other extrinsic or event)

Other alert kinds are not de-duplicated, because they are unlikely to report the same issue.

### Over Time

Some alerts are de-duplicated by only issuing an alert when the status changes:

- BlockStall/Resume
  - BlockReceiveResumed also takes priority over BlockChainTimeGap, because it contains the chain gap anyway
- SlotTime High/Low
- Farmer Increase/Decrease

### By Severity

Fork and side chain alerts are only issued at the threshold, and when the fork length is a multiple of 10 blocks.

### Between Servers

Alerts are de-duplicated between servers by connecting multiple servers to the same alerter instance.
Some alerts are only issued if they happen on the primary RPC server.

## Security notes

- The Slack OAuth token is loaded from a file and must be readable only by the current user.
  - On Unix, the process enforces `0400` or `0600` permissions. Other modes cause a panic at startup.
- The token is wrapped and zeroized on drop to reduce in-memory exposure.
- Do not commit or log the token. The `.gitignore` and `Debug` impl handle this by default.

## Limitations (PoC)

- Hardcoded Slack channel, workspace ID, and thresholds.
- Minimal decoding/validation for extrinsics and events; fields are parsed best-effort.
- Minimal stateful aggregation (e.g., summing multiple related transfers).
- Alerts are partly de-duplicated, but some duplicates might still happen (this is safer than accidentally ignoring important alerts).
- No alert deduplication between multiple alerter instances.
- Limited CLI configuration.
- No persistent storage, no metrics, no dashboards.
- Basic error handling: logs warnings on transient decode/metadata mismatches and continues.
- Limited testing, some tests are still manual.

## Getting started

1. Prerequisites

   - Rust (edition 2024 compatible)
   - A Slack bot token with permission to post to the target channel in the Autonomys workspace
   - A running Subspace node (a local non-archival node is fine)

2. Optional: Prepare Slack secret file

   - Save the Slack "bot token" to a file named `slack-secret` in the repository root
   - Restrict permissions (Unix):
     - `chmod 400 slack-secret` (or `chmod 600 slack-secret`)

3. Optional: Run a local Subspace node

   - This is the most efficient way to run an instance, because it gives low latency for the block subscriptions, best blocks checks, extrinsics, and events retrieval.
   - Follow the Subspace monorepo docs to build/run a local node, or run a dev node suitable for testing.
     See the Subspace reference implementation for details: [Subspace monorepo](https://github.com/autonomys/subspace).

4. Build and run
   - `cargo run -- --name "My Test Bot" --icon "warning" --node-rpc-url wss://rpc.mainnet.autonomys.xyz/ws`
     - to use multiple nodes, supply `--node-rpc-url` multiple times
     - public node URLs are [listed in subspace.rs](https://github.com/autonomys/subspace-chain-alerts/blob/ac33ed7d200a1fdc3b92c1919f7b9cfacfba37c6/chain-alerter/src/subspace.rs#L43-L49)
     - `--production` will sent alerts to the production channel (except for startup alerts, which always go to the test channel)
     - `--slack=false` will disable Slack message posting entirely, and just log alerts to the terminal.
   - Testing options:
     - `--alert-limit=5` will exit after 5 alerts have been posted, including the startup alert.
     - `--test-startup` will always exit after the startup alert, even if other alerts fired during the initial context load.
   - After context blocks are loaded, you should see a Slack message in `#chain-alerts-test` summarizing connection and block info.
   - All arguments are optional. The default node is `localhost`, and the default icon is the instance external IP address country flag (looked up via an online GeoIP service, which can be wrong).
   - `RUST_LOG` can be used to filter logs, see:
     <https://docs.rs/tracing-subscriber/0.3.19/tracing_subscriber/filter/struct.EnvFilter.html#directives>

## Project structure

- Crate `chain-alerter`
  - `chain_fork_monitor.rs`: reassembles received blocks into a coherent set of forks
  - `alerts.rs`: checks for alerts in each block and extrinsic
    - `accounts.rs`: address-based alerts and address context for all alerts
    - `subscan.rs`: links to Subscan.io for blocks, extrinsics, and events
    - `transfer.rs`: amount transfer alerts and amount context for all alerts
    - `farming_monitor.rs`: unique farmer vote count monitoring
    - `slot_time_monitor.rs`: slot production rate monitoring for slot alerts
  - `subspace.rs`: Uses `subxt` and `scale-value` for chain interaction
  - `slack.rs`: Uses `slack-morphism` to send messages to Slack
  - `main.rs`: main process logic and run loop
    - Depends on `subspace-process` for process handling utility functions
  - `../test_utils.rs`: test support for alerts and subsystems.
  - `../tests.rs`: tests for alerts and subsystems.

### References

- Subspace Protocol reference implementation (node): [autonomys/subspace](https://github.com/autonomys/subspace)
- Tracking discussion for alerting PoC scope and follow-ups: [Issue #3](https://github.com/autonomys/subspace-chain-alerts/issues/3)
