# Durable tombstones (experimental)

\`Store::delete(&Infohash)\` removes a logical torrent from the searchable
infohash index without immediately rewriting compressed data blocks. A hybrid
torrent's v1 and v2 hashes are deleted **together**; deleting an unknown hash
returns \`false\`.

A deletion is logged as its own small \`DHTD\` frame (magic, version, v1/v2 flags
and raw hash bytes) inside an existing \`DHTS\` segment. It has the same
outer length + CRC32 wrapper, durability guarantee (\`sync_data\`) and startup
replay rules as insertion frames. Deletions can be replayed after a saved
hash-index checkpoint, and deleted hashes can be reinserted later.

### Why not remove files immediately?

Each compressed block can contain metadata for many torrrents, so rewriting
one would be costly and would risk other records during crashes. Tombstones
preserve query semantics immediately; a later **compaction** operation will
rewrite only the currently live records into new segments and free obsolete
ones safely.

Until compaction is implemented, deleting entries *increases* the segment
files by a small amount and does not reclaim disk space. Quotas apply to
tombstone writes just like normal insertions.

The on-disk frame remains experimental. Older readers that don't recognize
the \`DHTD\` marker must not open stores containing tombstones.
