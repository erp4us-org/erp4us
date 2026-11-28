# Changelog

## Unreleased

- Added a loopback Keycloak login, verified organization selection, and a persisted NovaMotion manufacturing catalog with a browser-based BOM and gross requirements view.
- Added idempotently seeded NovaMotion customers and suppliers plus a scoped, paginated partner directory with optional profiles.
- Added a platform-only application readiness view with real probes, and an explicit reset command for the isolated synthetic preview.
- Added scoped Sales drafts with catalog-backed customer and product snapshots and exact EUR totals. Drafts do not confirm an order or reserve inventory.
- Added explicit Sales confirmation with an atomic outbox and recovery-aware JetStream publication.
- Added replay-safe Inventory consumption and a linked reservation view with actual reserved and outstanding finished-goods quantities.
- Added persisted material netting against pinned multilevel BOMs. Planning reserves available components and shows uncovered production and purchase quantities without creating orders.
- Added authorized shortage handoff through JetStream and scoped, replay-safe Procurement proposal drafts. No approval or purchase order is created.
- Added Article Relay heartbeat visibility to platform diagnostics, separate from readiness and runtime state.
- Added versioned draft purchase terms with verified supplier snapshots and exact EUR unit prices and totals. Saving does not issue or approve orders.
- Added platform-only JetStream backlog and acknowledgement counters for the Inventory and Procurement consumers.
- Added opt-in budget approval stages for exact purchase draft and policy revisions, with separate approvers and no automatic purchase.
- Pinned Debian's `libzstd1` security update in the service images while the upstream Bookworm image still ships the earlier package.
- Replaced the shared demo bearer token with verified OAuth access tokens and per-operation grants scoped to the tenant and organization.
- Added a local German/English Angular workspace for registering and retrieving articles, with labeled icon buttons and a keyboard-driven browser scenario.

- Established the architecture baseline, public roadmap, contribution process and licensing proposal.
- Drafted a language-neutral service manifest, article semantic descriptor and business event envelope.
- Added local Master Data article registration and lookup with MongoDB, Compose and CI checks.
