//! Little-endian f32 byte codec for sqlite-vec virtual table values.
//!
//! `vec_commit`, `vec_hunk`, and `vec_symbol` columns store `FLOAT[N]` embeddings
//! as raw bytes. These helpers handle the platform-deterministic LE encoding.

pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

pub fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    debug_assert!(
        b.len() % 4 == 0,
        "vec_codec: byte length {} not f32-aligned",
        b.len()
    );
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)]
    fn round_trip_preserves_values() {
        let v = vec![
            0.0_f32,
            0.1,
            -1.5,
            3.14,
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::MIN,
        ];
        let bytes = vec_to_bytes(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        let back = bytes_to_vec(&bytes);
        assert_eq!(back, v);
    }

    #[test]
    fn empty_round_trip() {
        let v: Vec<f32> = vec![];
        assert_eq!(bytes_to_vec(&vec_to_bytes(&v)), v);
    }

    #[test]
    fn full_dimension_round_trip() {
        let v: Vec<f32> = (0..384).map(|i| i as f32 * 0.1).collect();
        assert_eq!(bytes_to_vec(&vec_to_bytes(&v)), v);
    }
}
