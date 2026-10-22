//! A device: registered once for a user and an application, and checked on
//! every pull and upload.

use sc_auth::User;
use sc_catalog::{Catalog, SYNC_CLIENTS_TABLE, Table};
use sc_error::{Error, Result};
use sc_query::{Expr, Value};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::rows::{self, RowQuery};
use crate::schema::{StructField, TypeSchema};

/// `POST {mount}/sync/register`'s body: an optional name for the device, so an
/// admin can tell a user's phone from their tablet.
pub(crate) fn register_input_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new(
        "name",
        TypeSchema::optional(TypeSchema::text()),
    )])
}

/// The new device's id, which it sends with every pull.
pub(crate) fn register_output_schema() -> TypeSchema {
    TypeSchema::struct_of([StructField::new("client_id", TypeSchema::text())])
}

/// Register a device of `user` for `application`; answers its new id.
pub(crate) async fn register(
    cat: &Catalog,
    application: Uuid,
    user: &User,
    body: &Json,
) -> Result<Json> {
    let name = match body.get("name") {
        None | Some(Json::Null) => Json::Null,
        Some(Json::String(name)) => Json::String(name.trim().to_owned()),
        Some(_) => return Err(Error::invalid("`name` must be text")),
    };
    let devices = devices_table(cat)?;
    let id = Uuid::new_v4();
    rows::create_row(
        cat,
        &devices,
        &json!({
            "id": id.to_string(),
            "user_id": user.id.to_string(),
            "application": application.to_string(),
            "name": name,
        }),
    )
    .await?;
    Ok(json!({ "client_id": id.to_string() }))
}

pub(super) fn devices_table(cat: &Catalog) -> Result<Table> {
    cat.get(SYNC_CLIENTS_TABLE)?.ok_or_else(|| {
        Error::invalid("offline sync needs a Postgres database; this one cannot record changes yet")
    })
}

/// The device must be this user's, for this application: a device id is not a
/// secret, so it is never enough on its own. Answers when it last pulled, if
/// it has.
pub(super) async fn check_device(
    cat: &Catalog,
    devices: &Table,
    id: Uuid,
    application: Uuid,
    user: &User,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let query = RowQuery {
        filter: Some(
            Expr::col("id")
                .eq(Expr::lit(Value::Uuid(id)))
                .and(Expr::col("user_id").eq(Expr::lit(Value::Uuid(user.id))))
                .and(Expr::col("application").eq(Expr::lit(Value::Uuid(application)))),
        ),
        limit: Some(1),
        ..RowQuery::new()
    };
    let Some(device) = rows::list_row_values(cat, devices, &query, None)
        .await?
        .into_iter()
        .next()
    else {
        return Err(Error::not_found(
            "unknown device: register it with `sync/register` first",
        ));
    };
    Ok(match device.get("last_seen") {
        Some(Value::Timestamp(at)) => Some(*at),
        _ => None,
    })
}
