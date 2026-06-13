//! In-memory ONNX graph-I/O dimension fixing (plan-30).
//!
//! The CoreML MLProgram path requires the model graph itself to carry
//! fixed dimensions: onnxruntime 1.24's ONNX→CoreML conversion emits
//! unbounded dims for symbolic `dim_param`s, the ANE rejects unbounded
//! graphs (`E5RT: ... has unbounded dimension`), and the CoreML-CPU
//! fallback is unstable (see `docs/perf/v0.11-coreml-fixed-shape.md`).
//!
//! [`fix_graph_io_dims`] replaces named `dim_param`s with concrete
//! `dim_value`s on `ModelProto.graph.input` and `.output` — the same
//! transformation `python -m onnxruntime.tools.make_dynamic_shape_fixed`
//! applies, minus the Python. It is a targeted protobuf rewrite: only
//! the message path Model.graph(7) → input(11)/output(12) →
//! ValueInfoProto.type(2) → TypeProto.tensor_type(1) → shape(2) →
//! dim(1) is re-encoded; every other field (including the ~127MB of
//! initializer weights) is copied through verbatim.

use anyhow::{bail, Result};

/// Replace named symbolic dims with fixed values on the model's graph
/// inputs and outputs. `dims` maps `dim_param` names (e.g.
/// `"batch_size"`) to the `dim_value` to bake in. Dim params not named
/// in `dims`, and dims elsewhere in the graph (`value_info`, node
/// attributes), are left untouched.
pub fn fix_graph_io_dims(model: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    // ModelProto.graph = field 7.
    rewrite_message(model, &|field, payload| {
        (field == 7).then(|| rewrite_graph(payload, dims))
    })
}

/// GraphProto.input = 11, .output = 12 (repeated ValueInfoProto).
fn rewrite_graph(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    rewrite_message(buf, &|field, payload| {
        matches!(field, 11 | 12).then(|| rewrite_value_info(payload, dims))
    })
}

/// ValueInfoProto.type = 2 (TypeProto).
fn rewrite_value_info(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    rewrite_message(buf, &|field, payload| {
        (field == 2).then(|| rewrite_type(payload, dims))
    })
}

/// TypeProto.tensor_type = 1 (TypeProto.Tensor).
fn rewrite_type(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    rewrite_message(buf, &|field, payload| {
        (field == 1).then(|| rewrite_tensor(payload, dims))
    })
}

/// TypeProto.Tensor.shape = 2 (TensorShapeProto).
fn rewrite_tensor(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    rewrite_message(buf, &|field, payload| {
        (field == 2).then(|| rewrite_shape(payload, dims))
    })
}

/// TensorShapeProto.dim = 1 (repeated Dimension).
fn rewrite_shape(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    rewrite_message(buf, &|field, payload| {
        (field == 1).then(|| rewrite_dimension(payload, dims))
    })
}

/// Dimension: dim_value = 1 (varint), dim_param = 2 (string). A
/// dim_param matching one of `dims` is replaced by the corresponding
/// dim_value; everything else copies through.
fn rewrite_dimension(buf: &[u8], dims: &[(&str, u64)]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(buf.len());
    let mut pos = 0;
    while pos < buf.len() {
        let start = pos;
        let key = read_varint(buf, &mut pos)?;
        let (field, wire) = (key >> 3, key & 7);
        if field == 2 && wire == 2 {
            let end = read_len_payload_end(buf, &mut pos, field)?;
            let payload = &buf[pos..end];
            let replacement = dims.iter().find(|(name, _)| name.as_bytes() == payload);
            match replacement {
                Some((_, value)) => {
                    write_varint(&mut out, 1 << 3); // dim_value, varint
                    write_varint(&mut out, *value);
                }
                None => out.extend_from_slice(&buf[start..end]),
            }
            pos = end;
            continue;
        }
        copy_field_body(buf, &mut pos, key)?;
        out.extend_from_slice(&buf[start..pos]);
    }
    Ok(out)
}

/// Per-field payload rewriter: returns `Some(new_payload)` for
/// length-delimited fields it wants to re-encode, `None` to copy the
/// field through verbatim.
type FieldRewriter<'a> = &'a dyn Fn(u64, &[u8]) -> Option<Result<Vec<u8>>>;

/// Walk a protobuf message, re-encoding the length-delimited fields the
/// rewriter claims and copying every other field byte-for-byte.
fn rewrite_message(buf: &[u8], rewrite: FieldRewriter) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(buf.len() + 16);
    let mut pos = 0;
    while pos < buf.len() {
        let start = pos;
        let key = read_varint(buf, &mut pos)?;
        let (field, wire) = (key >> 3, key & 7);
        if wire == 2 {
            let end = read_len_payload_end(buf, &mut pos, field)?;
            let payload = &buf[pos..end];
            match rewrite(field, payload) {
                Some(new_payload) => {
                    let new_payload = new_payload?;
                    write_varint(&mut out, key);
                    write_varint(&mut out, new_payload.len() as u64);
                    out.extend_from_slice(&new_payload);
                }
                None => out.extend_from_slice(&buf[start..end]),
            }
            pos = end;
            continue;
        }
        copy_field_body(buf, &mut pos, key)?;
        out.extend_from_slice(&buf[start..pos]);
    }
    Ok(out)
}

/// Advance past a non-length-delimited field body (the tag at `key` has
/// already been consumed; `pos` sits on the body).
fn copy_field_body(buf: &[u8], pos: &mut usize, key: u64) -> Result<()> {
    let (field, wire) = (key >> 3, key & 7);
    match wire {
        0 => {
            let _ = read_varint(buf, pos)?;
            Ok(())
        }
        1 => advance_fixed(buf, pos, 8, field),
        5 => advance_fixed(buf, pos, 4, field),
        2 => {
            let end = read_len_payload_end(buf, pos, field)?;
            *pos = end;
            Ok(())
        }
        other => bail!("unsupported protobuf wire type {other} on field {field}"),
    }
}

