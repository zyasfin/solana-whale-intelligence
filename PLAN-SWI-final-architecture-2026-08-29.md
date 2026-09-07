# PLAN SWI — Final Architecture

**Canonical marker:** `PLAN SWI`  
**Product:** Signal Forge  
**Status:** FINAL PLAN / architecture freeze; amended by REV-020 + 2026-09-05 selective token/cabal tracking + 2026-09-06 Token/LP boundaries + 2026-09-07 domain-isolation hardening; implementation partial  
**Frozen at:** 2026-08-29T12:02:55Z  
**Amended at:** 2026-08-31 — Token Recent + Deployer/Social Reuse Intelligence; 2026-09-05 — Selective Token Intake + Cabal Wallet Tracking; 2026-09-06 — Token/LP Domain Boundaries; 2026-09-07 — SWI Mega Intelligence domain isolation  
**Canonical local artifact:** `/root/PLAN-SWI-final-architecture-2026-08-29.md` 

**Domain decision:** one SWI platform/repository/PostgreSQL authority; Token, Wallet/Cabal, and LP are bounded logical domains with separate queues, projections, scores, alerts, dashboards, and execution policies. Confluence composes point-in-time components only. Physical split requires measured cross-domain SLO, dependency/runtime, storage, independent-scaling, or execution trust-boundary evidence.

## 0. Executive decision

Signal Forge adalah platform intelligence dan bounded autonomous execution multi-chain.

```text
Intelligence Plane
├─ Token
├─ Wallet
├─ Caller
├─ Narrative + Name/Meme Provenance
├─ Token Birth + Family + Canonicality
├─ Token Recent + Deployer/Social Reuse
├─ Revival + Cabal/Funding Graph
├─ LP Pool + LP Wallet
├─ Strategy/Sandbox
├─ Portfolio/Risk
└─ Source Health + Decision/Outcome Ledger

Execution Plane
├─ Token Auto-Trader
└─ LP Autopilot
   ├─ Solana: Meteora DLMM
   └─ Robinhood Chain
      ├─ Uniswap V2/V3/V4
      └─ PancakeSwap V2/V3
```

Semua intelligence menjadi immutable point-in-time evidence bundle sebelum policy decision. Tidak ada scraper, vendor, caller, wallet tag, model, atau LLM yang boleh langsung memanggil signer.

## 1. Frozen scope

### Chains

| Chain | Token/wallet/caller/narrative | LP product/execution |
|---|---:|---:|
| Solana | Yes | Meteora DLMM |
| Robinhood Chain | Yes | Uniswap V2/V3/V4 + PancakeSwap V2/V3 |
| Ethereum | Yes | `N/A` |
| Base | Yes | `N/A` |
| BSC | Yes | `N/A` |

Ethereum/Base/BSC tetap membaca pair/liquidity sebagai token intelligence. Tidak ada LP position product untuk ketiga chain tersebut.

### Trading modes

```text
READ_ONLY
SHADOW
PAPER
CONFIRM_EACH
AUTO_BOUNDED
PAUSED
HALTED
```

MVP: research, shadow, paper.  
Future: bounded autonomous token trading + LP opening/management.

### LP semantics

`OPEN_POSITION` berarti membuka liquidity position pada existing verified pool.

```text
CREATE_POOL
CREATE_TOKEN
```

Keduanya capability terpisah, disabled, later. Tidak implicit dari LP automation.

### Entity thresholds/product focus

- Meme target awal: market cap `< $50M`.
- Holding/outcome horizon utama: `< 3 minggu`, dengan windows eksplisit hingga `+21d`.
- Contract address adalah token truth; ticker/name hanya discovery clue.
- Archived/dead token menjadi compact tombstone; tidak dihapus.

## 2. Architectural principles

1. **Evidence before inference.** Raw chain/first-party source menang atas aggregator.
2. **Point-in-time truth.** Semua keputusan memakai data yang tersedia pada timestamp keputusan.
3. **No opaque universal score.** Component gates, missingness, confidence, freshness terlihat.
4. **Capability mandatory, vendor replaceable.** Provider dapat diganti raw-chain/self-compute/fallback.
5. **Cheap first.** Broad cheap discovery, expensive enrichment hanya shortlist/trigger.
6. **Archive, not delete.** History dipertahankan untuk revival, audit, dan missed-runner review.
7. **Fail closed for execution.** Missing/stale mandatory data tidak dianggap aman.
8. **LLM proposes/explains only.** Deterministic policy dan signer tetap authority.
9. **No direct source-to-signer path.** Semua action melewati immutable intent.
10. **Reconciliation before retry.** Ambiguous submission tidak pernah blind retry.
11. **Manual assertions coexist.** Manual/system/vendor/inferred tags terpisah dan dapat conflict.
12. **Minimal deployables.** Modular monolith + workers + isolated signer; no premature microservices.
13. **No unlicensed code reuse.** Meridian/Kaiser concepts dipelajari; source code tidak disalin.

## 3. High-level architecture

```text
                         ┌──────────────────────────────┐
                         │ Browser / Operator Channel   │
                         │ Signal Forge dashboard       │
                         └──────────────┬───────────────┘
                                        │ OIDC + session + RBAC
                         ┌──────────────▼───────────────┐
                         │ Web/API Modular Monolith     │
                         │ query, cases, config, policy │
                         └───────┬─────────┬────────────┘
                                 │         │
                          SQL/outbox       │ privileged policy approval
                                 │         │
                     ┌───────────▼─────────▼───────────┐
                     │ PostgreSQL                     │
                     │ evidence/event graph           │
                     │ projections + jobs + policies  │
                     │ decisions + intents + audit    │
                     └───────────┬─────────┬───────────┘
                                 │         │
                claim jobs       │         │ LISTEN/NOTIFY + polling
             SKIP LOCKED         │         │
          ┌──────────────────────▼──┐   ┌──▼─────────────────────┐
          │ Intelligence Workers    │   │ Execution Workers       │
          │ chain/social/enrichment │   │ token + LP policy engine│
          │ lifecycle/outcomes      │   │ quote/sim/reconcile     │
          └───────────┬─────────────┘   └──────────┬─────────────┘
                      │                             │ canonical intent
          ┌───────────▼─────────────┐      ┌───────▼─────────────┐
          │ External Sources        │      │ Isolated Signer      │
          │ RPC/API/browser/MTProto │      │ policy + tx decoder  │
          └─────────────────────────┘      └───────┬─────────────┘
                                                  │
                                           Solana / EVM RPC
```

## 4. Deployment units

### 4.1 `signal-forge-web`

Satu modular monolith:

- Dashboard/API.
- OIDC callback + server-side sessions.
- RBAC/workspace enforcement.
- Entity/case/query APIs.
- Source/provider configuration.
- Strategy/policy preview and approval.
- Alert inbox and execution tape.
- No private key.
- No signing.

### 4.2 `signal-forge-worker`

Same codebase, role-specific worker command:

- Chain ingestion/backfill.
- Social/web ingestion.
- Normalization/projections.
- Token lifecycle.
- Narrative resolver.
- Wallet/caller/cabal analytics.
- Outcome measurement.
- Scheduler/job executor.
- Source-health probes.

Workers use DB leases and `FOR UPDATE SKIP LOCKED`. No process-local flag as durable lock.

### 4.3 `signal-forge-browser-worker`

Separate only because browser/session dependencies differ:

- Authorized X search/profile/list ingestion.
- Authorized TikTok search.
- Raw payload/HTML/media capture.
- Parser-version and challenge/session-health reporting.
- ASR/OCR only shortlisted candidates.
- No CAPTCHA bypass, mass account creation, or quota evasion.

### 4.4 `signal-forge-executor`

- Deterministic token/LP policy evaluation.
- Fresh quote/state fetch.
- Intent creation.
- Capital/exposure reservation.
- Simulation/call-static.
- Transaction construction or complete decode.
- Submit exactly once.
- Reconciliation.
- No raw private key unless this process is also the isolated signer; preferred split tetap signer terpisah.

