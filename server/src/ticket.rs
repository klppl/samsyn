//! Share tickets: `replica` + base32(postcard(TicketV1)). Self-contained: tells the
//! receiving server which share, on which node, with which secret and access level.

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "replica";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub version: u8,
    pub share_id: String,
    pub name: String,
    /// Owner server's public key, hex (iroh node id).
    pub node_id: String,
    pub secret: String,
    pub access: String,
    /// Relay URL and direct addresses of the owner server (phase 2). Empty = unknown.
    pub addrs: Vec<String>,
}

impl Ticket {
    pub fn encode(&self) -> anyhow::Result<String> {
        let bytes = postcard::to_stdvec(self)?;
        Ok(format!("{PREFIX}{}", data_encoding::BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()))
    }

    pub fn decode(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        let body = s.strip_prefix(PREFIX).context("not a replica ticket")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .context("ticket is not valid base32")?;
        let t: Ticket = postcard::from_bytes(&bytes).context("ticket payload is malformed")?;
        anyhow::ensure!(t.version == 1, "unsupported ticket version {}", t.version);
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = Ticket {
            version: 1,
            share_id: "0123456789abcdef".into(),
            name: "Project X".into(),
            node_id: "ab".repeat(32),
            secret: "cd".repeat(32),
            access: "rw".into(),
            addrs: vec![],
        };
        let s = t.encode().unwrap();
        assert!(s.starts_with("replica"));
        assert_eq!(Ticket::decode(&s).unwrap(), t);
        assert_eq!(Ticket::decode(&format!("  {}\n", s.to_ascii_uppercase().replacen("REPLICA", "replica", 1))).unwrap(), t);
    }
}
