#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ "$#" -ne 1 ] || [ "$1" != "--discard-preview-data" ]; then
    printf '%s\n' 'This deletes only the erp4us-preview Compose project volumes.' \
        'Run with --discard-preview-data to confirm.' >&2
    exit 2
fi
if [ -L .erp4us-preview ] || [ ! -f .erp4us-preview/compose.env ] || [ ! -f .erp4us-preview/credentials.json ]; then
    printf '%s\n' 'No recognized local erp4us-preview initialization to reset.' >&2
    exit 2
fi
export ERP4US_IDENTITY_DIR=./.erp4us-preview
export COMPOSE_PROJECT_NAME=erp4us-preview
# Older previews lack this value. Compose only interpolates it to stop the
# project; it is never installed or used to authenticate.
export DEV_NATS_DIAGNOSTICS_PASSWORD DEV_SALES_MONGO_PASSWORD DEV_NATS_SALES_RELAY_PASSWORD
export DEV_INVENTORY_MONGO_PASSWORD DEV_NATS_INVENTORY_PASSWORD
export DEV_NATS_INVENTORY_PLAN_PASSWORD DEV_PROCUREMENT_MONGO_PASSWORD DEV_NATS_PROCUREMENT_PASSWORD
DEV_NATS_DIAGNOSTICS_PASSWORD=$(openssl rand -hex 32)
DEV_SALES_MONGO_PASSWORD=$(openssl rand -hex 32)
DEV_NATS_SALES_RELAY_PASSWORD=$(openssl rand -hex 32)
DEV_INVENTORY_MONGO_PASSWORD=$(openssl rand -hex 32)
DEV_NATS_INVENTORY_PASSWORD=$(openssl rand -hex 32)
DEV_NATS_INVENTORY_PLAN_PASSWORD=$(openssl rand -hex 32)
DEV_PROCUREMENT_MONGO_PASSWORD=$(openssl rand -hex 32)
DEV_NATS_PROCUREMENT_PASSWORD=$(openssl rand -hex 32)
docker compose --project-name erp4us-preview --env-file .erp4us-preview/compose.env \
    -f compose.yaml -f compose.delivery.yaml -f compose.manufacturing.yaml \
    -f compose.preview-identity.yaml -f compose.diagnostics.yaml -f compose.sales.yaml \
    -f compose.sales-events.yaml -f compose.inventory.yaml -f compose.procurement.yaml down --volumes
archive=$(mktemp -d .erp4us-preview-archive-XXXXXX)
mv .erp4us-preview "$archive/identity"
if [ -f ui/public/demo-oidc.json ]; then mv ui/public/demo-oidc.json "$archive/browser-config.json"; fi
printf '%s\n' 'Only the isolated preview project was reset. Run sh scripts/demo-start.sh to reseed it.'
