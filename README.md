## Subspace Chain Alerts (PoC)

Important event alerts for Subspace blockchains.

### Status: Proof of Concept

- This repository is in a PoC state. Interfaces, configuration, and behavior may change without notice.
- Hardcoded settings are used for speed of iteration (Slack channel, WebSocket URL, thresholds, etc.).

### What it does currently

- Connects to a local Subspace node over WebSocket (`ws://127.0.0.1:9944`).
- Keeps runtime metadata up to date via a background updater.
- Posts Slack messages to a test channel when:
  - The process launches and connects to the node (first observed block).
  - Any `Sudo::*` extrinsic is included in a best block.
  - Any `Balances::force*` extrinsic is included.
  - Any `Balances::*` extrinsic whose transfer value exceeds a high threshold.
- Slack messages include block height, time, hash, and the chain genesis hash, plus a truncated view of extrinsic bytes and decoded fields for quick triage.
- Rate-limited Slack client with generous retries to avoid message loss on transient failures.

### Current defaults and hardcoded values

- Slack OAuth secret file path: `slack-secret` (file-based secret, not env vars).
- Slack Workspace (team) ID: `T03LJ85UR5G` (Autonomys workspace).
- Slack channel: `chain-alerts-test` (resolved to channel ID at startup).
- Bot name and icon: "Teor's Chain Alerts Tester" and `:flag-au:`.
- Node WebSocket: `ws://127.0.0.1:9944`.
- Amount unit: `AI3` (1 AI3 = 10^18 planck-like units).
- Alert threshold for large balance changes: `1_000_000 AI3`.
- Slack API retries: 30; request throttle: 2s; channel list page limit: 1000.

### Security notes

- The Slack OAuth token is loaded from a file and must be readable only by the current user.
  - On Unix, the process enforces `0400` or `0600` permissions. Other modes cause a panic at startup.
- The token is wrapped and zeroized on drop to reduce in-memory exposure.
- Do not commit the token; keep it out of version control and CI logs.

### Limitations (PoC)

- Hardcoded Slack channel, workspace ID, bot name/icon, node URL, and thresholds.
- Best blocks subscription only; no finalized block selection logic yet.
- Minimal decoding/validation for some extrinsics; fields are parsed best-effort.
- No stateful aggregation (e.g., summing multiple related transfers) or deduplication.
- No CLI/config file/env var configuration surface yet.
- No persistent storage, no metrics, no dashboards.
- Basic error handling: logs warnings on transient decode/metadata mismatches and continues.

### Getting started

1. Prerequisites

   - Rust (edition 2024 compatible)
   - A Slack bot token with permission to post to the target channel in the Autonomys workspace
   - A running Subspace node (local dev is fine)

2. Prepare Slack secret file

   - Save the bot token to a file named `slack-secret` in the repository root
   - Restrict permissions (Unix):
     - `chmod 400 slack-secret` (or `chmod 600 slack-secret`)

3. Run a local Subspace node

   - Follow the Subspace monorepo docs to build/run a local node, or run a dev node suitable for testing. See the Subspace reference implementation for details: [Subspace monorepo](https://github.com/autonomys/subspace).

4. Build and run
   - `cargo run -p subspace-chain-alerter`
   - On first observed block, you should see a Slack message in `#chain-alerts-test` summarizing connection and block info.

### Project structure

- Workspace member: `chain-alerter/`
  - `src/main.rs`: main process logic
  - Depends on Subspace crates (e.g., `subspace-process`) and `subxt` for chain interaction
  - Uses `slack-morphism` with `hyper-rustls` and `aws-lc` crypto provider

### References

- Subspace Protocol reference implementation (node/farmer/gateway): [autonomys/subspace](https://github.com/autonomys/subspace)
- Tracking discussion for alerting PoC scope and follow-ups: [Issue #3](https://github.com/autonomys/subspace-chain-alerts/issues/3)
