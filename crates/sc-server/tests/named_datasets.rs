//! Test support: model bodies written with their dataset inline.
//!
//! A model's dataset is a named dataset now, referenced by id (analytics TODO
//! A1.8), and the API refuses anything else. The model tests were written when
//! a dataset was a column list on the model, and that shape is still the
//! clearest way to say what a test fits. So each test client passes a body
//! through [`inline_datasets`] before it is sent: every inline dataset in it —
//! `dataset`, and each related dataset's — becomes a `createDataset` call, and
//! [`use_ids`] puts the ids in their place.
//!
//! The dataset is the one `DatasetDef::from_columns` builds: a Calculated
//! column per column, the filter, the order, and a Select columns keeping
//! exactly those. It is what the migration makes of a stored model too.

use sc_dataset::DatasetDef;
use serde_json::{Value, json};

/// Every inline dataset in `body`, as the pointer its reference goes to and
/// the `createDataset` body that makes it.
pub fn inline_datasets(body: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Some(ds) = body.get("dataset")
        && ds.get("table").is_some()
    {
        out.push(("/dataset".to_owned(), create_body(ds)));
    }
    if let Some(Value::Array(related)) = body.get("related") {
        for (i, r) in related.iter().enumerate() {
            if let Some(ds) = r.get("dataset")
                && ds.get("table").is_some()
            {
                out.push((format!("/related/{i}"), create_body(ds)));
            }
        }
    }
    out
}

/// Replace each inline dataset with the id it was created under.
pub fn use_ids(body: &mut Value, ids: Vec<(String, String)>) {
    for (pointer, id) in ids {
        if pointer == "/dataset" {
            body["dataset"] = json!({ "dataset_id": id });
        } else if let Some(Value::Object(item)) = body.pointer_mut(&pointer) {
            item.remove("dataset");
            item.insert("dataset_id".to_owned(), json!(id));
        }
    }
}

/// The `createDataset` body for one inline dataset, under a fresh name.
pub fn create_body(inline: &Value) -> Value {
    let table = inline["table"].as_str().unwrap_or_default().to_owned();
    let columns: Vec<(String, String)> = inline["columns"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    (
                        c["name"].as_str().unwrap_or_default().to_owned(),
                        c["expr"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let order: Vec<(String, bool)> = inline["order"]
        .as_array()
        .map(|os| {
            os.iter()
                .map(|o| {
                    (
                        o["expr"].as_str().unwrap_or_default().to_owned(),
                        o["descending"].as_bool().unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let cols: Vec<(&str, &str)> = columns
        .iter()
        .map(|(n, e)| (n.as_str(), e.as_str()))
        .collect();
    let ord: Vec<(&str, bool)> = order.iter().map(|(e, d)| (e.as_str(), *d)).collect();
    let name = format!("{table} {}", &uuid::Uuid::new_v4().to_string()[..8]);
    let def = DatasetDef::from_columns(name.clone(), table, &cols, inline["filter"].as_str(), &ord);
    json!({
        "name": name,
        "base": def.base,
        "operations": def.operations,
    })
}

/// Whether a request to `path` carries a model body.
pub fn carries_a_model(method: &str, path: &str) -> bool {
    method != "GET"
        && (path == "/api/models"
            || path.starts_with("/api/model-data/")
            || path.starts_with("/api/model-datasets/")
            || path.starts_with("/api/model-bindings/"))
}