### 4.5 `signal-forge-signer`

Isolated OS user/container/VM:

- Narrow private/Tailscale interface.
- Workload-authenticated requests only.
- Reads active signed policy projection.
- Rebuilds or fully decodes transaction.
- Rejects unknown programs/contracts/instructions/hooks.
- Applies amount, recipient, slippage, price-impact, fee, deadline, nonce, and exposure checks.
- Cannot access social/provider credentials.
- Key cannot be exported through API.

### 4.6 PostgreSQL

Authoritative store for:

- Event/evidence graph.
- Entity projections.
- Jobs/leases/outbox.
- Strategies/policies.
- Decision bundles.
- Intents/executions/reconciliation.
- Audit/RBAC/session metadata.

No Redis/Kafka/Neo4j in V1. PostgreSQL queue/outbox + `LISTEN/NOTIFY` cukup. Neo4j hanya jika measured graph-query gap muncul.

### 4.7 Blob evidence store

V1: content-addressed filesystem mounted read-only to consumers where possible.

Stores:

- Raw JSON/HTML.
- Screenshots/frame samples.
- Media/audio.
- OCR/ASR artifacts.
- Transaction simulations/decoded payloads.

Object key = content hash. Metadata and references live in PostgreSQL. Upgrade to S3/MinIO only when multi-node/storage needs justify it.

### 4.8 Optional local SQLite spool

Not required when PostgreSQL healthy. Allowed only for disconnected/edge collectors:

```text
journal_mode=WAL
synchronous=NORMAL
busy_timeout=15000
```

Spool is not product truth. It flushes idempotently into PostgreSQL.

## 5. Network and identity

```text
Default network      = Tailscale/private
Human authentication = Google OIDC Authorization Code + PKCE
Admission            = invite-only
Permanent identity   = issuer + subject
Authorization        = workspace RBAC + resource scope
Step-up              = passkey/WebAuthn
Worker authentication= workload identity
```

### Roles

| Role | Research | Tags/notes | Paper | Confirm trade | Activate auto | Signer policy/users |
|---|---:|---:|---:|---:|---:|---:|
| Viewer | Yes | No | No | No | No | No |
| Analyst | Yes | Yes | Yes | No | No | No |
| Trader | Yes | Yes | Yes | Yes | Approved policy only | No |
| Owner | Yes | Yes | Yes | Yes | Yes | Step-up required |

Security rules:

- No first-login-becomes-owner.
- Invite binds once to immutable OIDC identity.
- Email/domain alone bukan permanent allowlist.
- Opaque server-side session cookie: `HttpOnly`, `Secure`, `SameSite`.
- No OAuth token in `localStorage`.
- Exact redirect URI, state, nonce, PKCE, audience/signature validation.
- CSRF protection, session rotation, idle/absolute expiry.
- Multi-user rows carry `workspace_id`; PostgreSQL RLS as defense-in-depth.

## 6. Source and provider plane

### Trust tiers

```text
T0 raw chain / first-party protocol
T1 first-party social/source
T2 normalized indexer
T3 market/security aggregator
T4 local inference
```

Every observation stores:

```text
source_id
source_tier
source_event_id
occurred_at
first_seen_at
ingested_at
source_updated_at
raw_ref
parser_version
confidence
truth_status
freshness_status
```

### Provider pools

```text
Provider family
├─ legitimate user-owned credentials
│  ├─ quota/class token buckets
│  ├─ cooldown + Retry-After
│  ├─ circuit breaker
│  └─ usage/cost accounting
├─ endpoint mirrors
│  └─ weighted health/latency + chain-ID validation
└─ semantic fallbacks
   └─ stored as separate observations
```

Rules:

- No blind round-robin.
- No account/IP rotation for quota evasion.
- Auth/validation errors disable credential; request invalid tidak dicoba ke semua key.
- Public no-key API uses one host-level limiter/cache.
- Different vendors never silently overwrite each other.
- Source activation requires capability/cost preview + user approval.

### Primary source stack

#### Solana

- Helius: raw truth/history/stream.
- Birdeye: analytics/holders/wallet PnL, shortlist/on-demand where needed.
- Meteora Data API + SDK/on-chain: DLMM.
- Jupiter: quote/routing + metadata fallback.
- DEX Screener: lightweight market discovery.
- RugCheck: security enrichment.

#### EVM shared

- Paid RPC + independent failover.
- Raw logs/receipts/traces.
- Etherscan V2: Ethereum/Base/BSC.
- Blockscout/Robinscan: Robinhood.
- DEX Screener + GeckoTerminal.
- GoPlus; Honeypot only ETH/Base/BSC.
- Self-computed normalized wallet swaps/PnL.

#### Narrative/caller

- Telegram MTProto: caller truth.
- Self-hosted authorized X scraper: primary X.
- X API: disabled default, optional validation.
- TikTok authorized search: token-triggered cultural origin.
- RSS/site diff/GitHub: official catalyst.
- Neynar first; Snapchain canonical/fallback for Farcaster.

## 7. Ingestion and normalization

### 7.1 Canonical event envelope

```text
EventEnvelope
- event_id
- workspace_id nullable for global public data
- chain/platform
- event_type
- entity_keys[]
- source_id
- source_event_id
- occurred_at nullable
- observed_at
- ingested_at
- raw_hash/raw_ref
- parser_version
- payload_schema_version
- truth_status
- confidence
```

Idempotency key defaults:

```text
source_id + source_event_id + payload_schema_version
```

If source lacks stable ID:

```text
source_id + normalized entity + event type + time bucket + raw hash
```

### 7.2 Ingestion flow

```text
fetch/stream
→ raw evidence write
→ envelope validation
→ idempotent append
→ normalization
→ entity resolution
→ graph edges
→ scalar projections
→ trigger evaluation
→ jobs/outbox
```

Raw evidence is written before parser-derived claims where practical.

### 7.3 Source dependence

Two vendors repeating one upstream event are not two independent confirmations. `source_dependencies` records upstream/derived relationships. Confluence counts independent evidence separately from correlated copies.

## 8. Intelligence Plane domains

### 8.1 Token Intelligence

- Canonical chain-qualified contract.
- Metadata and authorities.
- Liquidity, MC/FDV, volume/trades.
- Holder distribution and exclusions.
- Security observations.
- Market/lifecycle status.
- Freshness/coverage.

### 8.2 Token Birth Lifecycle

```text
CREATED
PRE_GRADUATION
MIGRATED/GRADUATED
FIRST_LIQUIDITY
ACTIVE
COOLING
DORMANT
ARCHIVED
TOMBSTONED
```

Mint creation, launchpad creation, first pool, migration, and first meaningful liquidity remain separate timestamps.

Dormant/dead tokens are not individually polled. Global trade/new-pair feeds compare against compact dormant baselines; full enrichment only after wake gate.

### 8.3 Token Family and Canonicality

- Same name/symbol/image.
- Cross-chain derivatives.
- First deploy.
- First liquid.
- Current market leader.
- Official/derivative/copycat relation.
- Liquidity/attention rotation.

Contract address remains truth.

### 8.3.1 Token Recent + Deployer/Social Reuse Intelligence

Purpose: answer **“what changed recently around this token/project?”** across
explicitly linked contracts, deployment actors, and reused social identities.
This is a temporal evidence projection over existing token/family/wallet/social
graph data; it is not a symbol-based token merge and not an all-X firehose.

#### Identity anchors

```text
token truth        = chain_id + contract_address
wallet truth       = chain_id + wallet_address
X truth            = platform + immutable account/user ID
Telegram truth     = platform + chat/channel ID
website truth      = normalized registrable domain + observed ownership evidence
```

Ticker, display name, image, handle, and URL are discovery fingerprints only.
Factory/launchpad/program addresses must not be mislabeled as the human/project
deployer. Account handles and domains may change owner; historical bindings keep
`valid_from`, `valid_until`, and evidence.

#### Token-triggered method

