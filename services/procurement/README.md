# Procurement: draft a proposal for a material-plan shortage

For [SC-05 / PUR-001](../../docs/requirements/catalog/PUR.yaml), Procurement uses Inventory's material plan to draft a recommendation for missing buy components. For example, if Inventory needs ten motors and can reserve six, the draft recommends buying four. A plan with no positive buy shortages produces no draft. An authorized editor may explicitly select a supplier and save exact EUR terms for those four motors; the proposal stays a draft. Saving terms cannot approve purchases, contact suppliers, commit spend, issue orders or receive stock.

## Runtime

`compose.procurement.yaml` extends the local database and delivery overlays. The one-shot initializer creates the `procurement_app` MongoDB user scoped to the `procurement` database. The API binds to loopback on `ERP4US_PROCUREMENT_PORT` (8094 by default). The runtime roles are:

- `serve`: [read a draft, manage its non-binding terms, and record optional budget approval stages](../../contracts/openapi/procurement-v1.yaml). The identity policy must grant `procurement.proposal.read` for reads; terms writes require both `procurement.proposal.terms.write` and `master-data.partner.read` in the verified scope.
- `provision-consumer`: the operator command gets or creates only `procurement-material-plans-v1` on the existing `ERP4US_BUSINESS_EVENTS_V1` stream and refuses a mismatching configuration. It is run separately from the worker.
- `consume`: uses the restricted `procurement_inventory` broker login to attach to that exact tenant-filtered pull durable, with reply inbox `_INBOX.procurement-inventory`. It does not carry operator credentials or manage the stream.

The worker verifies the exact tenant subject, organization, producer, actor identity, correlation and causation, and the protocol and schema versions before writing a draft. These checks complement the broker's producer subject grants; the source header alone is not authentication. The worker saves draft and inbox receipt in one majority-acknowledged local transaction before acknowledging the delivery. `procurement.material_plan_drafts` uses a unique tenant/organization/plan key; `procurement.material_plan_inbox` uses a unique tenant/organization/message key. Identical broker replay and identical snapshots under new message IDs keep the original proposal ID and quantities, even if buy rows arrive in a different order. Rows are sorted by SKU, then revision (null first).

Invalid or out-of-scope events and conflicting snapshots go to `procurement.material_plan_quarantine`. Transient transaction failures get four delayed retries; after the final attempt only quarantine persistence is retried. The quarantine stores broker stream/sequence, reason, delivery count and creation metadata, not the message body. The worker waits for MongoDB to resolve the current effect before reading another delivery or deciding whether to quarantine it. It uses bounded best-effort progress notifications, not an exclusive distributed lock. The worker terminates delivery only after a majority read confirms the quarantine reference. Retain the data and durable while rolling back by stopping the API and worker; no legacy documents require migration.

## Save non-binding purchase terms

GET `.../by-plan/{plan_id}/terms` returns `404` until there is a saved revision. POST a fresh command ID, `expected_revision: 0` for the first save (otherwise the current revision), and one supplier ID and canonical exact EUR unit price per shortage row. The SKU and revision must match exactly, including explicit `null` for an unrevisioned row. Quantities and units come only from the immutable proposal. For example, four motors at `"12.34"` produce `"49.36"`; a price of `"12.345"` or an attempt to change the quantity is rejected. Responses always show `draft` and retain only supplier ID, code and name as a historical snapshot.

New edits use the existing local Compose Master Data owner at `http://master-data:8080/v1/partners/{id}`. Procurement forwards the verified actor bearer and scope; Master Data rechecks its own grant and scope. A missing partner, a foreign partner or a customer-only partner is not a valid supplier. Lookups share a three-second deadline, do not follow redirects, and never log request or response payloads. The draft and saved terms remain readable even when Master Data is offline.

The API creates two scoped unique indexes at startup on `procurement.purchase_terms_revisions`, by plan and revision, and by plan and command ID. A majority-acknowledged local transaction records the actor, time, command and immutable response. A repeat with the same intent, even with rows reordered, returns the original snapshot with `200` without another supplier lookup. A new edit returns `201`, while stale revisions or a changed saved command return `409`. On an uncertain outcome, retry with the same command body and ID. There are no cross-service writes or new events.

## Opt in to budget approval stages

Mount an explicitly reviewed [policy file](../../contracts/schemas/procurement-budget-policy-v1.json) through `compose.procurement-approvals.yaml`. The base configuration has no default and permits no approval if the policy is absent. A stage decision requires both proposal read and the exact `procurement.approval.<stage_key>` grant. The terms editor cannot approve that revision; if configured, approving actors must also be different across stages. Decisions use the same scoped local write guard as terms edits and bind to the original terms and policy. They are not purchase orders or budget reservations.

The new collections require no backfill. Disable the opt-in to hide approval state without deleting saved terms or decisions. When rolling back the service version, stop the newer API before starting an older writer; do not mix writers that do and do not enforce the same guard. Retain the data for review and replay.

## Legacy stdin adapter

For the original stock-shortage fixture (`procurement` with no arguments), run the demonstration from the repository root:

```sh
cargo build --locked -p procurement
python3 tests/integration/procurement.py
```

The test starts a disposable replica set and drives separate processes through their stdin/stdout contracts. The [shortage fixture](../../contracts/fixtures/stock-shortage-v1.json) stands in for a future Inventory event; it is not a broker envelope or a published event from Inventory. The adapter accepts [shortage snapshots](../../contracts/schemas/stock-shortage-v1.json) and [buyer lookups](../../contracts/schemas/get-draft-proposal-v1.json) by shortage ID. Successful JSON output is a draft object (`proposalId`, `status: "draft"`, `shortageId`, `orderId`, `lineId`, `articleId`, `unit`, `quantity`) or `null` when no draft is found or no units are missing. A zero shortage neither creates nor withdraws a draft.

Use a dedicated MongoDB account for the `procurement` database. Set `PROCUREMENT_MONGO_URI`, `PROCUREMENT_TENANT_ID`, and `PROCUREMENT_ORGANIZATION_ID` in the trusted process environment. Scope is not read from the payload. This fixture adapter has no network listener; it is for trusted local operators, not direct buyer or broker access. Build the runnable image with `docker build -f services/procurement/Dockerfile .`.

`procurement.drafts` stores the initial snapshot and creation metadata. The unique scoped shortage key and draft are inserted atomically in one MongoDB document. Retrying the same snapshot returns the existing proposal ID and quantity; a positive snapshot with the same scoped reference and different details is rejected. Revisions, withdrawal, supplier selection and purchasing approval remain out of scope. Roll back by stopping the adapter and retaining the database; no existing service collections are migrated.
