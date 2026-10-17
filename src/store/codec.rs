//! Chunk encoding: `k` arrays of `n` f32 values (NaN = no data), byte-shuffled and zstd
//! compressed. Shuffling groups the n-th byte of every value together, so the slowly changing
//! exponent bytes compress well.

const VERSION: u8 = 1;
const HEADER: usize = 4;
const LEVEL: i32 = 3;

pub fn encode(k: usize, values: &[f32]) -> Vec<u8> {
  debug_assert!(k > 0 && values.len().is_multiple_of(k));
  let n = values.len() / k;
  let total = values.len();
  let mut shuffled = vec![0u8; total * 4];
  for (i, v) in values.iter().enumerate() {
    for (b, byte) in v.to_le_bytes().into_iter().enumerate() {
      shuffled[b * total + i] = byte;
    }
  }

  let mut out = Vec::with_capacity(HEADER + total);
  out.push(VERSION);
  out.push(k as u8);
  out.extend_from_slice(&(n as u16).to_le_bytes());
  out.extend(zstd::bulk::compress(&shuffled, LEVEL).expect("zstd compress"));
  out
}

/// Returns `(k, n, values)` where values holds `k` arrays of `n` values back to back.
pub fn decode(blob: &[u8]) -> Option<(usize, usize, Vec<f32>)> {
  if blob.len() < HEADER || blob[0] != VERSION {
    return None;
  }
  let k = blob[1] as usize;
  let n = u16::from_le_bytes([blob[2], blob[3]]) as usize;
  let total = k * n;
  let raw = zstd::bulk::decompress(&blob[HEADER..], total * 4).ok()?;
  if raw.len() != total * 4 {
    return None;
  }

  let mut values = Vec::with_capacity(total);
  for i in 0..total {
    let bytes = [raw[i], raw[total + i], raw[2 * total + i], raw[3 * total + i]];
    values.push(f32::from_le_bytes(bytes));
  }
  Some((k, n, values))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrip_with_gaps() {
    let mut values: Vec<f32> = (0..720).map(|i| (i as f32 * 0.37).sin() * 30.0 + 40.0).collect();
    values[5] = f32::NAN;
    values[700] = f32::NAN;
    let blob = encode(3, &values);
    let (k, n, out) = decode(&blob).unwrap();
    assert_eq!((k, n), (3, 240));
    for (a, b) in values.iter().zip(&out) {
      assert!(a.to_bits() == b.to_bits());
    }
  }

  #[test]
  fn constant_series_compress_well() {
    let values = vec![34359738368.0f32; 288];
    let blob = encode(1, &values);
    assert!(blob.len() < 40, "blob is {} bytes", blob.len());
  }

  #[test]
  fn rejects_garbage() {
    assert!(decode(&[9, 1, 0, 0]).is_none());
    assert!(decode(&[1]).is_none());
  }
}