```text
1. Resolve anchor token by chain + contract.
2. Extract deployment actors:
   deployer/creator, mint/update authority, fee payer, factory/launchpad,
   initial funder, and authority changes.
3. Extract social identities:
   X account ID/handle, Telegram chat ID, website/domain, GitHub/Farcaster.
4. Reverse lookup prior/new contracts linked to those actors or identities.
5. Search cross-chain candidates from metadata fingerprint and reverse indexes.
6. Corroborate every candidate; never merge by symbol/name alone.
7. Trigger targeted X/web/TikTok lookup for activated or operator-requested cases.
8. Normalize evidence into recent events; dedupe correlated provider/repost copies.
9. Materialize per-token and per-family chronological projections.
```

Activation is cheap-first: first meaningful liquidity, migration/graduation,
credible caller, smart-wallet entry, fresh-wallet burst, revival wake, material
volume/trade activation, social profile CA change, or explicit operator request.
Dormant/dead tokens remain event-wake only; no individual social polling.

#### Relationships

```text
SAME_DEPLOYER
SAME_AUTHORITY
SAME_FEE_PAYER
SAME_FUNDER
FUNDED_BY_KNOWN_DEPLOYER
SAME_SOCIAL_ACCOUNT
REUSED_SOCIAL_LINK
OFFICIAL_CA_ANNOUNCEMENT
CROSS_CHAIN_DEPLOYMENT
DERIVATIVE_OF
SUSPECTED_COPYCAT
LIQUIDITY_ATTENTION_ROTATED_TO
```

Each relation remains independent. A reused social account does not by itself
prove common ownership; funding lineage does not by itself prove official status.

#### X/social approach

The browser worker uses an authorized session and token-triggered queries:

```text
contract address
exact name/symbol + disambiguating fingerprint
immutable account ID and current/historical handle
website/domain and Telegram URL/ID
description phrase, image/OCR hash, alias/wordplay
deployer/caller/funder identity
linked cross-chain contract addresses
```

It captures post/profile IDs, immutable account ID, text/media hash, published
time, observed time, reply/quote/repost relation, profile CA/link changes, raw
evidence reference, parser version, coverage, and session health. Official CA
announcements are distinct from third-party mentions. Missing or challenged X
coverage is `Insufficient`/`UNAVAILABLE`, never “no activity”. No CAPTCHA bypass,
account farming, quota evasion, or claim of whole-platform coverage.

#### Confidence and merge gates

```text
Exact
  same on-chain signer/authority; same immutable social account ID; or
  first-party account explicitly announces the chain-qualified contract.
Reconstructed
  new wallet funded by known deployer plus reused social/domain and coherent time.
Estimated
  multiple weaker fingerprint/link signals without authoritative confirmation.
Insufficient
  symbol/name/image/handle similarity alone or contradictory ownership evidence.
```

Cross-chain family membership requires at least one authoritative relation or
multiple independent corroborating signals. Ambiguous contracts remain separate
nodes with candidate edges. Confidence never upgrades merely because providers
repeat the same upstream source.

#### Recent event projection

Every event carries:

```text
event_type
anchor_token_key and related entity keys
chain_id + contract_address where applicable
occurred_at and observed_at
relation type + truth status + confidence components
source/evidence refs + dependency group
freshness/coverage/capability status
supersedes/retracted status
```

Sort by `occurred_at`; expose `observed_at` separately. Default windows are
`1h`, `24h`, `7d`, and `30d`. Event classes include deploy/authority change,
first liquidity/migration/revival, material market or holder activity, deployer
reuse, social reuse/profile CA change, official announcement, cross-chain family
candidate/confirmation, copycat warning, and liquidity/attention rotation.

This capability is **planned, not implemented** until ingestion, evidence graph,
reverse indexes, social worker, materialized projection, API, and dashboard tests
all satisfy the acceptance gates below.

#### Minimum acceptance scenarios

1. Same symbol/name on two chains without corroboration stays as two unrelated
   contracts; no family merge.
2. Same chain-qualified deployer/authority on multiple contracts creates the
   corresponding exact relation while factory/launchpad addresses remain separate.
3. A new wallet funded by a known deployer plus the same immutable X account ID
   creates a `Reconstructed` candidate, not automatic official status.
4. Reused handle/domain without immutable identity or ownership evidence remains
   `Insufficient`/candidate and raises possible copycat risk.
5. An official first-party account announcing an exact chain-qualified contract
   creates `OFFICIAL_CA_ANNOUNCEMENT` with immutable post/profile evidence.
6. Cross-chain contracts explicitly announced by the same authoritative project
   account may join one family; symbol-only namesakes may not.
7. Reposts/provider copies of one source collapse into one dependency group while
   retaining every evidence reference.
8. Deleted/retracted social evidence supersedes the projection but remains archived;
   missing/challenged X coverage remains visible.
9. Timeline ordering uses `occurred_at`, exposes `observed_at`, and returns evidence,
   confidence, freshness, coverage, chain, relation, and capability status.
10. Dormant/tombstoned tokens receive no individual polling and wake only from global
    chain/social events or explicit operator request.

### 8.4 Name/Meme Provenance

Token-first flow:

```text
deploy/first liquidity
→ metadata fingerprint
→ local archive search
→ exact/alias/web/X/TikTok search
→ OCR/ASR/image/phonetic expansion
→ earliest evidence
→ origin/adoption/propagation graph
```

Roles remain separate:

```text
originator
independent spread
official adopter
deployer
caller/amplifier
market-leading contract
```

Truth status:

```text
Exact
Reconstructed
Estimated
Insufficient
```

BASELINE example remains golden design case:

```text
TikToker mispronunciation
→ viral Baseline meme
→ Vaseline official adoption
→ token family
```

### 8.5 Caller Intelligence

- Telegram message/edit/delete/forward/reply evidence.
- CA resolution.
- Immutable T0 snapshot.
- Call timestamp and lead time.
- MFE/MAE.
- Realistic copy entry/PnL.
- Outcome windows through `+21d`.
- Caller reputation by regime and sample confidence.
- Propagation and copy-caller graph.

### 8.6 Wallet Intelligence

- Chain-qualified address + optional entity cluster.
- Exact swap reconstruction and cost basis.
- Early-entry timing.
- Realized/unrealized outcome.
- Recurrence across tokens.
- Independence/funding/cabal context.
- Objective-specific dimensions; no one smart-wallet score.

Custom tags are assertions:

```text
namespace:name
source_type = manual | system | import | vendor | inferred
truth_status
confidence
valid_from/valid_until
status = active | disputed | expired | revoked
supersedes_id
```

Manual assertions are never silently overwritten.

### 8.7 Cabal/Funding Graph

- Funding relationships.
- Synchronized entries/exits.
- Common deployer/authority.
- Shared counterparties.
- Correlated wallet clusters.
- False confluence detection.
- Evidence/confidence per edge.

### 8.8 Revival Intelligence

```text
global trade/social wake
→ dormant baseline comparison
→ cheap activation gate
→ narrative/caller/wallet refresh
→ revival quality
→ full opportunity evaluation
```

Prior token history and failure memory stay attached.

### 8.9 LP Pool Intelligence

Solana Meteora:

- Active bin/bin step/range.
- TVL/active TVL/reserves.
- Volume/fees/fee-to-TVL.
- Position history/claims/rewards/PnL.
- Initialized bin arrays/rent risk.

Robinhood:

- Verified factory/pool/manager.
- V2 reserves/share/full-range economics.
- V3 tick/liquidity/fee tier/position NFT.
- V4 PoolKey/PoolManager/hook policy.
- Self-computed LP PnL and gas.

### 8.10 LP Wallet Intelligence

- Position timing and hold duration.
- Range choice and reseed behavior.
- Fee/inventory/reward PnL.
- Outcome concentration.
- Regime consistency.
- Pool specialization.

### 8.11 Portfolio/Risk

- Exposure per token/pool/chain/strategy.
- Correlated exposure.
- Wallet reserve.
- Total open notional.
- Realized/unrealized PnL.
- Daily loss/drawdown.
- Ambiguous execution exposure.
- Treasury/hot-wallet separation.

