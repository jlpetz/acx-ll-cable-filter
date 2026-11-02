//! A model's protobuf, checked before ONNX Runtime sees it: no tensor may keep its data outside the file.
//!
//! **Why.** ONNX Runtime 1.31 (through ort rc.15) resolves a relative external-data location of a model loaded from bytes
//! (`commit_from_memory`) against the process's current folder, refusing only `..` and absolute paths (the verifier's
//! check, 2026-11-02: a 118-byte model whose initializer named `w.bin` read that file from the current folder). So "an
//! in-memory model has no base path" doesn't hold, and a hostile pack (a manifest and a model together, the hashes being the
//! cached manifest's) could read files under the current folder into its initializers. [`refuse_external_data`] refuses any
//! `TensorProto` with `external_data` entries (field 13) or a `data_location` (field 14) other than `DEFAULT`.
//!
//! **Where tensors live** (onnx.proto, IR 10), all walked: the graph's `initializer` (5) and `sparse_initializer` (15,
//! whose `values` and `indices` are tensors); every node's attributes (`t` 5, `tensors` 10, `sparse_tensor` 22,
//! `sparse_tensors` 23) and the subgraphs they hold (`g` 6, `graphs` 11: an `If`'s or `Loop`'s body, to any depth); the
//! model's `training_info` graphs (20: `initialization` 1, `algorithm` 3) and its `functions` (25: `node` 7,
//! `attribute_proto` 11). Other fields are skipped by their wire type. A field seen twice is checked each time (protobuf
//! merges repeats). Groups (wire types 3 and 4, which ONNX never uses), a malformed or truncated message, and nesting past
//! [`MAX_DEPTH`] (protobuf's own default recursion limit) are refused.

/// Messages nested deeper than this are refused.
pub const MAX_DEPTH: u32 = 100;

/// What the walk saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// Tensors (initializers, attribute tensors, sparse tensors' parts), every one with its data in the file.
    pub tensors: usize,
    /// Graphs, the main one and every subgraph.
    pub graphs: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Msg {
    Model,
    Graph,
    Node,
    Attribute,
    Tensor,
    Sparse,
    Training,
    Function,
}

/// The message a length-delimited `field` of `m` holds, when it is one that can hold a tensor.
fn child(m: Msg, field: u64) -> Option<Msg> {
    use Msg::*;
    match (m, field) {
        (Model, 7) | (Attribute, 6 | 11) | (Training, 1 | 3) => Some(Graph),
        (Model, 20) => Some(Training),
        (Model, 25) => Some(Function),
        (Graph, 1) | (Function, 7) => Some(Node),
        (Graph, 5) | (Attribute, 5 | 10) | (Sparse, 1 | 2) => Some(Tensor),
        (Graph, 15) | (Attribute, 22 | 23) => Some(Sparse),
        (Node, 5) | (Function, 11) => Some(Attribute),
        _ => None,
    }
}

fn varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos).ok_or("a truncated varint")?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("a varint over 10 bytes".into())
}

fn walk(buf: &[u8], m: Msg, depth: u32, seen: &mut Checked) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("messages nested over {MAX_DEPTH} deep"));
    }
    match m {
        Msg::Tensor => seen.tensors += 1,
        Msg::Graph => seen.graphs += 1,
        _ => {}
    }
    let mut pos = 0;
    while pos < buf.len() {
        let key = varint(buf, &mut pos)?;
        let (field, wire) = (key >> 3, key & 7);
        if m == Msg::Tensor && field == 13 {
            return Err("a tensor keeps its data in another file (external_data)".into());
        }
        match wire {
            0 => {
                let v = varint(buf, &mut pos)?;
                if m == Msg::Tensor && field == 14 && v != 0 {
                    return Err(format!("a tensor keeps its data in another file (data_location {v})"));
                }
            }
            1 => pos = pos.checked_add(8).filter(|&p| p <= buf.len()).ok_or("a truncated fixed64")?,
            5 => pos = pos.checked_add(4).filter(|&p| p <= buf.len()).ok_or("a truncated fixed32")?,
            2 => {
                let n = usize::try_from(varint(buf, &mut pos)?).map_err(|_| "a length over usize")?;
                let end = pos.checked_add(n).filter(|&e| e <= buf.len()).ok_or("a length past the end")?;
                if m == Msg::Tensor && field == 14 {
                    return Err("a tensor's data_location isn't a plain value".into());
                }
                if let Some(c) = child(m, field) {
                    walk(&buf[pos..end], c, depth + 1, seen)?;
                }
                pos = end;
            }
            w => return Err(format!("wire type {w} (a group or no protobuf at all)")),
        }
    }
    Ok(())
}

