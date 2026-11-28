#!/bin/sh
set -eu
exec mongosh --quiet --norc 'mongodb://mongo:27017/admin?replicaSet=erp4us' \
    --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" \
    --eval '
        const sales = db.getSiblingDB("sales");
        if (sales.getUser("sales_app") === null) {
            sales.createUser({
                user: "sales_app",
                pwd: process.env.DEV_SALES_MONGO_PASSWORD,
                roles: [{role: "readWrite", db: "sales"}]
            });
        }
    '