### 8.12 Source Health

```text
UP
SILENT
DEGRADED
DOWN
RECOVERING
DISABLED
```

Connected-but-silent is not healthy. Health includes last request/event/success, expected cadence, parser success, schema drift, quota/backoff, latency, error rate, and coverage impact.

## 9. Evidence and graph model

PostgreSQL graph tables:

```text
entity_nodes
entity_edges
evidence_refs
raw_evidence
observations
incidents
```

Core node types:

- Token/Contract.
- TokenFamily.
- Wallet/WalletCluster.
- Caller/SourceAccount.
- Post/Message/ExternalEvent.
- Narrative.
- Pool/Position.
- Strategy/Policy.
- Decision/Intent/Execution.

Edge examples:

```text
DEPLOYED_BY
FUNDED_BY
CALLED_BY
AMPLIFIED_BY
DERIVED_FROM
OFFICIALLY_ADOPTED_BY
FIRST_LIQUID_ON
HOLDS
SWAPPED
LP_PROVIDED
SAME_FAMILY_AS
SAME_DEPLOYER
SAME_AUTHORITY
SAME_FEE_PAYER
SAME_SOCIAL_ACCOUNT
REUSED_SOCIAL_LINK
OFFICIAL_CA_ANNOUNCEMENT
CROSS_CHAIN_DEPLOYMENT
SUSPECTED_COPYCAT
LIQUIDITY_ATTENTION_ROTATED_TO
EVIDENCED_BY
RESULTED_IN
```

Every edge stores time, source, truth status, confidence, evidence refs, valid window, and supersession status.

Neo4j deferred. Materialized projections serve operational queries.

## 10. Core relational model

### Access/control

```text
workspaces
identities
workspace_memberships
invites
sessions
webauthn_credentials
workload_identities
audit_events
```

### Sources/providers

```text
sources
provider_accounts
provider_credentials_ref
provider_endpoints
provider_health
provider_usage
source_dependencies
```

Secrets are external encrypted references; UI sees fingerprint/status only.

### Intelligence

```text
tokens
token_lifecycle_events
token_families
social_identities
token_social_bindings
token_recent_events
token_recent_projections
wallet_addresses
wallet_clusters
wallet_tag_definitions
wallet_tag_assertions
wallet_notes
callers
calls
narratives
pools
lp_positions
portfolio_snapshots
risk_findings
entity_memory
cooldowns
```

### Strategy/evaluation

```text
strategy_sources
strategies
strategy_versions
strategy_requirements
strategy_evaluations
shadow_outcomes
lessons
lesson_validations
```

Lesson lifecycle:

```text
PROPOSED → VALIDATED → ACTIVE → RETIRED
```

No direct auto-activation from prose or tiny samples.

### Decisions/execution

```text
decision_bundles
decision_components
decision_transitions
automation_policies
automation_policy_versions
trade_intents
capital_reservations
execution_attempts
chain_transactions
reconciliation_incidents
positions
position_actions
kill_switches
```

All status transitions append audit events. Operational projection may update, but history is immutable.

## 11. Scheduling and refresh

### Refresh tiers

```text
Critical  15–30s
Hot       1–5m
Warm      15–60m
Cold      6–24h or event-driven
Dormant   no individual polling
Dead      no individual polling
```

Examples:

- Open position/risk/reconciliation: Critical.
- Hot token/caller/LP opportunity: Hot.
- Active wallet/narrative follow-up: Warm.
- Activated Token Recent enrichment: Hot; linked social/family follow-up: Warm.
- Source registry/static metadata: Cold.
- Archived token: event wake only.

### Job model

```text
jobs
- job_type
- entity_key
- priority
- available_at
- lease_owner
- lease_until
- attempts
- max_attempts
- dedupe_key
- payload_version
```

Workers claim via `FOR UPDATE SKIP LOCKED`. Backoff, dead-letter incident, and operator retry are explicit.

### Priority triggers

- First meaningful liquidity.
- Migration/graduation.
- Credible caller.
- Smart-wallet entry.
- Fresh-wallet/cabal burst.
- Volume/trade activation.
- Dormant wake.
- LP economics/range change.
- Source-health degradation.

## 12. Decision architecture

### 12.1 Decision bundle

```text
DecisionBundle
- target entity/action
- decision_at
- evidence snapshot IDs
- component results
- missing capabilities
- source freshness/completeness
- confidence/truth status
- strategy version
- rule version
- policy version
- alternatives/rejections
- final disposition
```

### 12.2 Component classes

```text
MANDATORY PASS
security, contract identity, chain state, mandatory freshness

SIZING INPUT
liquidity, volatility, confidence, portfolio exposure

STRATEGY INPUT
narrative, caller, wallet, revival, LP economics, regime

HALT INPUT
source degradation, chain disagreement, drawdown, reconciliation incident
```

No universal score. `N/A`, missing, zero, and safe are distinct.

### 12.3 Opportunity outcomes

```text
SURFACE
WATCH
REJECT
COOLDOWN
PAPER_INTENT
CONFIRM_INTENT
AUTO_INTENT
HALT
```

Every reject/suppress has reason and later shadow outcome where feasible. This powers missed-runner review.

## 13. Strategy Lab and evaluation

### Strategy source

- Raw URL/content hash.
- Author and attribution.
- Claims/modules/assumptions.
- Required capabilities.
- Chain/venue/regime scope.
- Editable copy separated from immutable source.

### Evaluation requirements

1. Immutable T0 features only.
2. Rejected-candidate shadow outcomes.
3. Fixed outcome windows.
4. Position-sized executable quotes.
5. Fees/gas/tip/rent/slippage/latency.
6. Chronological train/validation/test.
7. Purge/embargo where labels overlap.
8. Rolling walk-forward.
9. Untouched holdout.
10. Regime/source/chain/route slices.
11. Missing-feature coverage.
12. Confidence intervals and multiplicity awareness.
13. Negative findings retained.
14. Shadow before canary/active.

Strategy lifecycle:

```text
DRAFT
SHADOW
PAPER
VALIDATED
APPROVED
CANARY
ACTIVE
PAUSED
RETIRED
```

## 14. Token Auto-Trader

### Actions

```text
BUY
SELL
PARTIAL_SELL
CLOSE
EMERGENCY_EXIT
```

Arbitrary transfers, arbitrary calldata, and wallet sweep are not trading actions.

### Flow

```text
activation/revival/caller/wallet event
→ evidence bundle
→ mandatory token/security gates
→ strategy/regime evaluation
→ portfolio sizing
→ immutable intent
→ atomic reservation
→ fresh position-sized quote
→ transaction simulation/decode
→ signer policy
→ broadcast once
→ reconciliation
```

### Policy limits

- Max per trade/token/chain/strategy.
- Max total exposure.
- Trades/notional per time window.
- Slippage/price impact/gas/tip.
- Liquidity/route depth.
- Daily loss/drawdown.
- Wallet reserve.
- Allowed router/program/contract.
- Cooldown and denylist.

## 15. LP Autopilot

### Actions

```text
OPEN_POSITION
ADD_LIQUIDITY
CLAIM_FEES
COMPOUND_FEES
PARTIAL_WITHDRAW
CLOSE_POSITION
RESEED_POSITION
SWAP_RESIDUALS
EMERGENCY_EXIT
```

### Autonomous cycles

```text
screen pools
→ strategy/range compatibility
→ OPEN_POSITION intent
→ monitor authoritative position
→ deterministic claim/compound/range/yield/risk rules
→ action intent
→ signer
→ reconciliation
```

### Meteora DLMM adapter

- Exact program/pool/mints.
- Active bin/bin step/range.
- Initialized bin-array checks.
- Reject unexpected non-refundable initialization rent by default.
- Single/dual-sided capability.
- Spot/Curve/Bid-Ask; advanced Multi-Layer only after validation.
- Claim/add/withdraw/close/reseed.
- No unrelated debit/writable/authority/close-account instruction.

### Robinhood V3 adapters

