# Roadmap

This is a priority order, not a release schedule. Run the smallest useful slice continuously and keep new services independently deployable.

## Current integration: a complete manufacturing preview

The [First Working ERP milestone](https://github.com/erp4us-org/erp4us/milestone/5) uses [tracker #119](https://github.com/erp4us-org/erp4us/issues/119) and one [draft integration PR #120](https://github.com/erp4us-org/erp4us/pull/120). The following milestones describe draft-branch evidence and future exit criteria, not released capabilities.

| Step | Evidence or next gate |
|---|---|
| Sign-in, catalog and order | The draft branch runs real local Keycloak sign-in with verified tenant scope, persisted partners and pinned BOMs. An Atlas order for 10 `NM-1000` at EUR 2,490 confirms durably; Inventory reserves eligible finished stock after JetStream delivery without allocating again on replay. |
| Net material planning | The [tested checkpoint](https://github.com/erp4us-org/erp4us/commit/eaf5ee9) commits one pinned plan and actual component reservations. It recommends making ten of each subassembly and the final module and identifies five exact purchase shortages. |
| Procurement draft | The draft branch creates one Procurement-owned proposal from an authorized handoff of the five shortages. Local Compose and browser tests verify the handoff survives broker outage and replay without changing the proposal. The buyer can record scoped suppliers and exact EUR unit prices without approving or ordering anything. |
| Staged purchasing approval | An explicit [synthetic policy](docs/rfc/preview-budget-approvals.md) requires two decisions for the EUR 1,882.40 draft. Local tests prove distinct actors, stable retry and revision changes without reusing old approval. An approved draft is not a purchase order. |
| Purchase order and partial receipts | The draft branch adds [explicit local issuance](docs/rfc/preview-purchase-issuance.md) for [#23](https://github.com/erp4us-org/erp4us/issues/23): one immutable batch per plan after complete approval, with no supplier transmission. Next: receive the end plates in two lots of six under a separate grant, account for the open quantity and allocate each receipt to the waiting plan exactly once. |
| Production and fulfillment | [Execution proposal](docs/rfc/novamotion-execution-preview.md): consume held inputs, record actual quantities bottom-up, reserve the completed modules for Sales and ship ten. Keep partial shipment terms separate from partial receipts. Persist the [private shipment PDF](docs/rfc/preview-document-release.md) and [linked process history](docs/rfc/novamotion-process-history.md). Verify zero outstanding finished demand and holds, no duplicate movements and no negative stock. |
| Operational acceptance | The draft branch shows application readiness, Article Relay heartbeat age and live JetStream counters for Article, Inventory and Procurement consumers. Local tests distinguish a missing consumer from measured zero and show the Procurement backlog draining after a worker restart. Read-only [runtime inventory and redacted logs](docs/rfc/preview-runtime-diagnostics.md) are the next diagnostic gate, not container control ([#122](https://github.com/erp4us-org/erp4us/issues/122)). Prove the full flow in a clean browser session, tenant isolation and interrupted-service recovery. Qualify the preview identity image ([#121](https://github.com/erp4us-org/erp4us/issues/121)) before final acceptance. |

See the [preview runbook](docs/development/first-working-erp.md) for the exact scope and remaining checks.

## Platform priorities

| Priority | Deliverable | Exit evidence |
|---|---|---|
| Now | Foundation and Master Data article register/get | Compose smoke reaches MongoDB; duplicate SKU is rejected; service restart preserves the article. Draft contract checks, governance and initial threat model are present. |
| P0 | [Real identity and tenant scope](https://github.com/deed-ee/erp/issues/2) | OIDC token verification with a self-hostable provider, tenant/organization membership and per-operation policies. Negative tests across tenants, organizations, wrong audience and missing permission. Retire the demo credential outside isolated tests. |
| P0 | [Reliable event slice](https://github.com/erp4us-org/erp4us/issues/4) and [versioned NATS transport](https://github.com/erp4us-org/erp4us/issues/97) | Compose replica set and JetStream with scoped credentials; exercise the shared messaging crate across Article outbox, acknowledgements, broker outage, forced restart, deduplication, quarantine/replay and trace propagation. Verify a non-destructive cutover and enforce the versioned wire contracts. |
| P1 | [Revisioned configuration reference](https://github.com/erp4us-org/erp4us/issues/98) | Prove an authorized settings snapshot and revision notification with an existing settings owner. |
| P1 | [Registry and Angular shell](https://github.com/deed-ee/erp/issues/3) | Operator-approved manifests, login and scope switching, article form/detail from metadata at runtime, English/German labels and keyboard-accessible Playwright scenario. |
| P1 | Order confirmation to reservation | Master Data partners, UoM and addresses; Sales quotation, conversion, snapshots and confirmation; Inventory warehouse, ledger, availability and reservation. Test both successful and shortage variants without cross-database writes. |
| P1 | Procurement and saga history | Shortage creates a proposal; authorized approval produces a purchase order; Inventory posts the receipt, then reserves against the waiting order; show history and recoverable failures in Angular. Restart each participating service and replay duplicates. |
| P2 | Document lifecycle and external service | Tenant-scoped versioned uploads/downloads with checksums, short-lived URL and orphan cleanup; independent non-Rust example registers through the protocol. |
| P2 | Agent integration | Authorized agent discovers and uses an allowlisted MCP query and approved write using the same domain rules, with audit, dry run and no mandatory LLM. |
| Before preview | Production and community readiness | [License and contribution terms decided](docs/community/licensing.md); validate migrations, backup/restore, sandbox and lifecycle controls; add supply-chain CI, verified builds, screenshots, demo and known limits. Obtain approval before releasing. |

Defer accounting, payroll, general-purpose workflow, WASM, marketplace and multi-region infrastructure until there is a concrete use case. Country-specific tax rules are extension work, not universal core logic. No regulatory compliance is claimed.

For business rationale, proposed M0–M7 phase gates and measurable acceptance cases, see the [requirements guide](docs/requirements/guide.md), [catalog index](docs/requirements/index.md) and [tracked backlog](docs/requirements/backlog.md). The order-to-procurement MVP remains the first cross-service outcome; a phase or capability entry is not a release promise.
