# Requirements decision queue

Use the existing [RFC process](../../GOVERNANCE.md#rfc-process). A research ticket requests evidence; it does not authorize implementation. Each recommendation must identify who decides, two viable policies, their effects on two contrasting companies, migration/compatibility, access and history risk, the smallest falsifying test and what would justify deferral. A reviewer records the disposition in Git; accepted architecture decisions get a linked ADR.

| Question | Example that should distinguish options | Decision owner for review (not an appointment) |
|---|---|---|
| [Retroactive facts #55](https://github.com/erp4us-org/erp4us/issues/55) | Address entered today, valid last week, invoice already sent | Record owner and commercial document owner |
| [Conversions #56](https://github.com/erp4us-org/erp4us/issues/56) | Fixed pack versus measured timber volume and mass | Unit steward and Inventory posting owner |
| [Translation review #57](https://github.com/erp4us-org/erp4us/issues/57) | Source revised while a translation candidate is awaiting approval | Resource owner and localization reviewers |
| [Business numbers #59](https://github.com/erp4us-org/erp4us/issues/59) | Cancelled draft and simultaneous confirmations in two entities | Document owner with local finance reviewer |
| [NovaMotion execution](novamotion-execution-preview.md) | One ship-complete dispatch versus six plus four under explicit split delivery terms | Manufacturing, Inventory, and fulfillment owners |
| [Private PDF release #17](preview-document-release.md) | Interrupted replacement of shipment evidence versus a service-only completion report | Documents, fulfillment, and security reviewers |
| [Process history #22](novamotion-process-history.md) | A delayed purchase draft after an observed handoff versus an order that needs no inventory | Workflow, fact owners, and security reviewers |
| [Staged purchase approvals #23](preview-budget-approvals.md) | A two-stage NovaMotion draft versus a firm with a different explicitly configured spending policy | Procurement and the authorized budget owner |
| [Runtime diagnostics #122](preview-runtime-diagnostics.md) | Stopped versus absent containers, with no link from diagnostic reads to restart authority | Platform operator and security reviewers |

Do not reopen ADR 0002 to choose a different messaging platform. The outstanding question is how long replay evidence is kept and how its effects are reconciled. Likewise, ADR 0001 decides service ownership, not the master-data override policy.

## Decided for the first reference flows

- [#54](https://github.com/erp4us-org/erp4us/issues/54): [ORG-003](../requirements/catalog/ORG.yaml), Demo Global. One explicitly shared Article identity; description overrides and negotiated prices remain company-local. Override expiry remains a separate question.
- [#58](https://github.com/erp4us-org/erp4us/issues/58): [INV-001](../requirements/catalog/INV.yaml), Demo Trading. Reserve six of ten immediately; separate allocation from dispatch and purchase authorization. Other operating models may need a different policy.
