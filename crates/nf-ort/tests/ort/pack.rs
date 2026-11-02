//! Loading a pack, and what it refuses (TODO 5's tests, item 2): hashes, sizes, the path and id rules, an unknown spec
//! major, a broken copy, a model whose tensors keep their data in another file (even with a manifest that pins it), the
//! runtime's load against the full one (the conformance files), and the session's signature check against the graph.

use std::path::Path;

use nf_ort::onnx_check::{encode, refuse_external_data};
use nf_ort::pack::{MAX_MANIFEST, Verify, check, sha256_hex};
use nf_ort::{ModelSession, Pack, SessionOptions};

use crate::common::{self, ID};

/// The fetched pack's folder copied to `dir/<id>` (so the folder is named by the id), with `edit` applied to its
/// manifest's JSON and `bytes` to its model file.
fn copy(pack: &Pack, dir: &Path, edit: impl FnOnce(&mut serde_json::Value), bytes: impl FnOnce(&mut Vec<u8>)) -> std::path::PathBuf {
    let to = dir.join(ID);
    std::fs::create_dir_all(to.join("conformance")).unwrap();
    for f in &pack.manifest.files {
        std::fs::copy(pack.file(&f.path).unwrap(), to.join(&f.path)).unwrap();
    }
    let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(pack.dir.join("manifest.json")).unwrap()).unwrap();
    edit(&mut m);
    std::fs::write(to.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
    let model = to.join(&pack.manifest.graph.model);
    let mut b = std::fs::read(&model).unwrap();
    bytes(&mut b);
    std::fs::write(&model, b).unwrap();
    to
}

#[test]
fn the_fetched_pack_loads_with_every_hash_checked() {
    let Some(p) = common::pack() else { return };
    let m = &p.manifest;
    assert_eq!((m.id.as_str(), m.signal.sample_rate, p.rate_factor(), p.band_hz()), ("ulunas", 16_000, 3, 8000.0));
    assert_eq!(p.model_bytes.len(), 788_967);
    assert_eq!(nf_ort::pack::sha256_hex(&p.model_bytes).unwrap(), "f2e804d54d6a88f4f82f44d86c9f1cf646db2509bfca935cfbfc5fcd8cbfac3b");
    assert_eq!((p.stft().n_fft, p.stft().hop_length, m.state.len(), m.conformance.len()), (512, 256, 3, 2));
    assert_eq!(m.licenses.weights, "MIT");
    assert!(m.files.iter().all(|f| f.size.is_some() && f.sha256.is_some()));
}

#[test]
fn a_broken_copy_or_a_bad_manifest_is_refused() {
    let Some(p) = common::pack() else { return };
    let dir = common::temp_dir("pack");
    let refused = |name: &str, edit: &dyn Fn(&mut serde_json::Value), bytes: &dyn Fn(&mut Vec<u8>), why: &str| {
        let d = dir.join(name);
        let path = copy(&p, &d, edit, bytes);
        let e = Pack::load(&path).expect_err(name);
        assert!(e.0.contains(why), "{name}: {e} (wanted {why:?})");
    };
    let none = |_: &mut serde_json::Value| {};
    let same = |_: &mut Vec<u8>| {};
    // A good copy loads.
    assert!(Pack::load(&copy(&p, &dir.join("good"), none, same)).is_ok());
    refused("flipped", &none, &|b| b[1000] ^= 1, "SHA-256");
    refused("truncated", &none, &|b| b.truncate(1000), "bytes, the manifest says");
    refused("grown", &none, &|b| b.push(0), "more than the");
    refused("spec", &|m| m["spec"] = "noise-filter.model-pack/1".into(), same_bytes(), "major version 1");
    refused("id", &|m| m["id"] = "UL-UNAS".into(), same_bytes(), "isn't [a-z0-9]");
    refused("folder", &|m| m["id"] = "other".into(), same_bytes(), "the folder is");
    refused("traversal", &|m| m["files"][1]["path"] = "../LICENSE".into(), same_bytes(), "unsafe path");
    refused("absolute", &|m| m["files"][1]["path"] = "C:/Windows/win.ini".into(), same_bytes(), "unsafe path");
    refused("device", &|m| m["files"][1]["path"] = "nul.txt".into(), same_bytes(), "unsafe path");
    refused("twice", &|m| m["files"][2]["path"] = "license".into(), same_bytes(), "listed twice");
    refused("unpinned", &|m| m["files"][1]["sha256"] = serde_json::Value::Null, same_bytes(), "needs its size");
    refused("two models", &|m| m["files"][1]["role"] = "model".into(), same_bytes(), "exactly one model");
    refused("waveform", &|m| m["graph"]["kind"] = "waveform".into(), same_bytes(), "waveform packs aren't supported");
    refused("rate", &|m| m["signal"]["sample_rate"] = 44_100.into(), same_bytes(), "whole divisor");
    refused("ref", &|m| m["role"]["inputs"] = serde_json::json!(["mic", "ref"]), same_bytes(), "mic only");
    refused("stft", &|m| m["graph"]["stft"]["window_type"] = "vorbis".into(), same_bytes(), "Hann");
    refused("shape", &|m| m["io"]["input"]["shape"] = serde_json::json!([1, 256, 1, 2]), same_bytes(), "[1, 257, 1, 2]");
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(MAX_MANIFEST, 1 << 20);
}

