#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
umask 077
set -C
root_password=$(openssl rand -hex 32)
service_password=$(openssl rand -hex 32)
{
    printf 'DEV_MONGO_ROOT_PASSWORD=%s\n' "$root_password"
    printf 'DEV_MASTER_DATA_PASSWORD=%s\n' "$service_password"
    printf 'DEV_NATS_ADMIN_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_RELAY_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_RECEIPTS_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_REPLAY_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_CONFIG_READER_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_DIAGNOSTICS_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_SALES_RELAY_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_INVENTORY_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_INVENTORY_PLAN_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_NATS_PROCUREMENT_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_RECEIPTS_MONGO_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_MANUFACTURING_MONGO_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_SALES_MONGO_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_INVENTORY_MONGO_PASSWORD=%s\n' "$(openssl rand -hex 32)"
    printf 'DEV_PROCUREMENT_MONGO_PASSWORD=%s\n' "$(openssl rand -hex 32)"
} > "${ERP4US_ENV_FILE:-.env}"
printf 'Created .env with local development credentials.\n'