Uniswap V3 + Pancake V3:

- Chain ID `4663`.
- Factory/pool/position manager allowlist.
- Token order, fee tier, tick spacing.
- Tick bounds/deadline/min amounts.
- Exact recipient.
- No unlimited approval.
- Full multicall decode.

### Robinhood V4 adapter

- Exact PoolManager/PositionManager.
- Exact PoolKey/currency order/fee/tick spacing.
- Hook address and permissions allowlist.
- Hookless/explicit approved hooks first.
- Unknown hooks/callbacks/calldata rejected.

### Robinhood V2 adapters

Uniswap V2 + Pancake V2:

- Factory/pair verification.
- Full-range add/remove.
- Reserve/share/economics accounting.
- `RESEED_POSITION` and range shifts = `N/A`.

### LP accounting

Separate:

```text
inventory PnL
fee PnL
reward PnL
impermanent-loss estimate
swap/rebalance friction
gas/rent/tips
realized PnL
unrealized PnL
```

## 16. Execution state machine

```text
PROPOSED
APPROVED
RESERVED
BUILT
SIMULATED
SIGNED
SUBMITTED
CONFIRMED
FAILED_SAFE
UNKNOWN_RECONCILIATION
CANCELLED
```

### Idempotency

```text
workspace
+ strategy/policy
+ chain
+ wallet
+ source signal/event
+ action
+ target entity
+ policy version
```

### Locks/reservations

- One active execution lane per wallet.
- One action lock per position.
- Atomic capital/exposure reservation before side effect.
- Intent expiry and one-time nonce.
- Unknown transaction blocks conflicting action.

### Reconciliation

On timeout/ambiguous response:

```text
check signature/nonce/receipt/balance/position
→ LANDED
→ NOT_LANDED
→ UNKNOWN
```

No new submission while `UNKNOWN`.

## 17. Signer policy

The signer verifies independently:

- Chain ID/genesis.
- Wallet/workspace/policy.
- Policy active/not expired/not halted.
- Intent hash/idempotency/nonce.
- Router/program/factory/manager.
- Instruction/function selector.
- Pool/token pair/recipient/authority.
- Maximum native/token debit.
- Minimum output.
- Slippage/price impact.
- Gas/priority fee/tip/rent.
- Writable accounts/approvals.
- Deadline.
- Simulation deltas.

No endpoint signs arbitrary serialized transaction/calldata. If provider supplies a transaction, every operation is decoded and checked; preferred path is local construction from canonical intent.

## 18. Key custody

- Dedicated capped hot wallet per chain/risk domain.
- Treasury/vault never connected to auto-trader.
- Manual sweep to pre-approved destination.
- Key absent from browser, DB, logs, source, normal app `.env`.
- Isolated signer process/host.
- Tested backup/rotation before funding.
- HSM/MPC only when capital/risk justifies complexity.

## 19. Kill switches and exit-only mode

Kill switches:

- Global.
- Per chain.
- Per wallet.
- Per strategy/policy.
- Per protocol adapter.
- Source-health automatic.
- Daily-loss/drawdown automatic.
- Signer-local emergency stop.

When halted:

- No new entries/signatures by default.
- Reconciliation continues.
- Risk-reducing claim/withdraw/close may operate only under explicit `EXIT_ONLY` policy.

## 20. Observability and operations

### Metrics

- Source request/event/success latency.
- Session/challenge/parser health.
- Provider quota/cost/backoff/breaker.
- Queue depth/age/lease failures.
- Projection lag.
- Job success/dead-letter.
- Decision counts/reasons/missingness.
- Intent lifecycle latency.
- Signer denials.
- Unknown reconciliation.
- Portfolio exposure/loss/drawdown.

### Logs/traces

Structured request/job/decision/intent IDs across workers. Secrets and auth headers redacted.

### Alerts

- Source blind/silent.
- Parser/schema drift.
- Provider pool exhaustion.
- Queue/scheduler starvation.
- DB/checkpoint/storage failure.
- New privileged login/device.
- Role/policy/wallet/signer change.
- AUTO activation or limit increase.
- Repeated signer denial.
- Ambiguous transaction.
- Daily-loss/source-health halt.

### Watchdog principle

Healthy = silent. Alert only actionable degradation.

## 21. Data retention

### Hot

- Active opportunities/positions/intents.
- Recent observations and projections.
- Fast query indexes.

### Warm

- Full event/evidence and outcome windows.
- Decision bundles and strategy evaluation.

### Cold/archive

- Compressed raw evidence.
- Token tombstones.
- Historical graph edges.
- Closed positions/executions/audit.

Raw/media retention can be policy-driven, but hashes/evidence metadata and decision bundles must remain auditable.

## 22. Dashboard information architecture

```text
Command Center
Signals
Entities
├─ Tokens/Families
├─ Wallets/Clusters
├─ Callers
├─ Narratives
└─ Pools/Positions
Strategies
Lab
Operations
```

Key surfaces:

- Signal Spine.
- Lifecycle Rail.
- Evidence Strips.
- Confidence/freshness texture.
- Narrative River/Origin Trace.
- Token Family Constellation.
- Token Recent Timeline: chain/social/deployer filters, relationship evidence,
  occurred-vs-observed time, confidence/freshness/coverage, and 1h/24h/7d/30d windows.
- Deployer/Social Reuse Panel: prior/new deploys, authority/funder lineage,
  immutable social IDs, historical bindings, official announcements, copycat warnings.
- Caller Propagation Tree.
- Revival Seismic View.
- LP Range Chamber.
- Automation Policies.
- Execution Tape.
- Source Health/Cost.
- Decision explanation and missed-runner review.

Operational dashboard is opportunity-first. Investigation workspace is research-first.

## 23. Failure semantics

### Data capability modes

```text
FULL
DEGRADED
ON_DEMAND
UNAVAILABLE
```

### Data statuses

```text
Exact
Reconstructed
Estimated
Insufficient
N/A
```

### Rules

- `UNAVAILABLE`/`Insufficient` never converted to zero.
- Mandatory missing data blocks new execution.
- Provider fallback retains source identity and confidence.
- DB/guard failure does not silently pass safety checks.
- Stale cached data cannot trigger execution without policy-permitted hard age.
- Chain truth overrides stale local projection; mismatch creates incident.

## 24. Build order

### Phase 0 — Foundation

- Repo/project conventions.
- PostgreSQL migrations.
- Event/evidence envelope.
- Entity IDs and graph tables.
- Source registry/provider pool.
- Jobs/outbox.
- OIDC/RBAC/audit.
- Source health.

### Phase 1 — Core intelligence

- Solana/EVM raw ingestion.
- Token lifecycle/family/security.
- Token Recent event envelope, reverse indexes, cross-chain candidate resolver,
  and per-token/per-family projection.
- Wallet swaps/cost basis/tagging.
- Portfolio projections.
- Dashboard core.

### Phase 2 — Caller + provenance

- Telegram MTProto archive.
- X browser worker.
- Web/TikTok token-triggered resolver.
- Deployer/social reverse lookup, profile-link diff, and official CA attribution.
- Caller outcomes.
- Narrative graph.

### Phase 3 — Revival + cabal

- Dormant tombstones/baselines.
- Global wake detector.
- Funding/synchronization graph.
- Revival incidents.

### Phase 4 — LP intelligence

- Meteora pool/position/PnL.
- Robinhood protocol adapters/indexing.
- LP Wallet intelligence.
- LP Range Chamber.

### Phase 5 — Strategy Lab

- Strategy source/versioning.
- Shadow outcomes.
- Walk-forward/holdout evaluation.
- Paper execution with position-sized quote friction.

### Phase 6 — Secure execution

- Automation policies.
- Capital reservations/idempotency.
- Isolated signer.
- CONFIRM_EACH token + LP claim/close.
- Reconciliation/kill switches.

### Phase 7 — AUTO_BOUNDED

- Autonomous token entry/exit canary.
- Autonomous LP claim/close.
- Autonomous LP open/reseed after forward evidence.
- Advanced compound/partial/multi-layer/V4 hooks later.

