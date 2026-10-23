//! What a module's worker may reach: the permission set stored beside it, and
//! the sentence a denial is reported with (TODO "Modules in-process", phase 3;
//! specification §2).
//!
//! §15.1 used to end "Installing a module runs arbitrary code as the server …
//! There is no sandbox", and that sentence was a consequence of `node`, which
//! has no permission model to ask for. Deno does, so a module's worker is
//! handed a [`deno_permissions::PermissionsContainer`] built from *this* — a net
//! allow-list, a readable and a writable path list, and the environment
//! variables it may see.
//!
//! **Closed by default.** A module installed with nothing declared reaches
//! nothing: no host, no path, no variable. Widening it is an admin action on the
//! Modules tab, one entry at a time, and never a flag that quietly defaults
//! open — a permission set is only as good as what defaults to it.
//!
//! ## The two honesty constraints, kept here because they are easy to lose
//!
//! - **`npm install` is still unsandboxed.** Install scripts run as the server,
//!   before any worker exists. Nothing in this file changes that half, and the
//!   Modules tab says so beside the permissions rather than letting the presence
//!   of a permission screen imply otherwise.
//! - **Three capabilities are never granted at all**: subprocesses (`run`),
//!   native FFI, and dynamic `import` of anything off the disk. They have no
//!   entry here because there is no allow-list to put them on — a module that
//!   needs to run a program needs a different Saltcorn.
//!
//! ## What a closed module can still do, and why it must
//!
//! **Read its own code.** `require` walks the modules root, and a module that
//! may not read the package it is made of is a module that cannot exist. That
//! read is allowed by the require loader itself
//! ([`crate::deno`]'s `ensure_read_permission`), *not* by the container — so
//! `node:fs` reading the same directory is still denied. The distinction is
//! deliberate: the code a module is made of is not a capability, and everything
//! else on the filesystem is.
//!
//! **Read the environment — as nothing.** A denied `process.env.NODE_ENV` is
//! `undefined` rather than a throw, because half of npm reads a variable
//! speculatively at load time and turning that into a crash would be a rule that
//! most modules may not be installed. Deno's own `ignore` state is what does it.
//! A granted variable reads normally. Net, read and write denials are **errors**
//! by contrast, because there is something for an admin to do about each of
//! them and a silent failure would hide it.

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

/// The keys the stored JSON object uses, which are also the field names the API
/// and the Modules tab speak.
pub const PERM_NET: &str = "net";
/// Readable paths.
pub const PERM_READ: &str = "read";
/// Writable paths.
pub const PERM_WRITE: &str = "write";
/// Environment variables.
pub const PERM_ENV: &str = "env";
/// The roles the module may grant users (TODO-payments P3.3, design C4): one
/// entry, the **most privileged** role it may grant — `70` lets it grant 70 to
/// 99. Not a worker permission at all, but granted the way they are: by an
/// admin, on the install card and the Modules tab, never by the package.
pub const PERM_ROLES: &str = "roles";

/// The [`PERM_NET`] entry meaning *any host* — see [`check_host`].
///
/// Only `net` has one. A filesystem wildcard would be a module that may read the
/// database's password file, and an environment wildcard a module that may read
/// every secret this process was started with; neither has the excuse net has,
/// which is that the host is chosen after the grant.
pub const ANY_HOST: &str = "*";

/// What one module's worker may reach.
///
/// Every list is an allow-list and every empty list means *nothing*, never
/// *everything* — which is the one place this type could have gone wrong, and
/// the reason [`ModulePermissions::closed`] is what [`Default`] answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModulePermissions {
    /// Hosts the module may open a socket to: `broker.example` (any port),
    /// `broker.example:1883` (that port only), or `unix:/var/run/postgresql/
    /// .s.PGSQL.5432` (that Unix socket).
    pub net: Vec<String>,
    /// Absolute paths the module may read. A directory allows what is under it.
    pub read: Vec<String>,
    /// Absolute paths the module may write.
    pub write: Vec<String>,
    /// Environment variable names the module may see.
    pub env: Vec<String>,
    /// The most privileged role the module may grant (`ctx.roles.grant`), or
    /// `None` for no role at all. Never 1: no module may make an admin.
    pub roles: Option<u8>,
}

impl ModulePermissions {
    /// The set a module gets when nobody has granted it anything.
    pub fn closed() -> ModulePermissions {
        ModulePermissions::default()
    }

