//! KRPC BEP 5 and BEP 51 message framing and compact addresses.

use crate::bencode::{self, Value};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

pub type Id = [u8; 20];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeContact {
    pub id: Id,
    pub addr: SocketAddrV4,
}

pub enum Message<'a> {
    Query {
        transaction: &'a [u8],
        name: &'a [u8],
        args: &'a Value<'a>,
    },
    Response {
        transaction: &'a [u8],
        data: &'a Value<'a>,
    },
    Error {
        transaction: &'a [u8],
    },
}

pub fn parse<'a>(value: &'a Value<'a>) -> Option<Message<'a>> {
    let transaction = value.get(b"t")?.bytes()?;
    if transaction.is_empty() || transaction.len() > 8 {
        return None;
    }
    match value.get(b"y")?.bytes()? {
        b"q" => Some(Message::Query {
            transaction,
            name: value.get(b"q")?.bytes()?,
            args: value.get(b"a")?,
        }),
        b"r" => Some(Message::Response {
            transaction,
            data: value.get(b"r")?,
        }),
        b"e" => Some(Message::Error { transaction }),
        _ => None,
    }
}

pub fn id(value: &Value<'_>, key: &[u8]) -> Option<Id> {
    value.get(key)?.bytes()?.try_into().ok()
}

pub fn encode_query(id: &Id, transaction: u16, method: &[u8], target: &Id) -> Vec<u8> {
    // The target argument is interpreted as target or info_hash by callers.
    let label = if method == b"get_peers" {
        b"info_hash".as_slice()
    } else {
        b"target".as_slice()
    };
    let tx = transaction.to_be_bytes();
    let args = if method == b"ping" {
        Value::Dict(vec![(b"id", Value::Bytes(id))])
    } else {
        Value::Dict(vec![
            (b"id", Value::Bytes(id)),
            (label, Value::Bytes(target)),
        ])
    };
    bencode::encode(&Value::Dict(vec![
        (b"a", args),
        (b"q", Value::Bytes(method)),
        (b"t", Value::Bytes(&tx)),
        (b"y", Value::Bytes(b"q")),
    ]))
}

pub fn encode_response(transaction: &[u8], data: Value<'_>) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (b"r", data),
        (b"t", Value::Bytes(transaction)),
        (b"y", Value::Bytes(b"r")),
    ]))
}

pub fn encode_error(transaction: &[u8], code: i64, text: &[u8]) -> Vec<u8> {
    bencode::encode(&Value::Dict(vec![
        (
            b"e",
            Value::List(vec![Value::Integer(code), Value::Bytes(text)]),
        ),
        (b"t", Value::Bytes(transaction)),
        (b"y", Value::Bytes(b"e")),
    ]))
}

pub fn compact_node(contact: &NodeContact, out: &mut Vec<u8>) {
    out.extend_from_slice(&contact.id);
    out.extend_from_slice(&contact.addr.ip().octets());
    out.extend_from_slice(&contact.addr.port().to_be_bytes());
}

pub fn decode_nodes(bytes: &[u8], max: usize) -> Vec<NodeContact> {
    bytes
        .chunks_exact(26)
        .take(max)
        .filter_map(|chunk| {
            let mut id = [0; 20];
            id.copy_from_slice(&chunk[..20]);
            let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            let addr = SocketAddrV4::new(ip, port);
            valid_public_address(SocketAddr::V4(addr)).then_some(NodeContact { id, addr })
        })
        .collect()
}

pub fn compact_peer(addr: SocketAddrV4) -> [u8; 6] {
    let mut result = [0; 6];
    result[..4].copy_from_slice(&addr.ip().octets());
    result[4..].copy_from_slice(&addr.port().to_be_bytes());
    result
}

pub fn decode_peers(value: &Value<'_>, max: usize) -> Vec<SocketAddrV4> {
    let Some(items) = value.list() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let bytes = v.bytes()?;
            if bytes.len() != 6 {
                return None;
            }
            let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            let port = u16::from_be_bytes([bytes[4], bytes[5]]);
            let addr = SocketAddrV4::new(ip, port);
            valid_public_address(SocketAddr::V4(addr)).then_some(addr)
        })
        .take(max)
        .collect()
}

/// Avoid wasting network budget on unroutable responses and reduce the risk
/// of requests to private/internal endpoints from untrusted DHT nodes.
pub fn valid_public_address(addr: SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(v4) => {
            let ip = v4.ip();
            let octets = ip.octets();
            v4.port() != 0
                && !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_private()
                && !ip.is_link_local()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !ip.is_documentation()
                && octets[0] != 0
                && octets[0] != 100
                && octets[0] < 224
                && !(octets[0] == 169 && octets[1] == 254)
                && !(octets[0] == 198 && octets[1] == 18)
                && !(octets[0] == 198 && octets[1] == 19)
        }
        SocketAddr::V6(_) => false, // IPv4 initial transport: IPv6 will be separate.
    }
}

pub fn closer(a: &Id, b: &Id, target: &Id) -> std::cmp::Ordering {
    for i in 0..20 {
        let left = a[i] ^ target[i];
        let right = b[i] ^ target[i];
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_queries_and_compact_nodes() {
        let node_id = [17u8; 20];
        let packet = encode_query(&node_id, 12, b"sample_infohashes", &[42; 20]);
        let value = bencode::decode(&packet, 2048).unwrap();
        assert!(matches!(
            parse(&value),
            Some(Message::Query {
                name: b"sample_infohashes",
                ..
            })
        ));
        let address = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 6881);
        let mut out = Vec::new();
        compact_node(
            &NodeContact {
                id: node_id,
                addr: address,
            },
            &mut out,
        );
        assert_eq!(decode_nodes(&out, 10)[0].addr, address);
    }
}
