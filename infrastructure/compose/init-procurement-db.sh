#!/bin/sh
set -eu
exec mongosh --quiet --norc 'mongodb://mongo:27017/admin?replicaSet=erp4us' \
    --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" \
    --eval '
        const procurement = db.getSiblingDB("procurement");
        if (procurement.getUser("procurement_app") === null) {
            procurement.createUser({
                user: "procurement_app",
                pwd: process.env.DEV_PROCUREMENT_MONGO_PASSWORD,
                roles: [{role: "readWrite", db: "procurement"}]
            });
        }
    '
