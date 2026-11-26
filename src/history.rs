//! Compressed listening history. A chunk is listens in time order, each stored as
//! varint(ts delta) + varint(track_id) + varint(album_id), then zstd over the whole chunk.

use anyhow::{Context, Result, bail};

pub const CHUNK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub ts: i64,
    pub track: i32,
    /// 0 = no album
    pub album: i32,
}

pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(entries.len() * 6);
    let mut prev = 0i64;
    for e in entries {
        put(&mut raw, (e.ts - prev) as u64);
        put(&mut raw, e.track as u64);
        put(&mut raw, e.album as u64);
        prev = e.ts;
    }
    zstd::bulk::compress(&raw, 9).expect("zstd compress into Vec cannot fail")
}

pub fn decode(data: &[u8]) -> Result<Vec<Entry>> {
    let raw = zstd::stream::decode_all(data).context("history chunk is not valid zstd")?;
    let mut out = Vec::with_capacity(raw.len() / 6);
    let (mut pos, mut prev) = (0usize, 0i64);
    while pos < raw.len() {
        prev += get(&raw, &mut pos)? as i64;
        let track = get(&raw, &mut pos)? as i32;
        let album = get(&raw, &mut pos)? as i32;
        out.push(Entry { ts: prev, track, album });
    }
    Ok(out)
}

fn put(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn get(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&b) = buf.get(*pos) else { bail!("truncated varint") };
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return Ok(v);
        }
    }
    bail!("varint too long")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_size() {
        let entries: Vec<Entry> = (0..CHUNK as i64)
            .map(|i| Entry { ts: 1_700_000_000 + i * 200, track: (i % 300) as i32 + 50_000, album: (i % 40) as i32 })
            .collect();
        let data = encode(&entries);
        assert_eq!(decode(&data).unwrap(), entries);
        assert!(data.len() < CHUNK * 4, "{} bytes for {CHUNK} listens", data.len());
        assert!(decode(&encode(&[])).unwrap().is_empty());
    }
}
