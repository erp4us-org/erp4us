#!/bin/sh
set -eu
exec mongosh --quiet --norc 'mongodb://mongo:27017/admin?replicaSet=erp4us' \
    --username "$MONGO_INITDB_ROOT_USERNAME" --password "$MONGO_INITDB_ROOT_PASSWORD" \
    --eval '
        const manufacturing = db.getSiblingDB("manufacturing");
        if (manufacturing.getUser("manufacturing_app") === null) {
            manufacturing.createUser({
                user: "manufacturing_app",
                pwd: process.env.DEV_MANUFACTURING_MONGO_PASSWORD,
                roles: [{role: "readWrite", db: "manufacturing"}]
            });
        }
    '