    /// Whether nothing at all is granted.
    pub fn is_closed(&self) -> bool {
        self.net.is_empty()
            && self.read.is_empty()
            && self.write.is_empty()
            && self.env.is_empty()
            && self.roles.is_none()
    }

    /// How many entries there are across every list — what a badge counts.
    pub fn granted(&self) -> usize {
        self.net.len()
            + self.read.len()
            + self.write.len()
            + self.env.len()
            + usize::from(self.roles.is_some())
    }

    /// The part of the set a **worker** is built with — everything but
    /// [`roles`](Self::roles), which the server checks when a module asks and
    /// the isolate never sees. What the pool pins by, so two modules that may
    /// reach the same hosts share a worker whatever roles they may grant.
    pub fn sandbox(&self) -> ModulePermissions {
        ModulePermissions {
            roles: None,
            ..self.clone()
        }
    }

    /// Whether the module may grant `role`: a role at or below its ceiling in
    /// privilege, and never the admin's or the public's.
    pub fn may_grant_role(&self, role: u8) -> bool {
        self.roles
            .is_some_and(|ceiling| role >= ceiling && role > 1 && role < 100)
    }

    /// Read a stored (or posted) permission set, checking every entry.
    ///
    /// Strict, in the way [`crate::store`] is strict about a row: an entry that
    /// is not a string, a path that is not absolute, a host with a scheme on it
    /// are each an [`Error::invalid`] naming the entry. A permission set nobody
    /// can read exactly is one that would be applied approximately.
    pub fn from_json(value: &Json) -> Result<ModulePermissions> {
        let object = match value {
            Json::Object(object) => object,
            Json::Null => return Ok(ModulePermissions::closed()),
            other => {
                return Err(Error::invalid(format!(
                    "a module's permissions should be an object, got {}",
                    kind_of(other)
                )));
            }
        };
        for key in object.keys() {
            if ![PERM_NET, PERM_READ, PERM_WRITE, PERM_ENV, PERM_ROLES].contains(&key.as_str()) {
                return Err(Error::invalid(format!(
                    "`{key}` is not a module permission; the permissions are {PERM_NET}, \
                     {PERM_READ}, {PERM_WRITE}, {PERM_ENV}, {PERM_ROLES}"
                )));
            }
        }
        let permissions = ModulePermissions {
            net: entries(object, PERM_NET, check_host)?,
            read: entries(object, PERM_READ, check_path)?,
            write: entries(object, PERM_WRITE, check_path)?,
            env: entries(object, PERM_ENV, check_env)?,
            roles: role_ceiling(object)?,
        };
        Ok(permissions)
    }

    /// The set as it is stored and as the API reports it.
    pub fn to_json(&self) -> Attrs {
        let mut object = Attrs::new();
        object.insert(PERM_NET.to_owned(), json!(self.net));
        object.insert(PERM_READ.to_owned(), json!(self.read));
        object.insert(PERM_WRITE.to_owned(), json!(self.write));
        object.insert(PERM_ENV.to_owned(), json!(self.env));
        // A list like the others, so one form edits all five: empty, or the
        // one ceiling as text.
        object.insert(
            PERM_ROLES.to_owned(),
            json!(self.roles.map(|r| vec![r.to_string()]).unwrap_or_default()),
        );
        object
    }

    /// Whether the net list grants *any* host — the `*` entry, which is not a
    /// host and must not be handed to a permission parser as one.
    pub fn any_host(&self) -> bool {
        self.net.iter().any(|entry| entry == ANY_HOST)
    }

    /// One sentence per granted capability, for a log line and for the admin's
    /// own reading. Empty when nothing is granted, so the caller says "nothing"
    /// in its own words rather than rendering an empty list.
    pub fn sentences(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.any_host() {
            lines.push("may connect to any host".to_owned());
        } else if !self.net.is_empty() {
            lines.push(format!("may connect to {}", self.net.join(", ")));
        }
        if !self.read.is_empty() {
            lines.push(format!("may read {}", self.read.join(", ")));
        }
        if !self.write.is_empty() {
            lines.push(format!("may write {}", self.write.join(", ")));
        }
        if !self.env.is_empty() {
            lines.push(format!("may read the variables {}", self.env.join(", ")));
        }
        if let Some(ceiling) = self.roles {
            lines.push(format!("may grant users roles {ceiling} to 99"));
        }
        lines
    }
}

