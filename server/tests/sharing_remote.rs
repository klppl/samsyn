//! Server-to-server sharing over iroh with relays disabled (direct loopback connection):
//! owner on server A, bob and carol on server B. Mirrors server/scripts/remote-e2e.sh.

mod common;

use common::*;
use serde_json::json;

#[test]
fn p2p_mount_edit_revoke_restart() {
    let dir = tmp("remote");
    let pa = free_port().to_string();
    let pb = free_port().to_string();
    let mut sa = Server::start(&dir, "a", &["--relay", "none", "--p2p-port", &pa]);
    let mut sb = Server::start(&dir, "b", &["--relay", "none", "--p2p-port", &pb]);
    assert_ne!(sa.node_id(), sb.node_id());
    let owner = sa.owner_token.clone();
    let bob_tok = sb.owner_token.clone();
    let carol_tok = sb.add_user("carol");

    let a = Client::start(&sa, &owner, "va", &dir.join("A"), "A");
    a.write("Projects/X/a.md", "a-content\n");
    a.write("Projects/X/sub/b.md", "b-content\n");
    a.write("private.md", "secret\n");
    a.write_bytes("Projects/X/pic.png", b"\x89PNG fake");
    let b = Client::start(&sb, &bob_tok, "vb", &dir.join("B"), "B");
    eventually!(15, a.proc.log_contains("reconcile done") && b.proc.log_contains("reconcile done"), "clients up", a.proc, b.proc);

    let r = api_json(&sa, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let share_id = r["share"]["id"].as_str().expect("share id").to_string();
    let ticket = r["ticket"].as_str().expect("ticket").to_string();

    // Garbage is refused fast; a real ticket is probed against the owner before anything is stored.
    let (code, _) = api_raw(&sb, &bob_tok, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": "replicaxxxx", "mount_path": "Shared/X"})));
    assert_eq!(code, 400);
    let r = api_json(&sb, &bob_tok, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/X"})));
    assert_eq!(r["access"], "rw", "mount: {r}");
    eventually!(20, b.exists("Shared/X/a.md") && b.exists("Shared/X/pic.png"), "bob gets files over p2p", b.proc, sb.proc, sa.proc);
    assert_eq!(b.read("Shared/X/a.md"), "a-content\n");
    assert!(!b.exists("private.md"));
    assert!(b.same_bytes("Shared/X/pic.png", &a.path("Projects/X/pic.png")), "blob over p2p");
    let info = api_json(&sb, &bob_tok, "GET", "/api/vaults/vb/sharing", None);
    assert_eq!(info["mounts"][0]["status"], "remote: connected", "{info}");

    b.append("Shared/X/a.md", "edited-by-bob");
    eventually!(10, a.contains("Projects/X/a.md", "edited-by-bob"), "owner sees bob", a.proc, sa.proc);
    a.append("Projects/X/sub/b.md", "edited-by-owner");
    eventually!(10, b.contains("Shared/X/sub/b.md", "edited-by-owner"), "bob sees owner", b.proc, sb.proc);
    a.write("Projects/X/a.md", &format!("TOP\n{}", a.read("Projects/X/a.md")));
    b.append("Shared/X/a.md", "BOTTOM");
    eventually!(
        10,
        a.read("Projects/X/a.md") == b.read("Shared/X/a.md") && a.contains("Projects/X/a.md", "TOP") && a.contains("Projects/X/a.md", "BOTTOM"),
        "concurrent edits merge",
        a.proc,
        b.proc
    );
    b.write("Shared/X/new.md", "from bob\n");
    b.write_bytes("Shared/X/bob.gif", b"GIF89a bob");
    eventually!(20, a.read("Projects/X/new.md") == "from bob\n" && a.same_bytes("Projects/X/bob.gif", &b.path("Shared/X/bob.gif")), "owner gets bob's files (blob fetched over the link)", a.proc, sa.proc);
    a.remove("Projects/X/sub/b.md");
    eventually!(10, !b.exists("Shared/X/sub/b.md"), "delete propagates", b.proc);

    // Carol on server B with a read-only ticket.
    let ro = api_json(&sa, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "carol", "access": "ro"})));
    let ro_ticket = ro["ticket"].as_str().unwrap().to_string();
    let c = Client::start(&sb, &carol_tok, "vc", &dir.join("C"), "C");
    eventually!(10, c.proc.log_contains("reconcile done"), "carol up", c.proc);
    let r = api_json(&sb, &carol_tok, "POST", "/api/vaults/vc/mounts", Some(json!({"ticket": ro_ticket})));
    assert_eq!(r["access"], "ro", "carol mount: {r}");
    eventually!(20, c.contains("Shared/X/a.md", "TOP") && c.exists("Shared/X/pic.png"), "carol gets files", c.proc, sb.proc);
    c.append("Shared/X/a.md", "carol-edit");
    sleep(3.0);
    assert!(!a.contains("Projects/X/a.md", "carol-edit"));
    eventually!(5, !c.contains("Shared/X/a.md", "carol-edit"), "ro edit reverted", c.proc);

    // Revoke carol on A: B's link is denied and the mount turns revoked.
    let (code, _) = api_raw(&sa, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/carol"), None);
    assert_eq!(code, 204);
    eventually!(
        60,
        api_json(&sb, &carol_tok, "GET", "/api/vaults/vc/sharing", None)["mounts"][0]["status"] == "revoked",
        "carol's mount revoked on B",
        sb.proc,
        sa.proc
    );
    a.append("Projects/X/a.md", "after-revoke");
    eventually!(10, b.contains("Shared/X/a.md", "after-revoke"), "bob still receives", b.proc);
    sleep(1.0);
    assert!(!c.contains("Shared/X/a.md", "after-revoke"));
    assert!(c.exists("Shared/X/a.md"));

    // Restart B: link reconnects.
    sb.restart(&["--relay", "none", "--p2p-port", &pb]);
    a.append("Projects/X/a.md", "while-b-down");
    eventually!(30, b.contains("Shared/X/a.md", "while-b-down"), "edit flows after B restart", b.proc, sb.proc);
    // Restart A: an edit made meanwhile arrives afterwards (fixed p2p port keeps the address valid).
    sa.proc.kill();
    b.append("Shared/X/a.md", "while-a-down");
    sleep(1.0);
    sa.restart(&["--relay", "none", "--p2p-port", &pa]);
    eventually!(60, a.contains("Projects/X/a.md", "while-a-down"), "offline edit arrives after A restart", a.proc, sa.proc, sb.proc);
    a.append("Projects/X/a.md", "after-a-restart");
    eventually!(30, b.contains("Shared/X/a.md", "after-a-restart"), "edit flows after A restart", b.proc, sb.proc);

    // Unmount removes the replica, keeps files.
    let info = api_json(&sb, &bob_tok, "GET", "/api/vaults/vb/sharing", None);
    let mid = info["mounts"][0]["id"].as_str().unwrap().to_string();
    let (code, _) = api_raw(&sb, &bob_tok, "DELETE", &format!("/api/mounts/{mid}"), None);
    assert_eq!(code, 204);
    assert!(b.exists("Shared/X/a.md"));
    let replica_dirs: Vec<_> = walkdir::WalkDir::new(&sb.data)
        .into_iter()
        .flatten()
        .filter(|e| e.path().ends_with(&mid))
        .collect();
    assert!(replica_dirs.is_empty(), "replica dir still there");

    for s in [&sa, &sb] {
        let errs: Vec<_> = s.errors().into_iter().filter(|e| !e.contains("p2p link failed")).collect();
        assert!(errs.is_empty(), "server errors: {errs:?}");
    }
}
