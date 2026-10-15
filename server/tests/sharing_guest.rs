//! Guest mode: owner's server runs with --public-url; two headless guests join with tickets
//! only (rw and ro). Mirrors server/scripts/guest-e2e.sh.

mod common;

use common::*;
use serde_json::json;

#[test]
fn guests_join_with_tickets_only() {
    let dir = tmp("guest");
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let mut server = Server::start_at(dir.join("data"), dir.join("server.log"), port, &["--no-p2p", "--public-url", &url]);
    let owner = server.owner_token.clone();

    let o = Client::start(&server, &owner, "va", &dir.join("O"), "O");
    o.write("Projects/X/a.md", "a-content\n");
    o.write("Projects/X/sub/b.md", "b-content\n");
    o.write("private.md", "secret\n");
    o.write_bytes("Projects/X/pic.png", b"\x89PNG fake");
    eventually!(15, o.proc.log_contains("reconcile done"), "owner up", o.proc);

    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let share_id = r["share"]["id"].as_str().expect("share id").to_string();
    let ticket = r["ticket"].as_str().expect("ticket").to_string();
    let ro = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "guest-ro", "access": "ro"})));
    let ro_ticket = ro["ticket"].as_str().unwrap().to_string();
    let inspect = api_json(&server, &owner, "POST", "/api/tickets/inspect", Some(json!({"ticket": ticket})));
    assert_eq!(inspect["url"], url, "ticket carries the public url");

    // Auth hygiene: wrong secret and unknown share answer alike; too many failures -> 429.
    let bad = "ab".repeat(32);
    let (c1, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    let (c2, _) = api_raw(&server, "", "GET", &format!("/guest/shares/nope?secret={bad}"), None);
    assert_eq!((c1, c2), (403, 403));
    for _ in 0..9 {
        api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    }
    let (c3, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    assert_eq!(c3, 429);
    assert!(!server.proc.log_contains("secret="), "secret leaked into the log");
    // The limiter is per IP and the guests below come from loopback too: restart to clear it.
    server.restart(&["--no-p2p", "--public-url", &url]);

    let g1 = Client::start_guest(&ticket, &dir.join("G1"), "G1");
    let g2 = Client::start_guest(&ro_ticket, &dir.join("G2"), "G2");
    eventually!(20, g1.exists("a.md") && g1.exists("pic.png") && g2.exists("a.md"), "guests get files", g1.proc, g2.proc, server.proc);
    assert_eq!(g1.read("a.md"), "a-content\n");
    assert!(g1.exists("sub/b.md"));
    assert!(g1.same_bytes("pic.png", &o.path("Projects/X/pic.png")));
    assert!(!g1.exists("private.md"));
    let info = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    let seen: Vec<bool> = info["shares"][0]["grants"].as_array().unwrap().iter().map(|g| !g["last_seen_at"].is_null()).collect();
    assert_eq!(seen, vec![true, true], "last_seen_at: {info}");

    g1.append("a.md", "edited-by-guest");
    eventually!(10, o.contains("Projects/X/a.md", "edited-by-guest"), "owner sees guest edit", o.proc, server.proc);
    o.append("Projects/X/sub/b.md", "edited-by-owner");
    eventually!(10, g1.contains("sub/b.md", "edited-by-owner") && g2.contains("sub/b.md", "edited-by-owner"), "guests see owner edit", g1.proc, g2.proc);
    g1.write("new.md", "from guest\n");
    g1.write_bytes("guest.gif", b"GIF89a guest");
    eventually!(15, o.read("Projects/X/new.md") == "from guest\n" && o.same_bytes("Projects/X/guest.gif", &g1.path("guest.gif")), "owner gets guest's files", o.proc, server.proc);
    eventually!(10, g2.same_bytes("guest.gif", &g1.path("guest.gif")), "ro guest gets the blob", g2.proc);

    // Read-only guest.
    eventually!(30, std::fs::metadata(g2.path("a.md")).map(|m| m.permissions().readonly()).unwrap_or(false), "ro files read-only on disk", g2.proc);
    let mut p = std::fs::metadata(g2.path("a.md")).unwrap().permissions();
    p.set_readonly(false);
    std::fs::set_permissions(g2.path("a.md"), p).unwrap();
    g2.append("a.md", "ro-edit");
    sleep(3.0);
    assert!(!o.contains("Projects/X/a.md", "ro-edit"));
    eventually!(5, !g2.contains("a.md", "ro-edit"), "ro edit restored", g2.proc);
    g2.write("mine.md", "mine\n");
    eventually!(8, g2.read("read-only copy/mine.md") == "mine\n" && !g2.exists("mine.md"), "ro guest's new file moved aside", g2.proc);
    assert!(!g2.proc.log_contains("conflict"), "ro restore made a conflict copy");

    // Revoke the ro ticket: the guest is cut off and exits; the rw guest continues.
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/guest-ro"), None);
    assert_eq!(code, 204);
    eventually!(45, g2.proc.log_contains("refused the connection"), "ro guest cut off", g2.proc, server.proc);
    o.append("Projects/X/a.md", "after-revoke");
    eventually!(10, g1.contains("a.md", "after-revoke"), "rw guest still receives", g1.proc);
    sleep(1.0);
    assert!(!g2.contains("a.md", "after-revoke"));

    assert!(server.errors().is_empty(), "server errors: {:?}", server.errors());
}
