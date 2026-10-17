//! Share tickets: `samsyn` + base32(postcard(Ticket)). Self-contained: tells the receiving
//! side which share, on which node, with which secret and access level, and (v2) the owner
//! server's public URL so a plugin without its own server can join directly ("guest mode").
//!
//! Postcard is not self-describing, so the leading version byte selects the layout: v1 has no
//! `url`; v2 appends it; v3 appends `rw_paths` (writable paths on a read-only ticket). A ticket
//! is encoded as v2 unless it has `rw_paths`, so plain tickets keep working with older plugins
//! and servers; all three versions are accepted.

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "samsyn";
pub const CURRENT_VERSION: u8 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub version: u8,
    pub share_id: String,
    pub name: String,
    /// Owner server's public key, hex (iroh node id).
    pub node_id: String,
    pub secret: String,
    pub access: String,
    /// Relay URL and direct addresses of the owner server. Empty = unknown.
    pub addrs: Vec<String>,
    /// Owner server's public HTTPS URL for guest clients (v2). Empty = not reachable that way.
    pub url: String,
    /// Share-relative paths a read-only ticket may still write (v3); folders end with `/`.
    pub rw_paths: Vec<String>,
}

/// Wire layout of version 2 tickets (no `rw_paths`).
#[derive(Serialize, Deserialize)]
struct TicketV2 {
    version: u8,
    share_id: String,
    name: String,
    node_id: String,
    secret: String,
    access: String,
    addrs: Vec<String>,
    url: String,
}

/// Wire layout of version 1 tickets (kept so old tickets keep working).
#[derive(Serialize, Deserialize)]
struct TicketV1 {
    version: u8,
    share_id: String,
    name: String,
    node_id: String,
    secret: String,
    access: String,
    addrs: Vec<String>,
}

impl Ticket {
    pub fn encode(&self) -> anyhow::Result<String> {
        let bytes = if self.version == 1 {
            postcard::to_stdvec(&TicketV1 {
                version: 1,
                share_id: self.share_id.clone(),
                name: self.name.clone(),
                node_id: self.node_id.clone(),
                secret: self.secret.clone(),
                access: self.access.clone(),
                addrs: self.addrs.clone(),
            })?
        } else if self.rw_paths.is_empty() {
            postcard::to_stdvec(&TicketV2 {
                version: 2,
                share_id: self.share_id.clone(),
                name: self.name.clone(),
                node_id: self.node_id.clone(),
                secret: self.secret.clone(),
                access: self.access.clone(),
                addrs: self.addrs.clone(),
                url: self.url.clone(),
            })?
        } else {
            postcard::to_stdvec(&Ticket { version: 3, ..self.clone() })?
        };
        Ok(format!("{PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()))
    }

    pub fn decode(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        let body = s.strip_prefix(PREFIX).context("not a samsyn ticket")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .context("ticket is not valid base32")?;
        match bytes.first() {
            Some(1) => {
                let t: TicketV1 = postcard::from_bytes(&bytes).context("ticket payload is malformed")?;
                Ok(Ticket {
                    version: 1,
                    share_id: t.share_id,
                    name: t.name,
                    node_id: t.node_id,
                    secret: t.secret,
                    access: t.access,
                    addrs: t.addrs,
                    url: String::new(),
                    rw_paths: Vec::new(),
                })
            }
            Some(2) => {
                let t: TicketV2 = postcard::from_bytes(&bytes).context("ticket payload is malformed")?;
                Ok(Ticket {
                    version: 2,
                    share_id: t.share_id,
                    name: t.name,
                    node_id: t.node_id,
                    secret: t.secret,
                    access: t.access,
                    addrs: t.addrs,
                    url: t.url,
                    rw_paths: Vec::new(),
                })
            }
            Some(3) => Ok(postcard::from_bytes(&bytes).context("ticket payload is malformed")?),
            Some(v) => anyhow::bail!("unsupported ticket version {v}; upgrade this server"),
            None => anyhow::bail!("ticket payload is empty"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ticket {
        Ticket {
            version: 2,
            share_id: "0123456789abcdef".into(),
            name: "Project X".into(),
            node_id: "ab".repeat(32),
            secret: "cd".repeat(32),
            access: "rw".into(),
            addrs: vec!["relay:https://r.example/".into()],
            url: "https://sync.example".into(),
            rw_paths: Vec::new(),
        }
    }

    #[test]
    fn round_trip_v2() {
        let t = sample();
        let s = t.encode().unwrap();
        assert!(s.starts_with("samsyn"));
        assert_eq!(Ticket::decode(&s).unwrap(), t);
        assert_eq!(Ticket::decode(&format!("  {}\n", s.to_ascii_uppercase().replacen("SAMSYN", "samsyn", 1))).unwrap(), t);
    }

    #[test]
    fn round_trip_v3_with_rw_paths() {
        let mut t = sample();
        t.version = CURRENT_VERSION;
        t.access = "ro".into();
        t.rw_paths = vec!["Inbox/".into(), "todo.md".into()];
        let s = t.encode().unwrap();
        let back = Ticket::decode(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.version, 3);
    }

    #[test]
    fn no_rw_paths_means_v2_on_the_wire() {
        let mut t = sample();
        t.version = CURRENT_VERSION;
        let back = Ticket::decode(&t.encode().unwrap()).unwrap();
        assert_eq!(back.version, 2, "plain tickets stay readable by older plugins");
        t.version = 2;
        assert_eq!(back, t);
    }

    #[test]
    fn v1_tickets_still_decode() {
        let mut t = sample();
        t.version = 1;
        t.url.clear();
        let s = t.encode().unwrap();
        let back = Ticket::decode(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.url, "");
    }

    #[test]
    fn unknown_version_is_refused() {
        let bytes = postcard::to_stdvec(&(7u8, "x")).unwrap();
        let s = format!("{PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&bytes).to_ascii_lowercase());
        assert!(Ticket::decode(&s).unwrap_err().to_string().contains("version 7"));
    }
}
