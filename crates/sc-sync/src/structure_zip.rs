//! The zip transport (APP_SYNC_PROPOSAL §7.4): the structure tree as a zip,
//! for an instance with no link, or a snapshot taken by hand.
//!
//! `feldspar backup --structure-only` writes the files a commit would hold
//! into a zip, at its root ([`write_zip`]). `feldspar sync deploy --from
//! FILE.zip`, or an upload on the Source control screen, reads it back
//! ([`read_zip`]) and plans it with the same planner as the branch tip:
//!
//! - **With a link**, the zip takes the tip's place and the base rules apply:
//!   Deploy is refused while there are local changes, a deferred drop stays
//!   incoming, and the next base is stored.
//! - **Without a link** there is no base, so nothing says whether an entity
//!   the zip lacks was deleted or never there. The zip is applied
//!   **additively** ([`additive_target`]): it creates and updates, and never
//!   deletes — a backup restore's rule.

use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::path::Path;

use serde_json::{Map, Value as Json};

use sc_error::{Context, Error, Result};

use crate::diff::match_sides;
use crate::entity::{Content, Entities, Entity, Kind, put_entity, remove_entity};
use crate::format::FORMAT_FILE;
use crate::tree::StructureTree;

/// The most a structure zip may hold, uncompressed. A structure is
/// definitions and a few code and asset directories: a zip that unpacks to
/// more than this is something else, or a zip bomb.
pub const MAX_ZIP_BYTES: u64 = 1 << 30;

/// The manifest every backup zip has, and a structure zip has not: how a
/// backup handed to a structure deploy is told apart.
const BACKUP_MANIFEST: &str = "manifest.json";

/// Write `tree` into `out` as a zip: each of [`StructureTree::to_files`] at
/// its path, deflated, with no timestamps, so the same tree is the same zip.
/// Answers the zip's size.
pub fn write_zip<W: Write + Seek>(tree: &StructureTree, out: W) -> Result<u64> {
    let mut zip = zip::ZipWriter::new(out);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let failed = |e: &dyn std::fmt::Display| Error::file(format!("writing the structure zip: {e}"));
    for (path, bytes) in tree.to_files() {
        zip.start_file(path.as_str(), options)
            .map_err(|e| failed(&e))?;
        zip.write_all(&bytes).map_err(|e| failed(&e))?;
    }
    let mut out = zip.finish().map_err(|e| failed(&e))?;
    out.stream_position().map_err(|e| failed(&e))
}

/// `tree` as the bytes of a zip ([`write_zip`]).
pub fn zip_bytes(tree: &StructureTree) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    write_zip(tree, &mut out)?;
    Ok(out.into_inner())
}

/// Read the structure zip at `path`.
pub fn read_zip(path: &Path) -> Result<StructureTree> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::file(format!("reading `{}`: {e}", path.display())))?;
    read_zip_from(std::io::BufReader::new(file))
        .with_context(|| format!("reading `{}`", path.display()))
}

/// Read a structure zip from `reader`: every file in it, at its path, as a
/// tree ([`StructureTree::from_files`], which refuses a newer format). A zip
/// with no `feldspar-structure.json` is refused, saying what it looks like
/// instead.
pub fn read_zip_from<R: Read + Seek>(reader: R) -> Result<StructureTree> {
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| {
        Error::invalid(format!(
            "this is not a zip file ({e}); a structure zip is what `feldspar backup \
             --structure-only` writes"
        ))
    })?;
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    if !names.iter().any(|n| n == FORMAT_FILE) {
        return Err(Error::invalid(
            if names.iter().any(|n| n == BACKUP_MANIFEST) {
                "this is a backup, not a structure zip: restore it under Settings → Backup, or \
                 with `feldspar restore`"
                    .to_owned()
            } else {
                format!(
                    "this zip has no `{FORMAT_FILE}`, so it is not a structure zip; one is \
                     written by `feldspar backup --structure-only`"
                )
            },
        ));
    }
    let mut files = BTreeMap::new();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::invalid(format!("reading the structure zip: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        if entry.encrypted() {
            return Err(Error::invalid(format!(
                "`{}` in the zip is encrypted; a structure zip is never protected by a \
                 password",
                entry.name()
            )));
        }
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        let limit = MAX_ZIP_BYTES - total;
        (&mut entry)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::invalid(format!("reading `{name}` from the zip: {e}")))?;
        total += bytes.len() as u64;
        if total > MAX_ZIP_BYTES {
            return Err(Error::invalid(format!(
                "the zip unpacks to more than {} MB, which is more than a structure holds",
                MAX_ZIP_BYTES / 1_000_000
            )));
        }
        files.insert(name, bytes);
    }
    StructureTree::from_files(files)
}

/// The target of a zip deployed with no link (§7.4): live, with each entity
/// of `zip` put over the one it matches (§3.2) or added. What only live has
/// stays, and so does a field or a constraint of a table that the zip's
/// version of the table lacks, so planning it creates and updates and never
/// deletes.
pub fn additive_target(live: &StructureTree, zip: &StructureTree) -> StructureTree {
    let sides = [Entities::of(live), Entities::of(zip)];
    let mut target = live.clone();
    for group in match_sides(&sides) {
        let member = |side: usize| group.members[side].map(|i| &sides[side].list[i]);
        let Some(z) = member(1) else {
            continue;
        };
        let mut z = z.clone();
        if let Some(l) = member(0) {
            if matches!(z.kind, Kind::Table | Kind::Users) {
                keep_live_parts(l, &mut z);
            }
            remove_entity(&mut target, l);
        }
        put_entity(&mut target, &z);
    }
    if let Some(format) = zip.get(FORMAT_FILE) {
        target.insert_entry(FORMAT_FILE, format.clone());
    }
    target
}