## 25. Explicit non-goals

- No all-X/TikTok firehose claim.
- No CAPTCHA bypass or account farming.
- No quota evasion.
- No opaque universal alpha score.
- No automatic strategy mutation from LLM prose.
- No direct vendor score verdict.
- No arbitrary signer endpoint.
- No treasury wallet in execution worker.
- No Neo4j/Kafka/Redis before measured need.
- No LP for Ethereum/Base/BSC.
- No pool/token creation in initial auto-execution scope.
- No reuse of unlicensed Meridian/Kaiser code/model artifacts.

## 26. Architecture invariants / acceptance gates

Implementation cannot be called complete unless:

1. Every observation has source/freshness/raw evidence or explicit reason why not.
2. Every decision is reproducible from immutable point-in-time bundle.
3. Missing mandatory data blocks execution.
4. Every execution has policy version, intent hash, reservation, and audit trail.
5. Signer independently validates full transaction semantics.
6. Ambiguous submission reconciles before retry.
7. One wallet execution lane and position action lock prevent races.
8. Dashboard never stores or receives private key.
9. Human privileged changes require OIDC identity + WebAuthn step-up.
10. Worker uses workload identity, not human session.
11. Chain restart reconciliation happens before new writes.
12. All unsupported protocol capabilities return `N/A`.
13. Token/LP PnL components remain separate.
14. Strategy activation follows shadow/paper/validation/approval lifecycle.
15. Source health can automatically halt new execution.
16. Global and signer-local kill switches are tested.
17. Audit logs redact secrets and remain append-only.
18. Repository licensing is checked before external code reuse.
19. Token Recent never merges contracts by symbol/name alone; every cross-chain,
    deployer, funder, or social relation retains chain-qualified entities,
    occurred/observed time, evidence, source dependency, freshness, and confidence.
20. A reused handle/domain without immutable identity or corroboration remains a
    candidate relation; missing X/social coverage is explicit, never inferred absent.
21. Recent timelines dedupe correlated copies and preserve retraction/supersession.

## 27. Canonical architecture summary for memory

```text
Marker: PLAN SWI
Product: Signal Forge
Architecture: modular monolith + PostgreSQL workers + browser worker + isolated signer
Chains: SOL, RH, ETH, Base, BSC
LP: SOL Meteora; RH Uniswap v2-v4 + Pancake v2-v3; ETH/Base/BSC N/A
Intelligence: token, selective token intake/triage, wallet, caller, provenance, birth, family, token recent, deployer/social reuse, fresh-wallet rotation, cabal/team-volume attribution, LP, strategy, portfolio, source health
Execution: future Token Auto-Trader + LP Autopilot AUTO_BOUNDED
Auth: invite-only Google OIDC, issuer+subject, RBAC, WebAuthn step-up
Autonomy: no per-action login after policy activation; worker uses workload identity
Signer: isolated deterministic policy signer; no arbitrary tx signing
Data: PostgreSQL event/evidence graph + projections + queue/outbox; content-addressed blob evidence
Decision: immutable point-in-time evidence bundles; no opaque universal score
Safety: capital reservation, idempotency, one wallet lane, simulation, full decode, reconciliation-first retry, kill switches
Persistence: archive/tombstone, never delete historical truth
External repos: concepts only; no unlicensed Meridian/Kaiser code reuse
```

## 28. Architecture amendment — Selective Token Intake + Cabal Wallet Tracking

**Amended at:** 2026-09-05  
**Scope:** Intelligence Plane only. No execution, signer, custody, treasury, LP action, or automatic trading scope is added.

### 28.1 Token-first intake and operator triage

Input authority is `chain_id + contract_address`; name/symbol is only a discovery alias and must resolve to an explicit contract before analysis.

Every submitted token receives cheap intake and a retained evidence baseline:

```text
OBSERVED
→ INCUBATING
→ CANDIDATE
→ NEEDS_CONFIRMATION | TRACK | WATCH | FLOW_ONLY | QUARANTINE | REJECT
```

- Intake does not deep-track every participant.
- `INCUBATING` retains deployer/authority, launch/first-liquidity time, liquidity/market-cap/volume snapshots, holder/participant summaries, initial LP, early-transaction references, risk observations, source freshness, and coverage.
- Default deep-enrichment candidate profile: market cap `>= $1M`, token age `<= 7d`, adequate liquidity and organic participation, no critical security finding. These are editable versioned profile defaults, not universal truth.
- Market cap alone never promotes a token; thin liquidity, wash volume, bundled supply, and concentrated ownership remain independent gates.
- Tokens below threshold stay event-driven/incubating so later activation can backfill the retained early window.
- Critical deterministic scam findings recommend `QUARANTINE`; suspicious/fake-volume evidence returns `NEEDS_CONFIRMATION`. Operator choices are `TRACK`, `WATCH`, `FLOW_ONLY`, `KEEP_INCUBATING`, `QUARANTINE`, or `REJECT`.
- Manual decisions are audited, versioned assertions and override automatic promotion/demotion until revoked. Reject/quarantine archives evidence; it does not delete history.

### 28.2 Selective wallet tracking

All token participants may exist as cheap observations. Expensive history, PnL, graph enrichment, and live monitoring apply only to selected candidates.

Candidate reasons include early entry, realized/unrealized profitability, efficient exits, fresh-wallet status, deployer/authority/initial-LP relationship, shared private funder, synchronized entry/exit, recurrence, meaningful size, and transfer to/from a tracked cluster.

```text
TRACK      deep backfill + live monitoring
WATCH      light monitoring; promote on trigger
FLOW_ONLY  retain graph/flow evidence; contributes no alpha
SKIP       baseline only; no expensive enrichment
INFRA      infrastructure/noise; visible but zero cabal/alpha weight
```

Token-specific performance and global reputation remain separate. Example assertions: `SHROOM_EARLY`, `SHROOM_PROFITABLE`, `ALPHA_SHROOM`, and cross-token `REPEATED_ALPHA`. One profitable token never creates a global alpha label. Every label stores sample size, concentration/outlier risk, realized/unrealized split, completeness, valid window, confidence, evidence refs, and source type.

### 28.3 Fresh-wallet rotation and cluster continuity

Tracking follows probabilistic entity continuity, not address permanence. A fresh address may join a candidate cluster through independently evidenced direct/private funding, common upstream private funder, funding shortly before entry, buy-plus-gas amount matching, synchronized repeated entries/exits, unusual token overlap, proceeds returning upstream, shared deployer/authority/fee payer/initial-LP actor, or repeated amount/timing/venue fingerprints.

`FUNDED_BY` never means `CREATED_BY` or common ownership. Wallet rotation/team/cabal identity remains `Exact`, `Reconstructed`, `Estimated`, or `Insufficient`; contradictory evidence remains visible.

### 28.4 Deployer/team/cabal correlation

Preserve deployer, creation signer, fee payer, mint/freeze/update authority, metadata updater, factory/launchpad/program, initial LP actor, first funder, buyer/seller, and inferred cluster as distinct roles.

```text
DIRECT_TEAM       authoritative signer/authority/LP evidence
PROBABLE_TEAM     direct private funding or strong corroborated continuity
POSSIBLE_CABAL    coordinated behavior without ownership proof
INFRA_ONLY        retained evidence, zero team/cabal weight
INSUFFICIENT      missing or contradictory proof
```

Factories, launchpads, routers, relayers, bridges, exchanges, MEV/searchers, system/rent/ATA operations, and dust/spam are never promoted to human/team identity merely because they occur on a path. No single shared funder, bridge, entry time, fresh-wallet flag, or popular-token co-buy is sufficient.

### 28.5 Noise-preserving graph policy

Every edge is retained for audit; decision contribution remains separate.