/// Walks the ONNX model in `model` (module notes): `Ok` with what it saw when no tensor keeps its data outside the file.
pub fn refuse_external_data(model: &[u8]) -> Result<Checked, String> {
    let mut seen = Checked::default();
    walk(model, Msg::Model, 0, &mut seen).map_err(|e| format!("the model file was refused: {e}"))?;
    if seen.graphs == 0 {
        return Err("the model file was refused: it has no graph".into());
    }
    Ok(seen)
}

/// Hand-encoded protobuf for the tests (here and nf-ort's integration tests): a field's key and bytes.
pub mod encode {
    pub fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// A varint field.
    pub fn int(field: u64, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(field << 3, &mut out);
        varint(v, &mut out);
        out
    }

    /// A length-delimited field (a message, a string, bytes).
    pub fn bytes(field: u64, v: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint((field << 3) | 2, &mut out);
        varint(v.len() as u64, &mut out);
        out.extend_from_slice(v);
        out
    }

    pub fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// A float tensor named `name` of two values, its data `raw_data` in the file, or `external` naming another file.
    pub fn tensor(name: &str, external: Option<&str>) -> Vec<u8> {
        let mut t = cat(&[int(1, 2), int(2, 1), bytes(8, name.as_bytes())]);
        match external {
            None => t.extend(bytes(9, &[0, 0, 192, 63, 0, 0, 16, 192])),
            Some(file) => {
                t.extend(bytes(13, &cat(&[bytes(1, b"location"), bytes(2, file.as_bytes())])));
                t.extend(int(14, 1));
            }
        }
        t
    }

    /// A model of IR 7, opset 13, with `graph`.
    pub fn model(graph: &[u8]) -> Vec<u8> {
        cat(&[int(1, 7), bytes(2, b"noise-filter-test"), bytes(8, &cat(&[bytes(1, b""), int(2, 13)])), bytes(7, graph)])
    }

    /// `Y = Identity(W)` with initializer `W` (its data in the file, or `external`): the verifier's 118-byte case.
    pub fn identity_model(external: Option<&str>) -> Vec<u8> {
        let node = cat(&[bytes(1, b"W"), bytes(2, b"Y"), bytes(4, b"Identity")]);
        let ty = bytes(1, &cat(&[int(1, 1), bytes(2, &cat(&[bytes(1, &int(1, 2))]))]));
        let output = cat(&[bytes(1, b"Y"), bytes(2, &ty)]);
        model(&cat(&[bytes(1, &node), bytes(2, b"g"), bytes(5, &tensor("W", external)), bytes(12, &output)]))
    }
}

#[cfg(test)]
mod tests {
    use super::encode::*;
    use super::*;

    #[test]
    fn a_model_with_its_data_in_the_file_passes() {
        let ok = identity_model(None);
        assert_eq!(refuse_external_data(&ok), Ok(Checked { tensors: 1, graphs: 1 }));
    }