fn advance_fixed(buf: &[u8], pos: &mut usize, width: usize, field: u64) -> Result<()> {
    let end = pos
        .checked_add(width)
        .filter(|e| *e <= buf.len())
        .ok_or_else(|| anyhow::anyhow!("fixed-width field {field} overruns buffer"))?;
    *pos = end;
    Ok(())
}

/// Read a length prefix at `pos` and return the payload end offset,
/// leaving `pos` at the payload start.
fn read_len_payload_end(buf: &[u8], pos: &mut usize, field: u64) -> Result<usize> {
    let len = read_varint(buf, pos)? as usize;
    pos.checked_add(len)
        .filter(|e| *e <= buf.len())
        .ok_or_else(|| anyhow::anyhow!("length-delimited field {field} overruns buffer"))
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            bail!("varint overruns buffer at offset {pos}");
        }
        if shift >= 64 {
            bail!("varint longer than 10 bytes at offset {pos}");
        }
        let byte = buf[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::fix_graph_io_dims;

    // -- minimal protobuf builders (test-only) ---------------------------

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn varint_field(field: u64, v: u64) -> Vec<u8> {
        let mut out = varint(field << 3);
        out.extend(varint(v));
        out
    }

    fn len_field(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = varint((field << 3) | 2);
        out.extend(varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn str_field(field: u64, s: &str) -> Vec<u8> {
        len_field(field, s.as_bytes())
    }

    // -- ONNX message builders -------------------------------------------

    /// TensorShapeProto.Dimension with dim_param (field 2).
    fn dim_param(name: &str) -> Vec<u8> {
        str_field(2, name)
    }

    /// TensorShapeProto.Dimension with dim_value (field 1).
    fn dim_value(v: u64) -> Vec<u8> {
        varint_field(1, v)
    }

    /// ValueInfoProto: name (1) + type(2).tensor_type(1){elem_type(1),
    /// shape(2){dim(1)*}}.
    fn value_info(name: &str, dims: &[Vec<u8>]) -> Vec<u8> {
        let mut shape = Vec::new();
        for d in dims {
            shape.extend(len_field(1, d));
        }
        let mut tensor = varint_field(1, 1); // elem_type = FLOAT
        tensor.extend(len_field(2, &shape));
        let type_proto = len_field(1, &tensor);
        let mut out = str_field(1, name);
        out.extend(len_field(2, &type_proto));
        out
    }

    /// ModelProto: ir_version (1, varint) + graph (7).
    fn model(graph_fields: &[Vec<u8>]) -> Vec<u8> {
        let graph: Vec<u8> = graph_fields.concat();
        let mut out = varint_field(1, 8);
        out.extend(len_field(7, &graph));
        out
    }

    const FIX: &[(&str, u64)] = &[("batch_size", 32), ("sequence_length", 512)];

    #[test]
    fn replaces_named_dim_params_in_graph_inputs_and_outputs() {
        let input = model(&[
            len_field(
                11,
                &value_info(
                    "input_ids",
                    &[dim_param("batch_size"), dim_param("sequence_length")],
                ),
            ),
            len_field(
                12,
                &value_info(
                    "last_hidden_state",
                    &[dim_param("batch_size"), dim_value(384)],
                ),
            ),
        ]);
        let expected = model(&[
            len_field(
                11,
                &value_info("input_ids", &[dim_value(32), dim_value(512)]),
            ),
            len_field(
                12,
                &value_info("last_hidden_state", &[dim_value(32), dim_value(384)]),
            ),
        ]);
        let fixed = fix_graph_io_dims(&input, FIX).expect("fix succeeds");
        assert_eq!(fixed, expected);
    }

    #[test]
    fn preserves_unrelated_fields_and_value_info() {
        // node (1) and initializer (5) are opaque payloads that must be
        // copied verbatim; value_info (13) carries the same dim_param
        // but is NOT a graph input/output, so it must stay symbolic.
        let opaque_node = len_field(1, b"node-bytes");
        let initializer = len_field(5, &[0xde, 0xad, 0xbe, 0xef]);
        let vi13 = len_field(13, &value_info("hidden", &[dim_param("batch_size")]));
        let graph_name = str_field(2, "g");
        let input = model(&[
            opaque_node.clone(),
            graph_name.clone(),
            initializer.clone(),
            len_field(11, &value_info("input_ids", &[dim_param("batch_size")])),
            vi13.clone(),
        ]);
        let expected = model(&[
            opaque_node,
            graph_name,
            initializer,
            len_field(11, &value_info("input_ids", &[dim_value(32)])),
            vi13,
        ]);
        let fixed = fix_graph_io_dims(&input, FIX).expect("fix succeeds");
        assert_eq!(fixed, expected);
    }

    #[test]
    fn leaves_unmatched_dim_params_alone() {
        let input = model(&[len_field(
            11,
            &value_info("x", &[dim_param("other_dim"), dim_value(7)]),
        )]);
        let fixed = fix_graph_io_dims(&input, FIX).expect("fix succeeds");
        assert_eq!(
            fixed, input,
            "unmatched dim_param must round-trip unchanged"
        );
    }

    #[test]
    fn errors_on_truncated_model() {
        let input = model(&[len_field(
            11,
            &value_info("input_ids", &[dim_param("batch_size")]),
        )]);
        let truncated = &input[..input.len() - 3];
        assert!(fix_graph_io_dims(truncated, FIX).is_err());
    }
}