/// One list of the object, checked entry by entry and sorted so that two sets
/// granting the same things are the *same* set.
///
/// Sorting is not tidiness: the pool pins a module to a worker by its permission
/// set ([`crate::deno`]), so two modules that were granted the same host in a
/// different order must share a worker rather than each getting one.
fn entries(
    object: &Attrs,
    key: &str,
    check: impl Fn(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let list = match value {
        Json::Array(list) => list,
        Json::Null => return Ok(Vec::new()),
        other => {
            return Err(Error::invalid(format!(
                "a module's `{key}` permission should be a list, got {}",
                kind_of(other)
            )));
        }
    };
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let Json::String(text) = entry else {
            return Err(Error::invalid(format!(
                "a module's `{key}` permission should be a list of strings, and one entry is {}",
                kind_of(entry)
            )));
        };
        out.push(check(text.trim())?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The `roles` entry: a list of at most one role number, the most privileged
/// the module may grant. Written as text by the form and as a number by a
/// manifest, so both are read.
fn role_ceiling(object: &Attrs) -> Result<Option<u8>> {
    let list = match object.get(PERM_ROLES) {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::Array(list)) => list,
        Some(other) => {
            return Err(Error::invalid(format!(
                "a module's `{PERM_ROLES}` permission should be a list, got {}",
                kind_of(other)
            )));
        }
    };
    let mut ceilings = Vec::new();
    for entry in list {
        let role = match entry {
            Json::Number(n) => n.as_u64(),
            Json::String(s) if s.trim().is_empty() => continue,
            Json::String(s) => s.trim().parse::<u64>().ok(),
            _ => None,
        };
        let Some(role) = role else {
            return Err(Error::invalid(format!(
                "`{entry}` is not a role number; the `{PERM_ROLES}` permission is the most \
                 privileged role the module may grant, like 70"
            )));
        };
        if role <= 1 {
            return Err(Error::invalid(
                "no module may grant the admin role (1); give it the most privileged role it \
                 may grant, like 70",
            ));
        }
        if role >= 100 {
            return Err(Error::invalid(format!(
                "role {role} cannot be granted: the public role is everybody's already, and \
                 roles run to 100"
            )));
        }
        ceilings.push(role as u8);
    }
    ceilings.sort_unstable();
    ceilings.dedup();
    match ceilings.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        _ => Err(Error::invalid(format!(
            "the `{PERM_ROLES}` permission is one role, the most privileged the module may \
             grant; it may then grant that role and every less privileged one"
        ))),
    }
}

/// A host, or a host and a port. Not a URL: what the permission checks match is
/// the socket's address, and `https://host/path` would silently match nothing.
fn check_host(entry: &str) -> Result<String> {
    if entry.is_empty() {
        return Err(Error::invalid("a network permission needs a host"));
    }
    // **Any host**, which is the one entry that is not a host at all. It exists
    // for the modules whose hosts are not knowable when the permission is
    // granted: an RSS table's feed is typed into the *table's* settings, not the
    // module's, so an allow-list of feed hosts would have to be edited every
    // time somebody adds a table. It is deliberately a thing an admin writes
    // out — one entry, spelled `*`, printed back as "any host" wherever the set
    // is shown — rather than a state an empty list could drift into.
    if entry == ANY_HOST {
        return Ok(entry.to_owned());
    }
    // A **Unix socket**, which Deno spells `unix:/path/to/socket` and which is
    // how a client on the same machine reaches a local server: `pg` connecting
    // to a Postgres on `/var/run/postgresql` opens one, and a table provider
    // over a local database is a real configuration (§8.3). It is a network
    // permission and not a filesystem one because that is where Deno checks it
    // — granting the *directory* for reading would grant far more.
    if let Some(path) = entry.strip_prefix("unix:") {
        if !std::path::Path::new(path).is_absolute() {
            return Err(Error::invalid(format!(
                "`{entry}` is not an absolute socket path, so it would mean a different socket \
                 depending on where the server was started"
            )));
        }
        return Ok(entry.to_owned());
    }
    if entry.contains("://") || entry.contains('/') {
        return Err(Error::invalid(format!(
            "`{entry}` is a URL; a network permission is a host or a host:port, like \
             `broker.example` or `broker.example:1883`"
        )));
    }
    // A trailing `:1883` is a port; a trailing `]` is an IPv6 address with no
    // port on it, which is a host and nothing more.
    if let Some((host, port)) = entry.rsplit_once(':')
        && !host.is_empty()
        && !host.ends_with(']')
    {
        match port.parse::<u16>() {
            Ok(0) | Err(_) => {
                return Err(Error::invalid(format!(
                    "`{port}` in `{entry}` is not a port number"
                )));
            }
            Ok(_) => {}
        }
    }
    Ok(entry.to_owned())
}

/// An absolute path, because a relative one means whatever directory the server
/// happened to start in — which is not a thing an admin can reason about.
fn check_path(entry: &str) -> Result<String> {
    if entry.is_empty() {
        return Err(Error::invalid("a filesystem permission needs a path"));
    }
    if !std::path::Path::new(entry).is_absolute() {
        return Err(Error::invalid(format!(
            "`{entry}` is not an absolute path, so it would mean a different directory \
             depending on where the server was started"
        )));
    }
    Ok(entry.to_owned())
}

/// A variable name.
fn check_env(entry: &str) -> Result<String> {
    if entry.is_empty() {
        return Err(Error::invalid("an environment permission needs a name"));
    }
    // Refused by name rather than accepted as a variable nobody has: `net` has
    // a wildcard ([`ANY_HOST`]) and this does not, and an admin who wrote `*`
    // here meant the one they had seen work on the line above.
    if entry == ANY_HOST {
        return Err(Error::invalid(
            "`*` is not an environment variable name; there is no wildcard here — name each \
             variable the module may read",
        ));
    }
    if entry.contains('=') || entry.contains('\0') {
        return Err(Error::invalid(format!(
            "`{entry}` is not an environment variable name"
        )));
    }
    Ok(entry.to_owned())
}

fn kind_of(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "a list",
        Json::Object(_) => "an object",
    }
}

