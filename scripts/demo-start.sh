#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ "${ERP4US_PREVIEW_BUDGET_APPROVALS:-0}" = "1" ]; then
    : "${ERP4US_PROCUREMENT_BUDGET_POLICY_FILE:?Select an approved preview budget policy file}"
    if [ ! -f "$ERP4US_PROCUREMENT_BUDGET_POLICY_FILE" ] || [ ! -r "$ERP4US_PROCUREMENT_BUDGET_POLICY_FILE" ]; then
        printf '%s\n' 'Cannot read the selected preview budget policy file.' >&2
        exit 1
    fi
fi
python3 scripts/preview-identity.py prepare
export ERP4US_IDENTITY_DIR=./.erp4us-preview
export ERP4US_ENV_FILE="$PWD/.erp4us-preview/compose.env"
export COMPOSE_PROJECT_NAME=erp4us-preview
if [ ! -f "$ERP4US_ENV_FILE" ]; then sh scripts/init-dev.sh; fi
for key in DEV_NATS_DIAGNOSTICS_PASSWORD DEV_SALES_MONGO_PASSWORD DEV_NATS_SALES_RELAY_PASSWORD \
    DEV_INVENTORY_MONGO_PASSWORD DEV_NATS_INVENTORY_PASSWORD DEV_NATS_INVENTORY_PLAN_PASSWORD \
    DEV_PROCUREMENT_MONGO_PASSWORD DEV_NATS_PROCUREMENT_PASSWORD; do
    if ! grep -q "^${key}=" "$ERP4US_ENV_FILE"; then
        password=$(openssl rand -hex 32)
        printf '%s=%s\n' "$key" "$password" >> "$ERP4US_ENV_FILE"
        unset password
    fi
done
compose() {
    if [ "${ERP4US_PREVIEW_BUDGET_APPROVALS:-0}" = "1" ]; then
        set -- -f compose.procurement-approvals.yaml "$@"
    fi
    docker compose -p erp4us-preview --env-file "$ERP4US_ENV_FILE" \
        -f compose.yaml -f compose.delivery.yaml -f compose.manufacturing.yaml \
        -f compose.preview-identity.yaml -f compose.diagnostics.yaml -f compose.sales.yaml \
        -f compose.sales-events.yaml -f compose.inventory.yaml -f compose.procurement.yaml "$@"
}
compose up -d preview-identity
python3 scripts/preview-identity.py configure
compose up --build -d master-data manufacturing sales sales-relay diagnostics article-relay article-receipts inventory procurement
compose run --rm inventory-seed
compose up --no-build -d inventory-worker inventory-plan-relay procurement-worker
# Identity policy is loaded only at process startup. Recreate consumers of
# the preview-generated file so a repeated launch applies any policy change.
compose up --no-build --no-deps --force-recreate -d master-data manufacturing diagnostics sales inventory procurement
compose run --rm manufacturing-seed
compose run --rm master-data-seed
printf '%s\n' 'Preview API, broker and provider started. Start the Angular workspace with: cd ui && npm ci && npm start' \
    'Open http://127.0.0.1:4200/local-access. Demo credentials are in .erp4us-preview/credentials.json (local, private).'