fn same_bytes() -> &'static dyn Fn(&mut Vec<u8>) {
    &|_| {}
}

/// The fetched model has all its data in the file; a hostile pack, its manifest pinning a model whose initializer names
/// `w.bin` (ONNX Runtime would read that from the current folder), is refused at load and, its bytes put in a loaded pack,
/// by the session too.
#[test]
fn a_model_with_external_data_is_refused_even_when_pinned() {
    let Some(p) = common::pack() else { return };
    let seen = refuse_external_data(&p.model_bytes).expect("UL-UNAS keeps its data in the file");
    eprintln!("UL-UNAS: {} tensors in {} graph(s), none external", seen.tensors, seen.graphs);
    assert!(seen.tensors >= 324 && seen.graphs >= 1, "{seen:?}");
    let evil = encode::identity_model(Some("w.bin"));
    let dir = common::temp_dir("external");
    let (size, sha) = (evil.len(), sha256_hex(&evil).unwrap());
    let path = copy(
        &p,
        &dir,
        |m| {
            m["files"][0]["size"] = size.into();
            m["files"][0]["sha256"] = sha.clone().into();
        },
        |b| *b = evil.clone(),
    );
    let e = Pack::load(&path).expect_err("a pinned model with external data");
    assert!(e.0.contains("another file"), "{e}");
    let mut q = p.clone();
    q.model_bytes = evil.clone();
    let e = ModelSession::new(&q, SessionOptions::default()).err().expect("refused before ONNX Runtime sees it");
    assert!(e.contains("another file"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The runtime's load reads and hashes the model and the licence only; the full one (the tests', the conformance
/// runner's) the conformance files too.
#[test]
fn the_runtime_load_skips_the_conformance_files() {
    let Some(p) = common::pack() else { return };
    let dir = common::temp_dir("verify");
    let path = copy(&p, &dir, |_| {}, |_| {});
    let wav = path.join(&p.manifest.files[2].path);
    let mut b = std::fs::read(&wav).unwrap();
    b[100] ^= 1;
    std::fs::write(&wav, b).unwrap();
    assert!(Pack::load(&path).is_ok(), "the runtime doesn't read the conformance pairs");
    let e = Pack::load_with(&path, Verify::All).expect_err("a broken conformance file");
    assert!(e.0.contains("SHA-256") && e.0.contains("conformance"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The manifest's names, dtypes and shapes against the graph's own inputs and outputs: a name the graph lacks, a shape it
/// disagrees with, and a graph input the manifest leaves out are each refused when the session is built.
#[test]
fn the_session_checks_the_graphs_signature() {
    let Some(p) = common::pack() else { return };
    let s = ModelSession::new(&p, SessionOptions::default()).expect("the session");
    let names: Vec<&str> = s.session().inputs().iter().map(|o| o.name()).collect();
    assert_eq!(names, ["mix", "conv_cache", "tfa_cache", "inter_cache"], "the graph's inputs, in order");
    assert_eq!(s.state_len(), 5358 + 402 + 1056);
    let with = |f: &dyn Fn(&mut Pack)| {
        let mut q = p.clone();
        f(&mut q);
        ModelSession::new(&q, SessionOptions::default()).err().expect("refused")
    };
    let e = with(&|q| q.manifest.io.input.name = "noisy".into());
    assert!(e.contains("no input named \"noisy\""), "{e}");
    let e = with(&|q| q.manifest.state[1].shape = vec![1, 400]);
    assert!(e.contains("tfa_cache") && e.contains("[1, 400]"), "{e}");
    let e = with(&|q| {
        q.manifest.state.pop();
    });
    assert!(e.contains("inter_cache") && e.contains("isn't in the manifest"), "{e}");
    let e = with(&|q| q.manifest.state[0].output = "nope".into());
    assert!(e.contains("no state output named \"nope\""), "{e}");
    // And the manifest's own checks, without a session.
    let mut m = p.manifest.clone();
    m.io.output.kind = Some("waveform".into());
    assert!(check(&m, ID).unwrap_err().contains("io.output kind"));
}

#[test]
fn a_missing_pack_says_how_to_fetch_it() {
    let e = Pack::installed("no-such-pack").unwrap_err();
    assert!(e.0.contains("isn't installed") && e.0.contains("python -I tools/fetch_models.py no-such-pack"), "{e}");
    assert!(e.0.contains("no-such-pack"), "the folder named: {e}");
    assert!(Pack::installed("../ulunas").unwrap_err().0.contains("isn't a model id"));
}
