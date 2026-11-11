//! Minimal WireGuard message classification (no decryption).
//!
//! Every WireGuard message starts with a 1-byte type followed by 3 reserved zero bytes:
//! - 1: handshake initiation, 148 bytes
//! - 2: handshake response, 92 bytes
//! - 3: cookie reply, 64 bytes
//! - 4: transport data, 16-byte header + ciphertext + 16-byte tag. Plaintext is padded to a
//!   multiple of 16 but never beyond the MTU, so the length is not necessarily aligned.
//!   A keepalive is an empty transport message, exactly 32 bytes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Handshake / cookie / keepalive: tiny and critical for session liveness.
    Control,
    /// Regular encrypted traffic.
    Data,
}

pub const HANDSHAKE_INIT_LEN: usize = 148;
pub const HANDSHAKE_RESP_LEN: usize = 92;
pub const COOKIE_REPLY_LEN: usize = 64;
pub const KEEPALIVE_LEN: usize = 32;

/// Classifies a datagram. Returns `None` if it does not look like a WireGuard message,
/// in which case the relay drops it.
pub fn classify(p: &[u8]) -> Option<Kind> {
    if p.len() < 4 || p[1..4] != [0, 0, 0] {
        return None;
    }
    match (p[0], p.len()) {
        (1, HANDSHAKE_INIT_LEN) | (2, HANDSHAKE_RESP_LEN) | (3, COOKIE_REPLY_LEN) => {
            Some(Kind::Control)
        }
        (4, KEEPALIVE_LEN) => Some(Kind::Control),
        (4, n) if n > KEEPALIVE_LEN => Some(Kind::Data),
        _ => None,
    }
}

/// Human-readable message type, for logs.
pub fn describe(p: &[u8]) -> &'static str {
    match (p.first(), p.len()) {
        (Some(1), HANDSHAKE_INIT_LEN) => "handshake initiation",
        (Some(2), HANDSHAKE_RESP_LEN) => "handshake response",
        (Some(3), COOKIE_REPLY_LEN) => "cookie reply (server under load)",
        (Some(4), KEEPALIVE_LEN) => "keepalive",
        (Some(4), _) => "data",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(t: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0xAAu8; len];
        v[0] = t;
        v[1..4].fill(0);
        v
    }

    #[test]
    fn control_messages() {
        assert_eq!(classify(&msg(1, 148)), Some(Kind::Control));
        assert_eq!(classify(&msg(2, 92)), Some(Kind::Control));
        assert_eq!(classify(&msg(3, 64)), Some(Kind::Control));
        assert_eq!(classify(&msg(4, 32)), Some(Kind::Control));
    }

    #[test]
    fn data_messages() {
        assert_eq!(classify(&msg(4, 48)), Some(Kind::Data));
        assert_eq!(classify(&msg(4, 1452)), Some(Kind::Data)); // MTU 1420: not 16-aligned
    }

    #[test]
    fn rejects_non_wireguard() {
        assert_eq!(classify(&[]), None);
        assert_eq!(classify(&[1, 0, 0]), None);
        assert_eq!(classify(&msg(1, 147)), None); // wrong handshake size
        assert_eq!(classify(&msg(4, 31)), None); // shorter than header + tag
        assert_eq!(classify(&msg(5, 148)), None); // unknown type
        let mut bad = msg(4, 48);
        bad[2] = 1; // reserved byte set
        assert_eq!(classify(&bad), None);
    }
}