| Edge class | Retain | Cabal/team weight |
|---|---:|---:|
| Direct wallet funding | yes | eligible |
| Shared private funder | yes | eligible with corroboration |
| Deployer/authority/initial-LP relation | yes | eligible by evidence strength |
| Bridge/relayer/CEX withdrawal | yes | zero |
| DEX/aggregator router or MEV/searcher | yes | zero |
| System/rent/ATA operation | yes | zero |
| Dust/spam | yes | zero |
| Personal-wallet token/proceeds transfer | yes | eligible after classification |

Infrastructure classification is versioned/time-aware. Unknown is not silently treated as private. UI shows infrastructure/noise separately and keeps evidence inspectable.

### 28.6 Team-volume attribution

Token views separate total gross volume, probable-team gross volume, probable-team net flow, wash-adjusted volume, external organic volume, infrastructure/noise volume, fresh-rotation wallet count, cluster concentration, confidence, coverage, and missing inputs.

Team volume is an evidence-backed estimate, never ground truth. Correlated addresses in one funding/behavior cluster count as one confluence confirmation; raw wallet count is not independent confirmation.

### 28.7 Scale model

Target: thousands of token intakes/day, not deep tracking every participant.

```text
Tier 0  cheap token intake for every observed/submitted contract
Tier 1  bounded early-window candidate scan
Tier 2  policy + operator triage
Tier 3  selected wallet/cluster enrichment and live tracking
Tier 4  incremental outcomes, recurrence, graph and alert projections
```

Use event/webhook/stream ingestion, bounded DB-leased queues, incremental PnL/cluster projections, measured hot raw retention, and long-lived normalized facts/evidence hashes. Track provider credits/day, request amplification, 429s, queue lag, reconnect gaps, and completeness. Shortlist size is capacity-controlled; only a small percentage of tokens should trigger expensive backfill.

### 28.8 Provider credential pools

Both Helius and GMGN must support bulk input of legitimate user-owned credentials through the same provider-pool control plane. “Pool” means health-aware scheduling and workload isolation, not quota evasion and not blind retry across every key.

#### Helius pool

- Load contiguous `HELIUS_KEY_1..N` credentials; retain backward compatibility only where explicitly required.
- HTTP selection is health-aware and fair across eligible keys: per-key/per-API-class token buckets, cooldown, bounded `Retry-After`, circuit breaker, usage/credit accounting, and source-health status.
- Streaming/WebSocket credentials use a separate pool/lane. A first-key-pinned stream does not inherit HTTP-pool capacity; stream failover requires lease ownership, reconnect backoff, cursor/gap recovery, and no duplicate subscription ownership.

#### GMGN pool

- Replace the current single `GMGN_API_KEY` runtime shape with contiguous bulk `GMGN_API_KEY_1..N`; legacy `GMGN_API_KEY` may be accepted only as a one-key fallback when numbered keys are absent.
- Each credential owns an independent weighted route bucket, cooldown, breaker, health state, usage/cost counters, and last-success/error metadata.
- Selection is health-aware weighted-fair across credentials eligible for the requested route/capability. Expensive routes consume their documented weight from the selected credential.
- HTTP 429/5xx/network failure may reschedule through normal pool policy after bounded backoff. Authentication, entitlement, validation, or malformed-request failures quarantine the affected credential/request and must not fan the same invalid request across all keys.
- Bulk input stores encrypted credential references plus non-secret fingerprints/status only; raw keys never appear in UI, logs, DB evidence, or review output.
- Pool configuration supports add/disable/drain/test, workload assignment, per-key route allowlist, and a preview before activation. Removal must not interrupt an in-flight leased request.

#### Capacity invariant

- Key count never substitutes for provider plan credits/RPS. Multiple keys may share an account-level quota; additive capacity cannot be assumed without current provider documentation or measured evidence.
- Capacity decisions use per-route requests, credits/day, retry amplification, 429 rate, queue lag, cooldown/circuit-open time, stream gaps, and 20–30% headroom.
- No account/IP rotation for quota evasion. Different provider/vendor observations remain source-distinct and never silently overwrite one another.

### 28.9 Minimum acceptance scenarios

1. Name/symbol cannot merge contracts without explicit chain-qualified resolution.
2. A sub-$1M token remains incubating with early evidence retained, then activates after crossing its versioned profile threshold.
3. Market cap above threshold plus critical scam evidence yields `QUARANTINE`, not auto-track.
4. Suspicious wash/team volume yields `NEEDS_CONFIRMATION` with component reasons.
5. Selecting ten wallets does not deep-track every participant.
6. Global `REPEATED_ALPHA` requires independent cross-token recurrence and sample confidence.
7. Bridge-funded and private-funder-funded wallets remain visible with different cabal weights.
8. Common infrastructure cannot create a probable-team cluster by itself.
9. Deployer-funded fresh-wallet bursts produce a confidence-scored candidate cluster, not certain ownership.
10. A stale worker cannot release a newer enrichment claim.
11. Team-volume components reconcile with visible confidence/coverage.
12. Thousands of daily intakes stay bounded because only shortlisted entities trigger backfill.
13. Helius HTTP selection skips cooling/open credentials and records per-key/class usage; the independently leased streaming pool proves failover and gap recovery.
14. GMGN accepts bulk `GMGN_API_KEY_1..N`, distributes route-weighted requests across healthy eligible credentials, quarantines auth-invalid keys without retry fan-out, and records per-key usage/health.
15. Provider capacity reporting distinguishes credential count from shared account/plan quota and raises lag/credit/429 saturation explicitly.
16. Reject/quarantine/manual overrides remain auditable and reversible without deleting evidence.

## 29. Architecture amendment — SWI Mega Intelligence Domain Boundaries

**Amended at:** 2026-09-07  
**Decision:** keep one SWI/Signal Forge platform, one repository, and one PostgreSQL authority. Separate Token, Wallet, Cabal, and LP domains at module, queue, projection, score, alert, dashboard, and execution-policy boundaries. Cross-domain Confluence is read-only composition; it never collapses unlike metrics into one universal score.

### 29.1 Domain boundary matrix

| Surface | Shared Core | Token-only | LP-only | Confluence | Execution |
|---|---|---|---|---|---|
| Authority | chain events, entity IDs, wallet/deployer/funder identity, evidence refs | token/contract + market lifecycle | pool/position identity and LP history | immutable component snapshots only | approved action-specific intent |
| Main outputs | normalized facts, graph evidence, source health | token market/security/PnL | pool economics, position/PnL, LP skill | separate component results | deterministic policy result |
| Mutable control | provider/workspace policy | token tracking profile | LP strategy/profile | read-model/rule version | isolated policy version |
| Forbidden mix | no domain score | no LP fees/IL and no wallet/cabal score injection | no token trading-alpha inference | no score blending; missing stays `UNAVAILABLE` | no direct evidence-to-signer path |

Shared Core contains raw/normalized chain ingestion, source/provider pools, entity and evidence graph, canonical wallet/deployer/funder identity, jobs/leases/outbox, workspace/RBAC/audit, source health, and content-addressed evidence. Wallet and Cabal remain separate bounded domains beside Token and LP: `wallet_tracking` owns wallet trading-performance/recurrence projections; `cabal_graph` owns cluster/team-volume projections. Each has its own queue, budget, labels, and lag SLO; both reference Shared Core graph identities without folding their scores into Token. Domain projections consume Shared Core; they do not duplicate ingestion truth.

### 29.2 Queue and worker isolation

Required logical queues:

```text
token_intake
token_enrichment
wallet_tracking
cabal_graph
lp_pool_scan
lp_position_monitor
outcome_measurement
```

- Every queue has independent priority, concurrency ceiling, provider budget, maximum lag, retry/dead-letter policy, and durable pause state.
- `token_intake` and tracked-wallet/cabal alerts outrank broad LP discovery. Critical open-position LP monitoring may have its own higher priority, but cannot consume the Token domain's reserved worker/provider capacity.
- LP scan flood must not delay token intake or cabal alerts. Token launch bursts must not starve critical LP position/risk monitoring.
- Queue ownership uses DB leases/fencing; no process-local lock is authoritative.
- Queue/read authority errors fail closed and surface degraded health.

### 29.3 Resource budgets

Each domain profile defines:

