use mongodb::{
    Database,
    bson::{DateTime, Document, doc},
    error::Error,
};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct Release {
    // Amount returned by this invocation, not the historical total.
    released: i32,
}

pub(super) async fn cancel(db: &Database, key: Document) -> mongodb::error::Result<Release> {
    let reservations = db.collection::<Document>("reservations");
    let balances = db.collection::<Document>("balances");
    let mut session = db.client().start_session().await?;
    session
        .start_transaction()
        .and_run2(async |session| {
            let filter = doc! {"_id": &key};
            let row = reservations
                .find_one(filter.clone())
                .session(&mut *session)
                .await?
                .ok_or_else(|| Error::custom("unknown reservation"))?;
            if row.contains_key("cancellation") {
                return Ok(Release { released: 0 });
            }
            if row.contains_key("materialPlanId") {
                return Err(Error::custom(
                    "release the material plan before cancelling this line",
                ));
            }
            let released = row.get_i32("reserved").map_err(Error::custom)?;
            let withdrawn = row.get_i32("outstanding").map_err(Error::custom)?;
            if released > 0 {
                let stock = row.get_document("stock").map_err(Error::custom)?;
                let filter =
                    doc! {"_id": stock, "available": {"$gte": 0, "$lte": i32::MAX - released}};
                let result = balances
                    .update_one(filter, doc! {"$inc": {"available": released}})
                    .session(&mut *session)
                    .await?;
                if result.matched_count != 1 {
                    return Err(Error::custom("cannot restore stock balance"));
                }
            }
            // Keep the confirmation and both original amounts alongside the current state.
            let change = doc! {"$set": {"reserved": 0, "outstanding": 0, "cancellation": {
                "released": released, "withdrawnDemand": withdrawn,
                "recordedAt": DateTime::now(), "actor": "inventory.local-fixture"
            }}};
            reservations
                .update_one(filter, change)
                .session(session)
                .await?;
            Ok(Release { released })
        })
        .await
}