/// Deno's own denial, rewritten as something an admin can act on.
///
/// A denial arrives as the module's own thrown error, from wherever inside its
/// dependency tree the socket or the file was reached for, and Deno words it for
/// somebody holding a command line: *Requires net access to "broker:1883", run
/// again with the --allow-net flag*. There is no command line here, so the
/// sentence names the screen instead — and says which list the entry goes on,
/// and what to type in it.
///
/// Returns `None` for anything that is not a permission denial, so the caller
/// leaves every other failure exactly as the module worded it.
pub fn explain_denial(module: &str, message: &str) -> Option<String> {
    // A net denial at **DNS lookup** time, which Deno's `node:dns` reports the
    // way node reports a failed `getaddrinfo`: `getaddrinfo EPERM <host>`. That
    // is the first socket call an HTTP client makes, so for anything built on
    // `http`/`https` (axios, node-fetch) it is the form a missing grant arrives
    // in. It carries no port, and the lookup is checked against the port the
    // request is for, so the remedy is the host alone, which allows every port.
    if let Some((_, rest)) = message.split_once("getaddrinfo EPERM ") {
        let host = rest.split_whitespace().next()?;
        return Some(format!(
            "the module `{module}` was denied net access to \"{host}\": add {host} to its \
             network allow-list in Settings → Modules → {module} → Permissions (the host \
             alone, with no port, allows every port)"
        ));
    }
    // `Requires <access>, run again with the --allow-<name> flag`, or the
    // `deno compile` variant of the same sentence.
    let (before, after) = message.split_once(", run again with the --allow-")?;
    // From "Requires " rather than from the start of the string: what arrives
    // here is an `Error`'s own `Display`, which has already put its kind in
    // front of the module's words.
    let at = before.rfind("Requires ")?;
    let access = before[at + "Requires ".len()..].trim();
    let name = after.split_whitespace().next()?.trim_end_matches("flag");
    let name = name.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
    let what = access.rsplit_once(" to ").map_or(access, |(_, what)| what);
    let remedy = match name {
        PERM_NET => format!(
            "add {what} to its network allow-list in Settings → Modules → {module} → Permissions"
        ),
        PERM_READ => format!(
            "add {what} to its readable paths in Settings → Modules → {module} → Permissions"
        ),
        PERM_WRITE => format!(
            "add {what} to its writable paths in Settings → Modules → {module} → Permissions"
        ),
        PERM_ENV => format!(
            "add {what} to its environment variables in Settings → Modules → {module} → \
             Permissions"
        ),
        other => format!(
            "Saltcorn never grants a module {other} access, so this module cannot do that on \
             this server"
        ),
    };
    Some(format!(
        "the module `{module}` was denied {access}: {remedy}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_declared_is_nothing_granted() {
        let permissions = ModulePermissions::from_json(&json!({})).unwrap();
        assert!(permissions.is_closed());
        assert_eq!(permissions, ModulePermissions::closed());
        assert_eq!(ModulePermissions::default(), ModulePermissions::closed());
        // And a row that has never been written reads as closed rather than as
        // an error: a module installed before this column existed is denied,
        // not broken.
        assert!(
            ModulePermissions::from_json(&Json::Null)
                .unwrap()
                .is_closed()
        );
    }

    #[test]
    fn a_set_round_trips_through_its_stored_json() {
        let permissions = ModulePermissions {
            net: vec!["broker.example:1883".into()],
            read: vec!["/srv/data".into()],
            write: Vec::new(),
            env: vec!["MQTT_PASSWORD".into()],
            roles: Some(70),
        };
        let stored = Json::Object(permissions.to_json());
        assert_eq!(ModulePermissions::from_json(&stored).unwrap(), permissions);
    }

    #[test]
    fn a_role_ceiling_is_one_role_never_the_admins_and_not_the_workers() {
        let set = ModulePermissions::from_json(&json!({ "roles": ["70"] })).unwrap();
        assert_eq!(set.roles, Some(70));
        assert_eq!(
            ModulePermissions::from_json(&json!({ "roles": [70] }))
                .unwrap()
                .roles,
            Some(70),
            "a manifest writes it as a number"
        );
        assert!(set.may_grant_role(70) && set.may_grant_role(99));
        assert!(!set.may_grant_role(60), "above the ceiling");
        assert!(!set.may_grant_role(1) && !set.may_grant_role(100));
        assert!(!ModulePermissions::closed().may_grant_role(80));
        assert_eq!(set.sentences(), ["may grant users roles 70 to 99"]);
        assert!(!set.is_closed());
        // The worker never sees it, so it does not split the pool.
        assert!(set.sandbox().is_closed());
        for bad in [
            json!(["1"]),
            json!(["100"]),
            json!(["70", "80"]),
            json!(["admin"]),
        ] {
            assert!(
                ModulePermissions::from_json(&json!({ "roles": bad })).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            ModulePermissions::from_json(&json!({ "roles": [""] }))
                .unwrap()
                .roles,
            None,
            "a blank line in the form is no ceiling"
        );
    }

    #[test]
    fn two_sets_granting_the_same_things_are_one_set() {
        // The pool pins by permission set, so this is the difference between two
        // modules sharing a worker and each getting one.
        let one = ModulePermissions::from_json(&json!({ "net": ["a:1", "b:2"] })).unwrap();
        let other = ModulePermissions::from_json(&json!({ "net": ["b:2", "a:1", "a:1"] })).unwrap();
        assert_eq!(one, other);
    }

    #[test]
    fn a_url_is_not_a_network_permission() {
        let err = ModulePermissions::from_json(&json!({ "net": ["https://broker.example"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("host:port"), "{err}");
    }

    /// A Unix socket is a network permission, because that is where Deno checks
    /// it — and a local Postgres a table provider points at is reached through
    /// one (§8.3).
    #[test]
    fn a_unix_socket_is_a_network_permission() {
        let permissions = ModulePermissions::from_json(
            &json!({ "net": ["unix:/var/run/postgresql/.s.PGSQL.5432"] }),
        )
        .unwrap();
        assert_eq!(permissions.net, ["unix:/var/run/postgresql/.s.PGSQL.5432"]);
        // …and it round-trips, so an admin who typed it once still has it.
        let stored = Json::Object(permissions.to_json());
        assert_eq!(ModulePermissions::from_json(&stored).unwrap(), permissions);

        // A relative socket path means a different socket depending on where the
        // server started, exactly as a relative file path does.
        let err = ModulePermissions::from_json(&json!({ "net": ["unix:run/pg.sock"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn a_relative_path_is_refused_because_it_names_nowhere_in_particular() {
        let err = ModulePermissions::from_json(&json!({ "read": ["data"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn an_unknown_permission_names_the_ones_there_are() {
        let err = ModulePermissions::from_json(&json!({ "run": ["ls"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("run"), "{err}");
        assert!(err.contains("net") && err.contains("env"), "{err}");
    }

    #[test]
    fn a_bad_port_says_so() {
        let err = ModulePermissions::from_json(&json!({ "net": ["broker:eighteen"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("port"), "{err}");
    }

    #[test]
    fn a_denial_names_the_module_the_capability_and_the_screen() {
        let sentence = explain_denial(
            "@saltcorn/mqtt",
            "Requires net access to \"broker.example:1883\", run again with the --allow-net flag",
        )
        .expect("a net denial should be recognised");
        assert!(sentence.contains("@saltcorn/mqtt"), "{sentence}");
        assert!(sentence.contains("broker.example:1883"), "{sentence}");
        assert!(sentence.contains("network allow-list"), "{sentence}");
        assert!(sentence.contains("Settings → Modules"), "{sentence}");
        // And nothing that reads as a command line an admin does not have.
        assert!(!sentence.contains("--allow-net"), "{sentence}");
    }

    #[test]
    fn a_denial_is_recognised_after_an_error_has_put_its_kind_in_front() {
        // What reaches this function is an `Error`'s `Display`, not the raw
        // throw: "configuration error: Requires …". Recognising only the raw
        // form would mean every real denial reached the admin as Deno's
        // command-line sentence.
        let sentence = explain_denial(
            "@saltcorn/mqtt",
            "configuration error: Requires net access to \"127.0.0.1:1883\", run again with the \
             --allow-net flag",
        )
        .expect("a wrapped net denial should be recognised");
        assert!(sentence.contains("127.0.0.1:1883"), "{sentence}");
    }

    #[test]
    fn a_capability_with_no_allow_list_says_it_is_never_granted() {
        let sentence = explain_denial(
            "@saltcorn/mqtt",
            "Requires run access to \"ls\", run again with the --allow-run flag",
        )
        .expect("a run denial should be recognised");
        assert!(sentence.contains("never grants"), "{sentence}");
    }

    #[test]
    fn a_denied_dns_lookup_is_a_net_denial_naming_the_host() {
        // What `@saltcorn/nominatim-geocode` (axios over `node:http`) throws
        // with no grant, or with a grant for `:443` only when the first request
        // is `http://`: Deno's `node:dns` words the denial as node would.
        let sentence = explain_denial(
            "@saltcorn/nominatim-geocode",
            "configuration error: getaddrinfo EPERM nominatim.openstreetmap.org",
        )
        .expect("a denied lookup should be recognised");
        assert!(
            sentence.contains("@saltcorn/nominatim-geocode"),
            "{sentence}"
        );
        assert!(
            sentence.contains("add nominatim.openstreetmap.org to"),
            "{sentence}"
        );
        assert!(sentence.contains("network allow-list"), "{sentence}");
        assert!(!sentence.contains("EPERM"), "{sentence}");
        // A lookup that failed for any other reason is the module's to word.
        assert!(explain_denial("m", "getaddrinfo ENOTFOUND nowhere.invalid").is_none());
    }

    #[test]
    fn anything_that_is_not_a_denial_is_left_alone() {
        assert!(explain_denial("@saltcorn/mqtt", "connect ECONNREFUSED").is_none());
    }

    #[test]
    fn the_granted_capabilities_read_as_sentences() {
        let permissions = ModulePermissions {
            net: vec!["broker.example:1883".into()],
            read: Vec::new(),
            write: Vec::new(),
            env: vec!["HOME".into()],
            roles: None,
        };
        let lines = permissions.sentences();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("broker.example:1883"));
        assert_eq!(permissions.granted(), 2);
        assert!(ModulePermissions::closed().sentences().is_empty());
    }

    #[test]
    fn any_host_is_a_net_entry_and_reads_as_one_sentence() {
        let permissions = ModulePermissions::from_json(&json!({ "net": ["*"] })).unwrap();
        assert!(permissions.any_host());
        assert_eq!(permissions.net, [ANY_HOST]);
        assert_eq!(permissions.sentences(), ["may connect to any host"]);
        // It is a grant like any other, so it counts and it is not "closed".
        assert_eq!(permissions.granted(), 1);
        assert!(!permissions.is_closed());
        // And a set without it never claims it, however many hosts it names.
        let named =
            ModulePermissions::from_json(&json!({ "net": ["a.example", "b.example"] })).unwrap();
        assert!(!named.any_host());
        assert_eq!(named.sentences(), ["may connect to a.example, b.example"]);
    }

    #[test]
    fn the_wildcard_is_the_net_lists_alone() {
        // A filesystem or environment wildcard has no meaning to give it: `*`
        // is not an absolute path and not a variable name, so the existing
        // checks refuse it without a special case.
        for key in [PERM_READ, PERM_WRITE] {
            let err = ModulePermissions::from_json(&json!({ key: ["*"] }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("absolute path"), "{key}: {err}");
        }
        assert!(ModulePermissions::from_json(&json!({ PERM_ENV: ["*"] })).is_err());
    }
}