```text
cpu/concurrency budget
provider requests + credits/day
stream/subscription slots
hot-storage bytes/day
retention window
priority + maximum lag
backfill ceiling
circuit-open/degraded thresholds
auto-pause threshold + resume hysteresis
```

Provider capacity is reserved by workload class. Helius HTTP, Helius streams, GMGN routes, market APIs, and protocol-specific LP sources each expose per-domain usage. One domain cannot borrow reserved capacity when doing so would violate another domain's lag SLO. Borrowing spare capacity requires an explicit scheduler policy and is revoked automatically under pressure.

### 29.4 Provider workload assignment

| Provider pool | Token-only assignment | Wallet/Cabal-only assignment | LP-only assignment | Isolation authority |
|---|---|---|---|---|
| Helius HTTP | token discovery, asset/transaction enrichment | tracked-wallet history, transfers, funding graph | Solana pool/position account and transaction reads | explicit credential/route workload allowlist + reserved credits/concurrency |
| Helius stream | token mint/market triggers | tracked-wallet transfer/trade triggers | pool/position state triggers | independent leases, subscription slots, reconnect/gap cursor |
| GMGN | token market/security/enrichment routes | wallet trade/performance routes | only explicitly allowlisted LP evidence routes | per-route eligible-key set + reserved daily credits/concurrency |
| LP APIs/RPC | no implicit access | no implicit access | protocol pool/position economics | LP-reserved routes/credits; never borrowed from Token reserve under pressure |

Each key/route binding records `allowed_domain`, workload class, reserved credits/day, concurrency, stream slots where relevant, and priority. Unassigned domains cannot select it. Health-aware selection remains shared infrastructure; bulk credentials are not blind rotation. Auth/entitlement/validation errors quarantine the affected credential/request and never fan one invalid request across all keys. No domain may exhaust the full provider/account quota: shared-plan capacity and reserved domain capacity are both enforced.

### 29.5 Projection and retention boundaries

Logical projections remain separate even when stored in one PostgreSQL cluster:

```text
token_market + token_security + token_trading_pnl
wallet_trading_performance + wallet_recurrence
cabal_graph + cabal_team_volume
lp_pool_economics
lp_position_state + lp_position_pnl + lp_wallet_performance
confluence_read_model
```

- Raw event/evidence ingestion is shared and append-oriented.
- Token trading PnL never includes LP fees, rewards, inventory conversion, or impermanent loss.
- LP PnL keeps fee, reward, inventory, IL, rebalance friction, gas/rent/tips, realized, and unrealized components separate.
- Confluence projections reference source projection IDs and freshness; they do not copy one domain's score into another.
- Retention classes are explicit: raw stream/payload/media = short; normalized trades/LP events = medium; aggregates/graph evidence/labels = long; audit records/decision bundles/immutable evidence hashes = permanent.
- Partition/drop policy applies only to explicitly disposable, re-derivable data; evidence and decisions are archive-not-delete.

### 29.6 Score, label, and alert namespaces

```text
token:*** (for example token:quality | token:security | token:early)
wallet:repeated_alpha | wallet:flow_usefulness
cabal:probable_team | cabal:team_volume
lp:range_skill | lp:fee_efficiency | lp:inventory_risk
flow:infrastructure
confluence:<profile>
```

- No universal `smart_score` or combined PnL.
- A strong token trader is not LP-skilled without independent LP history.
- A high-volume wallet may be `flow_usefulness` without alpha authority.
- Token-specific labels such as `SHROOM_EARLY` stay token-scoped; global reputation requires recurrence/sample confidence.
- Alerts carry domain, profile/rule version, severity, destination, dedup key, source freshness, and `why_now` evidence.
- Alert cooldown/dedup is domain-specific; Confluence alerts reference all contributing component IDs.

### 29.7 Dashboard information architecture

```text
Token
Wallet/Cabal
LP
Confluence
Source Health
Review/Confirmation Inbox
```

- Token page: lifecycle/security/deployer/team volume/fresh-wallet rotation/trading outcomes.
- Wallet/Cabal page: trades, recurrence, funding graph, cluster continuity, infra/noise edges.
- LP page: pools, positions, ranges/bins, fees/rewards/inventory/IL, LP-wallet skill.
- Confluence page: component cards, not one score; each card shows source, freshness, confidence, missingness, and evidence.
- Review inbox: `TRACK`, `WATCH`, `FLOW_ONLY`, `SKIP`, `QUARANTINE`, `REJECT`, conflict resolution, and manual overrides.

### 29.8 Cross-domain Confluence

Example output:

```text
Token quality                 PASS
Independent wallet cluster    STRONG
Probable team volume          18%
LP liquidity health           GOOD
Narrative evidence            PARTIAL
```

Rules:

- `UNAVAILABLE`, `Insufficient`, `N/A`, zero, and safe remain distinct.
- Missing LP capability never becomes LP score `0` or weakens token evidence silently.
- Correlated wallets count once for independence.
- Confluence is reproducible from immutable point-in-time component snapshots and explicit profile versions.
- A Confluence decision may surface/watch/reject; it cannot directly sign or execute.

### 29.9 Failure isolation

- LP provider down: Token and Wallet/Cabal pipelines continue within their reserved budgets; LP component is `UNAVAILABLE`.
- GMGN down: raw-chain intake continues; GMGN-derived enrichment is degraded, not fabricated absent.
- Token burst: critical LP position/risk monitor keeps reserved capacity.
- LP discovery flood: token intake/cabal alert SLO remains intact.
- Projection failure: source facts remain; only affected domain/read model degrades.
- Outbox/queue DB failure: affected side effects stop fail-closed; no false success.

### 29.10 Physical split criteria

Keep modular monolith + same-codebase workers until measured evidence shows one or more:

1. sustained domain SLO/latency interference after queue/resource isolation;
2. materially incompatible runtime/dependency requirements;
3. storage/IO workload conflict that partitioning and resource controls cannot contain;
4. independently scaled workload with demonstrated cost benefit;
5. distinct trust boundary, especially execution/signer/custody.

Even after a physical split, canonical event IDs, evidence refs, workspace identity, and decision contracts remain shared. Do not split repositories or databases merely for code organization.

**Not now:** separate Token/LP repositories; separate databases; Kafka; Neo4j; a universal alpha score; or one microservice per module.

### 29.11 Acceptance scenarios

1. Token spike does not slow critical LP position monitoring beyond its maximum lag.
2. LP scan flood neither delays token/cabal work nor consumes the Token domain's reserved provider budget.
3. Token trading PnL never includes LP fees/rewards/IL; LP PnL remains decomposed.
4. Wallet trading alpha does not imply LP skill.
5. Shared wallet/deployer/funder graph resolves once as Shared Core authority across domain views.
6. Confluence keeps working when one domain is unavailable, renders that component `UNAVAILABLE`, and preserves the others.
7. One provider/domain cannot spend another domain's reserved credit budget.
8. Correlated wallets count as one confirmation, not raw-address plurality.
9. Domain alert dedup/cooldown cannot suppress another domain's legitimate alert.
10. A queue authority error stops affected work and raises degraded health.
11. Retention removes only disposable raw partitions; normalized evidence/labels/decisions survive.
12. Physical service split is rejected until at least one measured split criterion is met.

Final form: one **SWI Mega Intelligence** platform with a shared evidence core; isolated Token, Wallet, Cabal, and LP logical domains; Confluence as a read-only composition layer; Execution as a separate trust boundary.

## 30. Related canonical inputs

- `/root/signal-forge-source-matrix-2026-08-28.md`
- `/root/solana-source-matrix-2026-08-28.md`
- `/root/signal-forge-meridian-feature-audit-2026-08-28.md`
- `/root/signal-forge-kaiser-charon-feature-audit-2026-08-29.md`
- `/root/signal-forge-security-auto-trade-design.md`
- `/root/signal-forge-lp-autopilot-design.md`

This document supersedes fragmented architecture assumptions when conflicts exist. Detailed source/license evidence remains in the related audit documents.
