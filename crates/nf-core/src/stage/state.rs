//! State (`clap.state`): the default save and load, which hold a stage's parameters. Called by whoever holds the stage,
//! never during `process`; not real-time (they allocate).
//!
//! Format v1, all little-endian: `b"NFST"`, u16 version (1), u16 reserved (0), u32 count, then count × (u32 id, f64
//! value). Loading ignores unknown ids, keeps the params the blob leaves out, clamps each value through
//! [`super::Stage::set_param`], skips read-only params, ignores bytes after the last entry, and refuses a bad magic, an
//! unknown version, a truncated blob or a NaN value (before applying any of it).

use super::{ParamFlags, Stage, StageError};

pub const MAGIC: [u8; 4] = *b"NFST";
pub const VERSION: u16 = 1;
/// Magic, version, reserved and count.
const HEADER: usize = 12;
/// Id and value.
const ENTRY: usize = 12;

/// Appends `stage`'s parameters, in its declared order.
pub fn save_params<S: Stage + ?Sized>(stage: &S, out: &mut Vec<u8>) {
    let values: Vec<(u32, f64)> = stage.params().iter().filter_map(|p| stage.param_value(p.id).map(|v| (p.id, v))).collect();
    out.reserve(HEADER + ENTRY * values.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&u32::try_from(values.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (id, v) in values {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// The (id, value) entries of a v1 blob.
fn entries(data: &[u8]) -> Result<Vec<(u32, f64)>, StageError> {
    let bad = |why: &str| Err(StageError(format!("stage state: {why}")));
    let Some((header, body)) = data.split_first_chunk::<HEADER>() else { return bad("too short for its header") };
    if header[..4] != MAGIC {
        return bad("not a noise-filter stage state (bad magic)");
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != VERSION {
        return bad(&format!("version {version} is unknown (this build reads {VERSION})"));
    }
    let count = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    let Some(body) = count.checked_mul(ENTRY).and_then(|n| body.get(..n)) else { return bad("truncated") };
    Ok(body
        .as_chunks::<ENTRY>()
        .0
        .iter()
        .map(|e| {
            let (id, v) = e.split_at(4);
            let id = u32::from_le_bytes([id[0], id[1], id[2], id[3]]);
            (id, f64::from_le_bytes(v.try_into().unwrap_or([0; 8])))
        })
        .collect())
}

/// Applies a blob from [`save_params`] to `stage` (see the module notes).
pub fn load_params<S: Stage + ?Sized>(stage: &mut S, data: &[u8]) -> Result<(), StageError> {
    let entries = entries(data)?;
    let known = |stage: &S, id: u32| stage.params().iter().any(|p| p.id == id && !p.flags.contains(ParamFlags::READONLY));
    if let Some((id, _)) = entries.iter().find(|(id, v)| v.is_nan() && known(stage, *id)) {
        return Err(StageError(format!("stage state: param {id} is NaN")));
    }
    for (id, v) in entries {
        if known(stage, id) {
            stage.set_param(id, v)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::Relaxed;

    use super::super::*;
    use super::*;

    /// Two params in atomics, and a read-only third.
    struct Two {
        a: AtomicU64,
        b: AtomicU64,
    }

    static PARAMS: [ParamInfo; 3] = [
        param(3, "a", "dB", (-10.0, 0.0), -1.0, ParamFlags::NONE),
        param(9, "b", "", (0.0, 4.0), 0.0, ParamFlags::STEPPED),
        param(5, "c", "", (0.0, 1.0), 0.0, ParamFlags::READONLY),
    ];

    const fn param(id: ParamId, name: &'static str, unit: &'static str, range: (f64, f64), default: f64, flags: ParamFlags) -> ParamInfo {
        ParamInfo { id, name: text(name), module: text(""), unit: text(unit), min: range.0, max: range.1, default, flags }
    }

    impl Two {
        fn new() -> Self {
            Self { a: AtomicU64::new((-1.0f64).to_bits()), b: AtomicU64::new(0) }
        }
        fn cell(&self, id: ParamId) -> Option<&AtomicU64> {
            match id {
                3 => Some(&self.a),
                9 => Some(&self.b),
                _ => None,
            }
        }
    }

    impl Stage for Two {
        fn descriptor(&self) -> &StageDescriptor {
            static D: StageDescriptor = StageDescriptor {
                id: text("test.two"),
                name: text("two"),
                vendor: VENDOR,
                url: text(""),
                version: text("0"),
                description: text(""),
                features: Cow::Borrowed(&[]),
            };
            &D
        }
        fn audio_ports(&self) -> &AudioPorts {
            &MONO_IN_OUT
        }
        fn activate(&mut self, _: &Activation) -> Result<(), StageError> {
            Ok(())
        }
        fn latency(&self) -> u32 {
            0
        }
        fn process(&mut self, _: ProcessIo<'_>) {}
        fn reset(&mut self) {}
        fn params(&self) -> &[ParamInfo] {
            &PARAMS
        }
        fn param_value(&self, id: ParamId) -> Option<f64> {
            if id == 5 { Some(0.0) } else { self.cell(id).map(|c| f64::from_bits(c.load(Relaxed))) }
        }
        fn set_param(&mut self, id: ParamId, v: f64) -> Result<(), ParamError> {
            let info = PARAMS.iter().find(|p| p.id == id).ok_or(ParamError::UnknownId(id))?;
            let v = info.accept(v)?;
            self.cell(id).ok_or(ParamError::ReadOnly(id))?.store(v.to_bits(), Relaxed);
            Ok(())
        }
    }

    fn blob(version: u16, entries: &[(u32, f64)]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend(version.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend((entries.len() as u32).to_le_bytes());
        for (id, v) in entries {
            out.extend(id.to_le_bytes());
            out.extend(v.to_le_bytes());
        }
        out
    }

    #[test]
    fn the_params_round_trip() {
        let mut s = Two::new();
        s.set_param(3, -4.5).unwrap();
        s.set_param(9, 3.0).unwrap();
        let mut saved = vec![0xAA]; // appended, not overwritten
        s.save_state(&mut saved);
        assert_eq!(saved.len(), 1 + HEADER + 3 * ENTRY);
        assert_eq!(&saved[1..], blob(1, &[(3, -4.5), (9, 3.0), (5, 0.0)]).as_slice(), "the layout, little-endian");
        let mut t = Two::new();
        t.load_state(&saved[1..]).unwrap();
        assert_eq!((t.param_value(3), t.param_value(9)), (Some(-4.5), Some(3.0)));
    }

    #[test]
    fn loading_ignores_unknown_ids_keeps_missing_ones_and_clamps() {
        let mut s = Two::new();
        s.set_param(9, 2.0).unwrap();
        s.load_state(&blob(1, &[(77, 1.0), (3, -50.0)])).unwrap();
        assert_eq!((s.param_value(3), s.param_value(9)), (Some(-10.0), Some(2.0)), "clamped; b kept");
        s.load_state(&blob(1, &[(9, 3.9), (5, 1.0), (77, f64::NAN)])).unwrap();
        assert_eq!(s.param_value(9), Some(3.0), "stepped: truncated; the read-only and unknown entries skipped");
        let mut long = blob(1, &[(3, -2.0)]);
        long.extend([1, 2, 3]);
        s.load_state(&long).unwrap();
        assert_eq!(s.param_value(3), Some(-2.0), "bytes after the entries are ignored");
    }

    #[test]
    fn loading_refuses_a_bad_blob_before_applying_any_of_it() {
        let mut s = Two::new();
        let refuse = |s: &mut Two, data: &[u8], why: &str| {
            let e = s.load_state(data).unwrap_err();
            assert!(e.0.contains(why), "{e} (wanted {why:?})");
        };
        refuse(&mut s, b"NFST", "too short");
        let mut magic = blob(1, &[]);
        magic[0] = b'X';
        refuse(&mut s, &magic, "bad magic");
        refuse(&mut s, &blob(2, &[(3, -2.0)]), "version 2");
        let whole = blob(1, &[(3, -2.0), (9, 1.0)]);
        refuse(&mut s, &whole[..whole.len() - 1], "truncated");
        refuse(&mut s, &blob(1, &[(3, -7.0), (9, f64::NAN)]), "param 9 is NaN");
        assert_eq!((s.param_value(3), s.param_value(9)), (Some(-1.0), Some(0.0)), "nothing applied");
        let mut huge = blob(1, &[]);
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        refuse(&mut s, &huge, "truncated");
    }
}