/// Add to the zip's table `z` each field and managed constraint of live's
/// `l` that it has no match for: by id and then by name for a field, as the
/// planner pairs them, and by name for a constraint.
fn keep_live_parts(l: &Entity, z: &mut Entity) {
    let (Some(lv), Content::Json { value: zv }) =
        (l.json().and_then(Json::as_object), &mut z.content)
    else {
        return;
    };
    let Some(zv) = zv.as_object_mut() else {
        return;
    };
    let objects = |v: &Map<String, Json>, key: &str| -> Vec<Map<String, Json>> {
        v.get(key)
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_object)
            .cloned()
            .collect()
    };
    let text =
        |v: &Map<String, Json>, key: &str| v.get(key).and_then(Json::as_str).map(str::to_owned);

    let mut fields = objects(zv, "fields");
    let mut added = false;
    for field in objects(lv, "fields") {
        let matched = fields
            .iter()
            .any(|f| text(f, "id").is_some() && text(f, "id") == text(&field, "id"))
            || fields
                .iter()
                .any(|f| text(f, "name") == text(&field, "name"));
        if !matched {
            fields.push(field);
            added = true;
        }
    }
    if added {
        zv.insert(
            "fields".into(),
            Json::Array(fields.into_iter().map(Json::Object).collect()),
        );
    }

    let mut constraints = objects(zv, "constraints");
    let mut added = false;
    for constraint in objects(lv, "constraints") {
        let managed = constraint.get("managed").and_then(Json::as_bool) == Some(true);
        if managed
            && !constraints
                .iter()
                .any(|c| text(c, "name") == text(&constraint, "name"))
        {
            constraints.push(constraint);
            added = true;
        }
    }
    if added {
        zv.insert(
            "constraints".into(),
            Json::Array(constraints.into_iter().map(Json::Object).collect()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn format() -> Json {
        crate::format::StructureFormat::current().to_json()
    }

    #[test]
    fn a_tree_round_trips_through_its_zip_and_the_zip_is_stable() {
        let mut tree = StructureTree::new();
        tree.insert_json(FORMAT_FILE, format());
        tree.insert_json("roles.json", json!([{ "role": "admin", "id": 1 }]));
        tree.insert_file("file-stores/assets/files/logo.png", vec![0, 159, 146, 150]);
        let bytes = zip_bytes(&tree).unwrap();
        assert_eq!(bytes, zip_bytes(&tree).unwrap());
        let back = read_zip_from(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(back.to_files(), tree.to_files());
    }

    #[test]
    fn a_zip_that_is_not_a_structure_is_refused_saying_what_it_is() {
        let not_zip = read_zip_from(std::io::Cursor::new(b"hello".to_vec())).unwrap_err();
        assert!(not_zip.to_string().contains("not a zip"), "{not_zip}");

        let mut backup = StructureTree::new();
        backup.insert_json(BACKUP_MANIFEST, json!({ "source": "feldspar" }));
        let e = read_zip_from(std::io::Cursor::new(zip_bytes(&backup).unwrap())).unwrap_err();
        assert!(e.to_string().contains("feldspar restore"), "{e}");

        let mut other = StructureTree::new();
        other.insert_json("tables/x.json", json!({}));
        let e = read_zip_from(std::io::Cursor::new(zip_bytes(&other).unwrap())).unwrap_err();
        assert!(e.to_string().contains(FORMAT_FILE), "{e}");
    }

    #[test]
    fn the_additive_target_keeps_what_only_live_has() {
        let field = |id: &str, name: &str| json!({ "id": id, "name": name });
        let mut live = StructureTree::new();
        live.insert_json(
            "tables/tickets.json",
            json!({
                "id": "t1", "name": "tickets",
                "fields": [field("f1", "title"), field("f2", "status")],
                "constraints": [{ "name": "c_live", "managed": true }],
            }),
        );
        live.insert_json(
            "tables/notes.json",
            json!({ "id": "t2", "name": "notes", "fields": [] }),
        );
        live.insert_json("roles.json", json!([{ "role": "staff", "id": 10 }]));

        let mut zip = StructureTree::new();
        zip.insert_json(FORMAT_FILE, format());
        zip.insert_json(
            "tables/tickets.json",
            json!({
                "id": "t1", "name": "tickets", "description": "from the zip",
                "fields": [field("f1", "title"), field("f3", "priority")],
            }),
        );
        zip.insert_json(
            "tables/other.json",
            json!({ "id": "t3", "name": "other", "fields": [] }),
        );
        zip.insert_json("roles.json", json!([{ "role": "manager", "id": 20 }]));

        let target = additive_target(&live, &zip);
        let tickets = target.json("tables/tickets.json").unwrap();
        assert_eq!(tickets["description"], "from the zip");
        let names: Vec<&str> = tickets["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["title", "priority", "status"]);
        assert_eq!(tickets["constraints"][0]["name"], "c_live");
        assert!(target.json("tables/notes.json").is_some());
        assert!(target.json("tables/other.json").is_some());
        let roles: Vec<&str> = target
            .json("roles.json")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles.len(), 2, "{roles:?}");
        assert!(target.json(FORMAT_FILE).is_some());
    }
}
