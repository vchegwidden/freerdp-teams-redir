//! An ICE candidate line broken into the fields of the `icecandidate` event
//! (`msrdc-reply-structures.md` §4) [O]: the Windows add-in sends them next to the line itself.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFields {
    pub foundation: String,
    /// `"rtp"` or `"rtcp"`.
    pub component: &'static str,
    pub protocol: String,
    pub priority: u64,
    pub address: String,
    pub port: u32,
    /// `host`, `srflx`, `prflx` or `relay`: the SDP names, as the add-in sends them.
    pub typ: String,
    pub related_address: Option<String>,
    pub related_port: Option<u32>,
    /// Only for TCP candidates.
    pub tcp_type: Option<String>,
    pub username_fragment: Option<String>,
}

/// Parses `candidate:<foundation> <component> <protocol> <priority> <address> <port> typ <type>
/// [raddr <address> rport <port>] [tcptype <type>] [ufrag <ufrag>] ...`. A leading `a=` is
/// tolerated. `None` if the line is not a candidate.
pub fn parse(line: &str) -> Option<CandidateFields> {
    let line = line.trim();
    let line = line.strip_prefix("a=").unwrap_or(line);
    let line = line.strip_prefix("candidate:")?;
    let mut it = line.split_whitespace();
    let foundation = it.next()?.to_owned();
    let component = match it.next()? {
        "1" => "rtp",
        "2" => "rtcp",
        _ => return None,
    };
    let protocol = it.next()?.to_ascii_lowercase();
    let priority = it.next()?.parse().ok()?;
    let address = it.next()?.to_owned();
    let port = it.next()?.parse().ok()?;
    if it.next()? != "typ" {
        return None;
    }
    let typ = it.next()?.to_owned();
    let mut c = CandidateFields {
        foundation,
        component,
        protocol,
        priority,
        address,
        port,
        typ,
        related_address: None,
        related_port: None,
        tcp_type: None,
        username_fragment: None,
    };
    // The rest is `name value` pairs in any order, and unknown ones are skipped.
    while let (Some(name), Some(value)) = (it.next(), it.next()) {
        match name {
            "raddr" => c.related_address = Some(value.to_owned()),
            "rport" => c.related_port = value.parse().ok(),
            "tcptype" => c.tcp_type = Some(value.to_owned()),
            "ufrag" => c.username_fragment = Some(value.to_owned()),
            _ => {}
        }
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_candidate() {
        let c = parse(
            "candidate:1234567890 1 udp 2122260223 192.0.2.10 50000 typ host generation 0 ufrag abcd network-id 1",
        )
        .unwrap();
        assert_eq!(c.foundation, "1234567890");
        assert_eq!(c.component, "rtp");
        assert_eq!(c.protocol, "udp");
        assert_eq!(c.priority, 2122260223);
        assert_eq!((c.address.as_str(), c.port), ("192.0.2.10", 50000));
        assert_eq!(c.typ, "host");
        assert_eq!(c.username_fragment.as_deref(), Some("abcd"));
        assert_eq!(c.related_address, None);
        assert_eq!(c.tcp_type, None);
    }

    #[test]
    fn relay_and_tcp_candidates() {
        let c = parse(
            "candidate:2 1 UDP 41885439 203.0.113.5 3478 typ relay raddr 0.0.0.0 rport 0 generation 0 ufrag x",
        )
        .unwrap();
        assert_eq!(c.protocol, "udp");
        assert_eq!(c.typ, "relay");
        assert_eq!(c.related_address.as_deref(), Some("0.0.0.0"));
        assert_eq!(c.related_port, Some(0));
        let t = parse(
            "a=candidate:3 1 tcp 1518280447 2001:db8::1 9 typ host tcptype active generation 0",
        )
        .unwrap();
        assert_eq!(t.address, "2001:db8::1");
        assert_eq!(t.tcp_type.as_deref(), Some("active"));
    }

    #[test]
    fn rejects_what_is_not_a_candidate() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("candidate:1 1 udp x 1.2.3.4 5 typ host"), None);
        assert_eq!(parse("candidate:1 3 udp 1 1.2.3.4 5 typ host"), None);
        assert_eq!(parse("candidate:1 1 udp 1 1.2.3.4 5 host"), None);
    }
}
