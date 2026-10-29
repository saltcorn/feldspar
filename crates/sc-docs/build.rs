//! Expand the allowlist of `docs/` files against the directory and write the
//! `include_str!`s for the ones it matches, because `include_str!` takes one
//! literal path and cannot itself be given a pattern.
//!
//! Cargo reruns this whenever anything in `docs/` changes, so a new
//! `tutorial-*.md` is compiled in by the next build with nobody listing it.

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

/// The files of `docs/` agents read. `*` matches any run of characters.
const ALLOWLIST: &[&str] = &[
    "tutorial-*.md",
    "AGG_EXPRS.md",
    "CHECKLISTS.md",
    "GRAPHQL_API.md",
    "I18N.md",
    "OPERATIONS.md",
    "ROW_HISTORY.md",
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")?;
    let docs = Path::new(&manifest).join("../../docs");
    println!("cargo:rerun-if-changed={}", docs.display());

    let mut names: Vec<String> = std::fs::read_dir(&docs)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| ALLOWLIST.iter().any(|pattern| matches(pattern, name)))
        .collect();
    names.sort();

    let mut out = String::from(
        "/// The files of `docs/` agents read, by id — the file name without `.md`.\n\
         pub const FILES: &[(&str, &str)] = &[\n",
    );
    for name in &names {
        let id = name.strip_suffix(".md").unwrap_or(name);
        let path = docs.join(name);
        writeln!(
            out,
            "    ({id:?}, include_str!({:?})),",
            path.display().to_string()
        )?;
    }
    out.push_str("];\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR")?).join("files.rs"), out)?;
    Ok(())
}

/// Whether `name` matches `pattern`, in which `*` is any run of characters.
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, rest)) => {
            let Some(tail) = name.strip_prefix(head) else {
                return false;
            };
            (0..=tail.len())
                .filter(|&i| tail.is_char_boundary(i))
                .any(|i| matches(rest, &tail[i..]))
        }
    }
}
