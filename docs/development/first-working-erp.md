# First Working ERP integration

Tracking: [#119](https://github.com/erp4us-org/erp4us/issues/119). Integration branch: `milestone/first-working-erp`. This work is not a release, production deployment, or claim that the full NovaMotion process is ready.

## Verified baseline

`main` at `9be1eab` already includes the scoped Master Data access-token verifier (#15), the versioned messaging library and Article relay/configuration reference (#97/#98), separately owned Inventory/Procurement fixture adapters (#95/#96/#106), and the Angular shell. The CAD/PLM example (#118) is a test contract, not a live BOM service. None of the remaining open source PRs supplied a production Sales, Inventory, Manufacturing, Shipment, or diagnostics implementation; #28/#31 are contributor/requirement work and #78 is an overlapping draft CI baseline. Leave their original review and milestone assignments intact.

## Current integration: NovaMotion manufacturing structures

Manufacturing owns its own MongoDB collection and credential. Each preview uses an explicit product and BOM revision, traverses pinned subassembly revisions, and computes exact whole-piece quantities with checked arithmetic. The primary and secondary demo structures are inserted by an explicit idempotent seed command; nothing is seeded by the normal server. Master Data and Manufacturing reuse the same static OIDC verification library with explicit owning-service grants. Browser scope selection uses verified memberships, not claims read by the frontend. No NATS event is emitted for the read-only preview. Cross-service commands will use the existing wire and outbox conventions, not this preview API as a write path.

With Docker Compose, Python, and the Node version in `ui/.nvmrc`, use an isolated test project:

```sh
sh scripts/init-dev.sh
python3 tests/integration/identity_fixture.py
docker compose -p erp4us-manufacturing -f compose.yaml -f compose.manufacturing.yaml up --build -d
docker compose -p erp4us-manufacturing -f compose.yaml -f compose.manufacturing.yaml run --rm manufacturing-seed
python3 tests/integration/manufacturing.py
```

The synthetic issuer is only for automated fixture tests, not user authentication. Run `npm ci` then `npm start` from `ui/`; `ERP4US_MANUFACTURING_E2E=1 npm run test:e2e` exercises the planning view through the running API. The [manufacturing contract](../../contracts/openapi/manufacturing-v1.yaml) documents the two scoped operations. Stop the isolated project with its same Compose files and project name. No migration touches existing Master Data, Inventory or Procurement state.

## Try the real browser login

On a trusted local machine, with the usual Compose prerequisites and ports 8080, 8090, 8091, 8092, 8093, 8094, 9443 and 4200 free, run:

```sh
sh scripts/demo-start.sh
cd ui
npm ci
npm start
```

This uses the fixed `erp4us-preview` Compose project, generated local infrastructure credentials, a pinned Keycloak development instance, MongoDB and the existing NATS delivery infrastructure. It provisions two fictional tenant users and a separate platform monitor. It stores secrets in the private `.erp4us-preview/` directory, not in Git or the UI configuration. If necessary trust the generated local-only certificate at `.erp4us-preview/provider/localhost.crt` in the browser used for the demo; do not alter production trust settings. Visit `http://127.0.0.1:4200/local-access` and use **Sign in with Keycloak** and `production` from the local credentials file. Choose **Manufacturing**, **NM-1000** and ten pieces to see both subassemblies and the gross material table. `auditor` is scoped to the other tenant and cannot read NovaMotion planning data. The login uses authorization code with PKCE and the backend makes scope and permissions available through the verified session endpoint. Access tokens are kept in memory; there is no production token refresh or identity-policy management in this checkpoint.

Run `ERP4US_REAL_OIDC_E2E=1 npm run test:e2e -- tests/preview-signin.spec.ts` from `ui/` against this stack. The Playwright certificate exception applies only to that dedicated local test. To stop without deleting volumes:

```sh
ERP4US_IDENTITY_DIR=./.erp4us-preview docker compose -p erp4us-preview \
  --env-file .erp4us-preview/compose.env -f compose.yaml -f compose.delivery.yaml \
  -f compose.manufacturing.yaml -f compose.preview-identity.yaml \
  -f compose.diagnostics.yaml -f compose.sales.yaml -f compose.sales-events.yaml \
  -f compose.inventory.yaml -f compose.procurement.yaml down
```

The explicit demo seed also creates three fictitious customers (including Atlas Handling) and four suppliers in Master Data. Browse the records under **Master data → Business partners** and filter by role. The directory and optional profile are published in the [Master Data API](../../contracts/openapi/master-data-v1.yaml); existing profile-free records remain valid. Seeding again does not overwrite an existing code in its scope. Addresses are synthetic and the current preview has one address and one delivery address per profile, no external identifier import.

The new image scan checks Keycloak and its Java dependencies. The current pinned upstream image still produces one fixable advisory match tracked in [#121](https://github.com/erp4us-org/erp4us/issues/121). Keep this preview loopback-only with synthetic data; do not waive the gate or treat local functional tests as final approval.

### Save and confirm a Sales order

Sign in as `production`, open **Sales orders**, choose **Atlas Handling Technologies GmbH**, select **NM-1000** revision **A**, and enter **10** pieces. Saving captures the customer profile and product/price snapshot for **EUR 24,900.00**. In its row, select **Review confirmation** and **Confirm order**, then reopen Sales to see the persisted status. Sales owns its own MongoDB records and resolves the customer and product through their scoped owning APIs. The [Sales API](../../contracts/openapi/sales-v1.yaml) uses distinct authorization and revision-aware command retries for confirmation. It commits the original terms and a [pending Sales event](../../contracts/schemas/nats-sales-order-confirmed-v1.json) in the same document update even if JetStream is down. A scoped relay sends the event after recovery. The preview supports one whole-piece line and EUR terms, not quotations. Sales itself does not reserve stock, and the browser cannot supply the unit price.

Once confirmed, open **Check reservation** from the order row. Inventory receives the event asynchronously and records **0 EA reserved, 10 EA outstanding** for `NM-1000`: that is the actual result in the seeded finished-goods warehouse, not gross material demand. A separate example, `NM-2000` × 10 on a fresh seed, reserves **6 EA** and leaves **4 EA** outstanding; a second competing order cannot consume those six again. The initialization command is idempotent and does not replenish used stock. See the [Inventory API](../../contracts/openapi/inventory-v1.yaml).

### Plan and reserve components

For the confirmed `NM-1000` line, open **Check reservation** and select **Plan and reserve materials**. This writes a plan. Inventory takes its own ten outstanding pieces and fetches the pinned Manufacturing structure under the actor's existing grants. It reserves eligible stock and saves the plan atomically. It expands only the unfilled portion of subassemblies; expected receipts do not count as stock. A retry or revisit displays the same plan without a second allocation.

On a fresh NovaMotion seed, the plan recommends making ten `NM-1100`, ten `NM-1200` and ten `NM-1000`, and buying **12 end plates, 6 motors, 10 spindles, 8 rails and 10 sensors**. The other materials come from physical opening stock. This is a linked, persisted material plan, not purchase approval, an issued purchase order, a production order or shipping authority. A later order in the same warehouse sees less available stock. Do not rerun the seed expecting to recover stock; releasing/replanning material allocations is not yet exposed.

### Send shortages to Procurement

With the separate handoff grant, choose **Send shortages to Procurement** on the saved material plan. The action commits one pending Inventory event; it neither commits spend nor requires the broker to be online. When Procurement consumes it, the same view can retrieve the authorized **Purchase proposal draft** with a stable proposal ID and the five uncovered quantities. JetStream acknowledgement and a stored Procurement draft are shown separately. Use **Refresh proposal** if delivery is still pending. Retrying the handoff or replaying the event does not create another proposal.

Under **Draft purchase terms**, a user with the separate write grant can select a scoped supplier and enter an exact EUR unit price for each existing row. For the synthetic example, choose `SUP-VECTORDRIVE` and enter 30.10 per end plate, 149.95 per motor, 45.20 per spindle, 12.00 per rail and 7.35 per sensor; the saved draft totals **EUR 1,882.40**. Procurement snapshots the supplier references and computes totals from the original shortage quantities. Saving creates a new revision, not an approval or order. Reopening retrieves the stored draft terms even if the partner directory is offline; updating them requires current supplier verification. See the [Procurement contract](../../contracts/openapi/procurement-v1.yaml).

### Review configured budget stages

Budget approval is an explicit opt-in on a fresh, disposable local preview. To test with the synthetic two-stage policy from this repository, start with:

```sh
ERP4US_PREVIEW_BUDGET_APPROVALS=1 \
ERP4US_PROCUREMENT_BUDGET_POLICY_FILE=./contracts/examples/procurement/budget-policy-v1.json \
sh scripts/demo-start.sh
```

This file is test data, not a real spending policy. The opt-in seeds a `department-approver` and a separate `budget-approver` in a fresh preview realm. It does not modify an existing realm in place: keep using a previous fixture without opt-in if its data must be preserved. Never discard preview volumes just to change a policy. In the saved Atlas proposal, sign in separately with each reviewer and choose the next **Approve stage** after reviewing the exact terms. One approval is not enough for the **EUR 1,882.40** example. A different terms or policy revision cannot reuse the completed stages; deploy policy changes with a new revision. Neither completion nor absence of a policy issues a purchase order or reserves a budget balance.

### Issue and reopen a local purchase order

To opt in to issuance as well as budget approval, start a fresh disposable preview with:

```sh
ERP4US_PREVIEW_BUDGET_APPROVALS=1 \
ERP4US_PREVIEW_PURCHASE_ISSUANCE=1 \
ERP4US_PROCUREMENT_BUDGET_POLICY_FILE=./contracts/examples/procurement/budget-policy-v1.json \
sh scripts/demo-start.sh
```

This adds the separate `purchase-issuer` user. It will not replace an existing realm with different accounts. After both reviewers approve the exact order terms, sign in as `purchase-issuer`, reopen the Sales order and its reservation, and choose **Review purchase issuance** then **Issue local purchase orders**. With the five lines all using `SUP-VECTORDRIVE`, Procurement records one child order with five lines for **EUR 1,882.40**. Reopening the same plan as issuer or `production` shows the same order; the buyer can read but not issue it, and the recorded terms cannot be edited. A repeated original command returns the same IDs. An issued record does not contact a supplier, reserve a budget balance or change stock. Receiving and allocating goods are separate, not yet implemented steps.

### Check application readiness

Sign in with the separate `platform-monitor` account and open **Administration**. The [diagnostics adapter](../adr/0007-preview-application-observation.md) probes Master Data, Manufacturing and JetStream. A responding service can still be unready; a stopped service produces **Not responding**, not a fabricated container status. The screen refreshes about every six seconds. Tenant roles do not implicitly grant platform access.

The same screen shows live counters for the preview business stream and three named consumers: Article receipts, Sales orders to Inventory, and Material plans to Procurement. A missing durable or disconnected broker shows unavailable, not zero backlog. Pending, awaiting acknowledgement, and redelivered are JetStream observations, not a promise that a business operation succeeded. **Article Relay heartbeat** shows the configured interval, last-seen instance and receipt age when available. The observation becomes overdue after three active intervals. It is unavailable if the observer cannot establish a fresh connection or confirm the interval, and disabled if the owner sets the interval to zero. On reconnect the adapter waits for a new heartbeat; an instance ID is not a Docker container ID. The dedicated broker grant permits only named metadata, the exact heartbeat subscription and its read-only settings query. No Docker socket, log bodies or raw broker messages reach the API. Runtime inventory and redacted logs remain in [#122](https://github.com/erp4us-org/erp4us/issues/122).

If the preview realm seed changes, use `sh scripts/demo-reset.sh --discard-preview-data` **only when the isolated synthetic data can be discarded**. The script targets the fixed `erp4us-preview` project and its Compose volumes, not the old fixture or any other project. It archives locally generated identity material under a private git-ignored directory. Start again to generate fresh users and keys.

## Remaining acceptance gates

- Permission-aware workflows across all contexts and editable tenant, organization, warehouse and role administration; extend isolation and user lifecycle coverage beyond the scoped planning view.
- Post [partial receipts](../rfc/preview-purchase-issuance.md#inventory-owns-receipt-and-allocation-proposed) against issued purchase lines under distinct receiving authority, then allocate to the waiting plan and reconcile received/open quantities. The draft itself buys nothing.
- Durable production orders/material consumption, deliveries and stored PDF, process timeline, and business recovery evidence.
- A [reviewed runtime inventory and redacted log boundary](https://github.com/erp4us-org/erp4us/issues/122#issuecomment-7236971940). Do not grant runtime access to any UI, gateway or business container.
- Generate the ten-unit quantitative evidence and full browser test on a clean launch before a final maintainer review or publishing any claims.

### Quantitative acceptance trace (not yet runnable end to end)

Use a fresh disposable warehouse and distinct approved roles. Capture the same scoped Sales order, material plan and purchase line references throughout; do not reset stock, edit a database, or publish a broker message to make a browser step succeed. The [quantity reference test](../../tests/requirements/test_novamotion_quantitative_case.py) checks this arithmetic, not browser behavior or approval.

| Checkpoint | What the browser acceptance must prove |
|---|---|
| Confirm and plan | Atlas: 10 × `NM-1000/A` at EUR 2,490.00, exact total EUR 24,900.00; 0 finished units held and 10 outstanding. The pinned plan starts with 8 end plates held and 12 missing. |
| Approve and issue | Both reviews bind to the same EUR 1,882.40 terms. Neither review issues anything. The separate issuer records one order for the five lines; issued but undelivered supply does not change stock. |
| Post the first lot | The separate receiver sees only eligible lines, not commercial terms. Six end plates give 14 held, 6 open and no new free stock. The original plan still shows 8 held/12 missing **at planning time**. Retry the same ID after an interrupted response, with no extra movement. |
| Prove the dependency | Once its own components are held, the drive can proceed independently. The frame still needs six plates; final assembly cannot use an unproduced frame. An expected delivery or incomplete receipt projection must not be counted as a hold. |
| Finish receipts and build | Record a **new** six-piece end-plate lot: 20 held, 0 open. A further plate is denied. Consume actual held inputs for 10 frames, 10 drives and 10 modules; the output is held for the parent or the Atlas line, not free stock. |
| Ship and reopen | Under the proposed ship-complete terms, six cannot dispatch and ten can. End with 10 shipped, zero outstanding delivery, no scenario holds, zero finished `NM-1000/A` stock and no negatives. Reopen the same authorized shipment PDF and linked history after restart, without a second movement or document. |

Run the denial cases with a wrong role, another organization and another tenant. Record unavailable or stale evidence as such; it is not measured zero. Production, dispatch, receiving activation, document storage and history remain individually gated by their linked reviews.
