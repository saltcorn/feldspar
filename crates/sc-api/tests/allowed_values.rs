//! A `string` field with allowed values (`options`) in the generated client:
//! the row type is the union of them, and an `<Table>Options` constant lists
//! them, so a form's select offers exactly what a write accepts.

use std::sync::Arc;

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation};
use sc_api::{ApiProvider, RestProvider};
use sc_catalog::{Catalog, DataField, DataFieldKind, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_types::{BasicType, TypeRef};
use serde_json::json;

#[tokio::test]
async fn a_field_with_allowed_values_is_their_union_and_a_constant_lists_them() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "machines",
            &[DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key()
                .generated(ColumnGenerator::Identity)],
        )
        .await?;
    catalog.reload().await?;
    let mut attributes = sc_types::Attrs::new();
    // Written as an admin types it; one value holds a comma and a quote.
    attributes.insert(
        "options".to_owned(),
        json!(r#""Chest", "Full body", "Arms, \"upper\"""#),
    );
    schema_edit::apply(
        &catalog,
        &[Operation::AddField {
            table: "machines".to_owned(),
            field: FieldSpec {
                name: "muscle_group".to_owned(),
                type_name: "string".to_owned(),
                kind: DataFieldKind::Plain,
                attributes,
                ..FieldSpec::default()
            },
        }],
        &ApplyOptions::default(),
    )
    .await?;

    let table = catalog.require("machines")?;
    let provider = RestProvider::project("/api", &[table]);
    let client = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let union = r#""Chest" | "Full body" | "Arms, \"upper\"""#;
    assert!(
        client.contains(&format!("  muscle_group: {union} | null;")),
        "{client}"
    );
    assert!(
        client.contains(
            "export const MachinesOptions = {\n  muscle_group: [\"Chest\", \"Full body\", \"Arms, \\\"upper\\\"\"],\n} as const;"
        ),
        "{client}"
    );
    Ok(())
}
