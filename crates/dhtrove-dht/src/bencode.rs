//! Zero-copy bounded bencode parsing with a prefix decoder for BEP 9 frames.
//! Input is never interpreted as UTF-8. A configurable value limit and recursion
//! budget prevent malicious DHT packets from allocating unbounded structures.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    Integer(i64),
    Bytes(&'a [u8]),
    List(Vec<Value<'a>>),
    Dict(Vec<(&'a [u8], Value<'a>)>),
}

impl<'a> Value<'a> {
    pub fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        match self {
            Self::Dict(entries) => entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&'a [u8]> {
        match self { Self::Bytes(bytes) => Some(bytes), _ => None }
    }

    pub fn integer(&self) -> Option<i64> {
        match self { Self::Integer(number) => Some(*number), _ => None }
    }

    pub fn list(&self) -> Option<&[Value<'a>]> {
        match self { Self::List(items) => Some(items), _ => None }
    }

    pub fn dict(&self) -> Option<&[(&'a [u8], Value<'a>)]> {
        match self { Self::Dict(entries) => Some(entries), _ => None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Truncated,
    TooLarge,
    TooDeep,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "invalid bencode: {self:?}") }
}
impl std::error::Error for Error {}

const MAX_DEPTH: usize = 40;
const MAX_ITEMS: usize = 100_000;

pub fn decode(input: &[u8], max_bytes: usize) -> Result<Value<'_>, Error> {
    let (value, used) = decode_prefix(input, max_bytes)?;
    if used != input.len() { return Err(Error::Invalid); }
    Ok(value)
}

pub fn decode_prefix(input: &[u8], max_bytes: usize) -> Result<(Value<'_>, usize), Error> {
    if input.len() > max_bytes { return Err(Error::TooLarge); }
    let mut cursor = Cursor { input, position: 0, remaining: MAX_ITEMS };
    let value = cursor.value(0)?;
    Ok((value, cursor.position))
}

struct Cursor<'a> {
    input: &'a [u8],
    position: usize,
    remaining: usize,
}
impl<'a> Cursor<'a> {
    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        if depth > MAX_DEPTH { return Err(Error::TooDeep); }
        if self.remaining == 0 { return Err(Error::TooLarge); }
        self.remaining -= 1;
        match self.input.get(self.position).copied().ok_or(Error::Truncated)? {
            b'i' => {
                self.position += 1;
                let start = self.position;
                let end = self.input[start..].iter().position(|c| *c == b'e').ok_or(Error::Truncated)? + start;
                let bytes = &self.input[start..end];
                // Canonical integer syntax protects downstream length/offset consumers.
                if bytes.is_empty() || bytes == b"-0"
                    || (bytes.len() > 1 && bytes[0] == b'0')
                    || (bytes.len() > 2 && bytes.starts_with(b"-0")) { return Err(Error::Invalid); }
                let raw = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
                let number = raw.parse::<i64>().map_err(|_| Error::Invalid)?;
                self.position = end + 1;
                Ok(Value::Integer(number))
            }
            b'l' | b'd' => {
                let is_dict = self.input[self.position] == b'd';
                self.position += 1;
                if is_dict {
                    let mut dict = Vec::new();
                    while self.input.get(self.position) != Some(&b'e') {
                        if self.position >= self.input.len() { return Err(Error::Truncated); }
                        let key = match self.value(depth+1)? {
                            Value::Bytes(bytes) => bytes,
                            _ => return Err(Error::Invalid),
                        };
                        let value = self.value(depth+1)?;
                        // Reject duplicate fields; callers should not interpret
                        // attacker-chosen ambiguous dictionaries inconsistently.
                        if dict.iter().any(|(existing, _)| *existing == key) { return Err(Error::Invalid); }
                        dict.push((key, value));
                    }
                    self.position += 1;
                    Ok(Value::Dict(dict))
                } else {
                    let mut list = Vec::new();
                    while self.input.get(self.position) != Some(&b'e') {
                        if self.position >= self.input.len() { return Err(Error::Truncated); }
                        list.push(self.value(depth+1)?);
                    }
                    self.position += 1;
                    Ok(Value::List(list))
                }
            }
            b'0'..=b'9' => {
                let start = self.position;
                while self.input.get(self.position).is_some_and(|c| c.is_ascii_digit()) { self.position += 1; }
                if self.input.get(self.position) != Some(&b':') { return Err(Error::Invalid); }
                let digits = &self.input[start..self.position];
                if digits.len() > 1 && digits[0] == b'0' { return Err(Error::Invalid); }
                let size: usize = std::str::from_utf8(digits).map_err(|_| Error::Invalid)?
                    .parse().map_err(|_| Error::Invalid)?;
                self.position += 1;
                let end = self.position.checked_add(size).ok_or(Error::TooLarge)?;
                if end > self.input.len() { return Err(Error::Truncated); }
                let data = &self.input[self.position..end];
                self.position = end;
                Ok(Value::Bytes(data))
            }
            _ => Err(Error::Invalid),
        }
    }
}

/// Serializer preserves dictionary order as given; callers provide canonical
/// ascending key order. Useful for KRPC and BEP 10/9 wire messages.
pub fn encode(value: &Value<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

fn write(value: &Value<'_>, out: &mut Vec<u8>) {
    match value {
        Value::Integer(value) => {
            out.push(b'i'); out.extend_from_slice(value.to_string().as_bytes()); out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':'); out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l'); for item in items { write(item, out); } out.push(b'e');
        }
        Value::Dict(entries) => {
            out.push(b'd'); for (key, item) in entries {
                write(&Value::Bytes(key), out); write(item, out);
            } out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_binary_and_prefix() {
        let value = Value::Dict(vec![(b"a", Value::Bytes(&[0,255,42])), (b"b", Value::Integer(42))]);
        let raw = encode(&value);
        assert_eq!(decode(&raw, 512).unwrap(), value);
        let mut extra = raw.clone();extra.extend_from_slice(b"payload");
        assert_eq!(decode_prefix(&extra, 512).unwrap().1, raw.len());
        assert!(decode(&extra, 512).is_err());
    }
    #[test]
    fn rejects_duplicate_keys_and_deep_or_malformed_data() {
        assert!(decode(b"d1:ai1e1:ai2ee", 128).is_err());
        assert!(decode(b"i03e", 128).is_err());
        assert!(decode(b"4:ab", 128).is_err());
        assert!(decode(&vec![b'l'; 45], 1024).is_err());
    }
}
