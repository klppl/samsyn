//! What a ticket lets its holder write. A grant is read & write, read-only, or read-only with a
//! list of share-relative paths that stay writable (`rw_paths`): `Inbox/` is a folder, `todo.md`
//! a note. The effective access for one key is "writable" when the grant is read & write or the
//! key lies under one of those paths. Everything that enforces access (bridges, guest sockets,
//! p2p links, the headless client) asks this one type.

use crate::ids::valid_key;

/// Upper bound on writable paths per ticket; plenty for a vault, small enough for a ticket.
pub const MAX_RW_PATHS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// Read & write everywhere; `rw_paths` is irrelevant then.
    pub base_rw: bool,
    /// Share-relative paths that are writable on a read-only grant. Folders end with `/`.
    pub rw_paths: Vec<String>,
}

impl Access {
    pub fn full() -> Self {
        Access { base_rw: true, rw_paths: Vec::new() }
    }

    pub fn read_only() -> Self {
        Access { base_rw: false, rw_paths: Vec::new() }
    }

    /// From a stored grant or mount: `access` is `rw` or `ro`; anything else counts as `ro`.
    pub fn new(access: &str, rw_paths: &[String]) -> Self {
        if access == "rw" {
            Access::full()
        } else {
            Access { base_rw: false, rw_paths: rw_paths.to_vec() }
        }
    }

    /// May this share-relative key be written?
    pub fn writable(&self, share_key: &str) -> bool {
        self.base_rw || self.rw_paths.iter().any(|p| if p.ends_with('/') { share_key.starts_with(p.as_str()) } else { share_key == p })
    }

    /// Can anything at all be written? Decides whether blob uploads are accepted and which
    /// message size cap applies to a connection.
    pub fn any_write(&self) -> bool {
        self.base_rw || !self.rw_paths.is_empty()
    }

    /// Read-only with some writable paths.
    pub fn is_mixed(&self) -> bool {
        !self.base_rw && !self.rw_paths.is_empty()
    }

    /// The `rw`/`ro` string stored in the registry and shown in tickets.
    pub fn label(&self) -> &'static str {
        if self.base_rw { "rw" } else { "ro" }
    }
}

/// Validate and normalise writable paths typed by the owner, relative to the share. A path
/// that is an existing key (`exists`) or that does not end in `/` and exists is a note; anything
/// else is a folder and gets a trailing `/`, so a folder that does not exist yet can be made
/// writable in advance. Entries nested under another folder entry are dropped as redundant.
pub fn clean_rw_paths(raw: &[String], only_file: bool, exists: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let trimmed = r.trim().replace('\\', "/");
        let folder_hint = trimmed.ends_with('/');
        let p = trimmed.trim_matches('/');
        if p.is_empty() {
            continue;
        }
        if only_file {
            return Err("writable paths only apply to shared folders, not to a single shared note".into());
        }
        if !valid_key(p) || p.split('/').any(|seg| seg.starts_with('.')) {
            return Err(format!("writable path '{p}' contains '..', hidden, empty or control-character segments"));
        }
        let entry = if !folder_hint && exists(p) { p.to_string() } else { format!("{p}/") };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    if out.len() > MAX_RW_PATHS {
        return Err(format!("at most {MAX_RW_PATHS} writable paths per ticket"));
    }
    // Drop entries covered by a folder entry (keeps the list canonical and short).
    let folders: Vec<String> = out.iter().filter(|p| p.ends_with('/')).cloned().collect();
    out.retain(|p| !folders.iter().any(|f| f != p && p.starts_with(f.as_str())));
    out.sort();
    Ok(out)
}

/// Writable paths received from another server (a `HelloReply`): keep only well-formed ones,
/// as given, never trusting the peer with the file system.
pub fn accept_rw_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| {
            let bare = p.trim_end_matches('/');
            !bare.is_empty() && valid_key(bare) && !bare.split('/').any(|seg| seg.starts_with('.')) && p.matches('/').count() == bare.matches('/').count() + usize::from(p.ends_with('/'))
        })
        .take(MAX_RW_PATHS)
        .cloned()
        .collect()
}

/// Human-readable summary for logs and UIs: `rw`, `ro`, or `ro (+Inbox/, todo.md)`.
pub fn describe(access: &str, rw_paths: &[String]) -> String {
    if access == "rw" || rw_paths.is_empty() {
        access.to_string()
    } else {
        format!("{access} (+{})", rw_paths.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn full_access_writes_everything() {
        let a = Access::new("rw", &s(&["ignored/"]));
        assert!(a.writable("anything.md") && a.any_write() && !a.is_mixed());
        assert_eq!(a.label(), "rw");
    }

    #[test]
    fn read_only_writes_nothing() {
        let a = Access::new("ro", &[]);
        assert!(!a.writable("a.md") && !a.any_write() && !a.is_mixed());
        assert!(!Access::new("garbage", &[]).writable("a.md"), "unknown access strings are read-only");
    }

    #[test]
    fn mixed_access_follows_paths() {
        let a = Access::new("ro", &s(&["Inbox/", "todo.md"]));
        assert!(a.is_mixed() && a.any_write());
        assert!(a.writable("Inbox/x.md") && a.writable("Inbox/deep/y.png") && a.writable("todo.md"));
        assert!(!a.writable("Inbox") && !a.writable("Inbox.md") && !a.writable("todo.md.bak") && !a.writable("Notes/a.md"));
    }

    #[test]
    fn clean_normalises_and_classifies() {
        let exists = |k: &str| k == "todo.md" || k == "Inbox/a.md";
        let out = clean_rw_paths(&s(&[" /Inbox/ ", "todo.md", "Later", "Inbox/a.md", "", "Inbox"]), false, exists).unwrap();
        assert_eq!(out, s(&["Inbox/", "Later/", "todo.md"]), "nested Inbox/a.md dropped, Later is a future folder");
        assert!(clean_rw_paths(&s(&["../x"]), false, |_| false).is_err());
        assert!(clean_rw_paths(&s(&[".obsidian/"]), false, |_| false).is_err());
        assert!(clean_rw_paths(&s(&["Inbox/"]), true, |_| false).is_err(), "single-note shares take no rw paths");
        assert!(clean_rw_paths(&s(&[]), true, |_| false).unwrap().is_empty(), "empty list is fine for a note");
        let many: Vec<String> = (0..MAX_RW_PATHS + 1).map(|i| format!("f{i}/")).collect();
        assert!(clean_rw_paths(&many, false, |_| false).is_err());
    }

    #[test]
    fn accept_filters_peer_input() {
        let out = accept_rw_paths(&s(&["Inbox/", "todo.md", "../x", "/abs/", "a//b", ".hidden/", "ok/sub/"]));
        assert_eq!(out, s(&["Inbox/", "todo.md", "ok/sub/"]));
    }

    #[test]
    fn describe_is_compact() {
        assert_eq!(describe("rw", &s(&["x/"])), "rw");
        assert_eq!(describe("ro", &[]), "ro");
        assert_eq!(describe("ro", &s(&["Inbox/", "todo.md"])), "ro (+Inbox/, todo.md)");
    }
}
