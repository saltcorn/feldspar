//! Table fixtures for this crate's unit tests: the smallest table a filter can
//! be asked about, with no catalog and no database.

use sc_catalog::{AccessRules, DataField, DbId, Table, TableId, TableSource};
use sc_types::{Attrs, BasicType, TypeRef};

/// A table with `fields`, whose primary key is `id` when it has such a column.
pub fn table_of(name: &str, fields: Vec<DataField>) -> Table {
    let primary_key = fields
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.base.name.clone())
        .collect();
    Table {
        id: TableId(name.to_owned()),
        name: name.to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        primary_key,
        fields,
        label: name.to_owned(),
        description: String::new(),
        access: AccessRules::default(),
        attributes: Attrs::new(),
        overlay: None,
        ownership: None,
        ownership_error: None,
        rls_enabled: false,
        constraints: Vec::new(),
    }
}

/// A nullable text column.
pub fn plain_field(name: &str) -> DataField {
    DataField::plain(name, TypeRef::Basic(BasicType::Text))
}

/// A column of an explicit basic type.
pub fn typed_field(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

/// A required integer primary key.
pub fn id_field() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
}
