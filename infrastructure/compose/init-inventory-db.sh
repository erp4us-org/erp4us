#!/bin/sh
set -eu
exec mongosh --quiet --norc 'mongodb://mongo:27017/admin?replicaSet=erp4us' \
    --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" \
    --eval '
        const inventory = db.getSiblingDB("inventory");
        if (inventory.getUser("inventory_app") === null) {
            inventory.createUser({
                user: "inventory_app",
                pwd: process.env.DEV_INVENTORY_MONGO_PASSWORD,
                roles: [{role: "readWrite", db: "inventory"}]
            });
        }
    '
