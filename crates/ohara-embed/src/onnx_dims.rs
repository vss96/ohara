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
    let _ = dims;
    let _ = model;
    bail!("unimplemented")
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
                &value_info("input_ids", &[dim_param("batch_size"), dim_param("sequence_length")]),
            ),
            len_field(
                12,
                &value_info("last_hidden_state", &[dim_param("batch_size"), dim_value(384)]),
            ),
        ]);
        let expected = model(&[
            len_field(11, &value_info("input_ids", &[dim_value(32), dim_value(512)])),
            len_field(12, &value_info("last_hidden_state", &[dim_value(32), dim_value(384)])),
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
        assert_eq!(fixed, input, "unmatched dim_param must round-trip unchanged");
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