    /// External data anywhere a tensor can be: an initializer, a node's attribute (a Constant's value), a subgraph's
    /// initializer (an If's branch), a sparse initializer's values, a function's node, a training graph; by the
    /// `external_data` entries alone, or by `data_location` alone.
    #[test]
    fn external_data_is_refused_wherever_a_tensor_can_be() {
        let ext = || tensor("W", Some("w.bin"));
        let refused = |what: &str, m: Vec<u8>| {
            let e = refuse_external_data(&m).expect_err(what);
            assert!(e.contains("another file"), "{what}: {e}");
        };
        refused("an initializer", identity_model(Some("w.bin")));
        let constant = cat(&[bytes(2, b"Y"), bytes(4, b"Constant"), bytes(5, &cat(&[bytes(1, b"value"), bytes(5, &ext())]))]);
        refused("a Constant's value", model(&bytes(1, &constant)));
        let tensors_attr = cat(&[bytes(4, b"X"), bytes(5, &cat(&[bytes(1, b"a"), bytes(10, &tensor("ok", None)), bytes(10, &ext())]))]);
        refused("an attribute's tensor list", model(&bytes(1, &tensors_attr)));
        let branch = bytes(5, &ext());
        let iff = cat(&[bytes(4, b"If"), bytes(5, &cat(&[bytes(1, b"then_branch"), bytes(6, &branch)]))]);
        refused("an If's branch", model(&bytes(1, &iff)));
        let deep = cat(&[bytes(4, b"Loop"), bytes(5, &cat(&[bytes(1, b"body"), bytes(6, &bytes(1, &iff))]))]);
        refused("a Loop's body's If's branch", model(&bytes(1, &deep)));
        let sparse = cat(&[bytes(1, &ext()), bytes(2, &tensor("i", None))]);
        refused("a sparse initializer", model(&bytes(15, &sparse)));
        let sparse_attr = cat(&[bytes(4, b"Constant"), bytes(5, &cat(&[bytes(1, b"sparse_value"), bytes(22, &sparse)]))]);
        refused("a sparse attribute", model(&bytes(1, &sparse_attr)));
        let function = cat(&[bytes(1, b"f"), bytes(7, &constant)]);
        refused("a function", cat(&[identity_model(None), bytes(25, &function)]));
        refused("a training graph", cat(&[identity_model(None), bytes(20, &bytes(1, &bytes(5, &ext())))]));
        let entries_only = cat(&[int(1, 2), bytes(8, b"W"), bytes(13, &bytes(1, b"location"))]);
        refused("external_data entries alone", model(&bytes(5, &entries_only)));
        let location_only = cat(&[int(1, 2), bytes(8, b"W"), int(14, 1)]);
        refused("data_location alone", model(&bytes(5, &location_only)));
        // An unknown field 13 elsewhere (not in a tensor) is nobody's external data.
        assert!(refuse_external_data(&cat(&[identity_model(None), bytes(13, b"x")])).is_ok());
    }

    #[test]
    fn malformed_or_hostile_protobuf_is_refused() {
        let ok = identity_model(None);
        for cut in [1, ok.len() / 2, ok.len() - 1] {
            assert!(refuse_external_data(&ok[..cut]).is_err(), "truncated at {cut}");
        }
        assert!(refuse_external_data(&[0x0b]).unwrap_err().contains("wire type 3"), "a group");
        assert!(refuse_external_data(&[0xff; 12]).is_err(), "a runaway varint");
        assert!(refuse_external_data(&int(1, 7)).unwrap_err().contains("no graph"));
        // Graphs nested past the limit, through If branches.
        let mut g = identity_model(None);
        for _ in 0..60 {
            let iff = cat(&[bytes(4, b"If"), bytes(5, &cat(&[bytes(1, b"then_branch"), bytes(6, &g)]))]);
            g = bytes(1, &iff);
        }
        assert!(refuse_external_data(&model(&g)).unwrap_err().contains("nested"));
        let wrong_type = cat(&[int(1, 2), bytes(14, b"\x01")]);
        assert!(refuse_external_data(&model(&bytes(5, &wrong_type))).unwrap_err().contains("data_location"));
    }
}
