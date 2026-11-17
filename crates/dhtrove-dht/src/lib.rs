//! Async IPv4 Mainline DHT node and index crawler.
//!
//! Actively surveys BEP 51 sample_infohashes, walks BEP 5 routing contacts,
//! observes incoming get_peers/announce_peer, and performs bounded get_peers
//! lookups. It never impersonates unrelated DHT node IDs and does not download
//! torrent content. IPv6, BEP 44 and uTP are separate future features.

pub mod bencode;
pub mod krpc;
mod node;
pub mod routing;

pub use node::{DhtConfig, DhtNode, DhtStats, Sighting};

pub use krpc::{Id, NodeContact};
