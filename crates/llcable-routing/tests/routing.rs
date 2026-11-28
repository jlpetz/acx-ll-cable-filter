use llcable_routing::{Bus, BusSpec, Ducking, Effect, EffectSpec, GraphSpec, Input, PacketEffect, Router, RoutingError, Send, Tap};

fn passthrough(from: Tap) -> BusSpec {
    BusSpec { sends: vec![Send::unity(from)], effects: vec![] }
}

#[test]
fn dry_split_preserves_stereo_bits_and_merging_never_changes_the_dry_output() {
    let mut quarter = Send::unity(Tap::Bus(0));
    quarter.gain = 0.25;
    let graph = GraphSpec {
        input_count: 2,
        buses: vec![passthrough(Tap::Input(0)), BusSpec { sends: vec![quarter], effects: vec![] }, BusSpec {
            sends: vec![Send::unity(Tap::Bus(1)), Send { from: Tap::Input(1), gain: 0.5, pan: 1.0 }],
            effects: vec![],
        }],
    };
    let mut router = graph.build(48_000, 35).unwrap();
    for n in [1, 11, 35, 3, 0] {
        let mut original = [[-0.25, 0.75]; 35];
        original[0] = [-0.0, f32::from_bits(7)];
        let mono = [0.2; 35];
        router.process(&[Input::Stereo(&original[..n]), Input::Mono(&mono[..n])]).unwrap();
        for (x, y) in router.output(0).unwrap().iter().zip(original) {
            assert_eq!(x.map(f32::to_bits), y.map(f32::to_bits));
        }
        assert_eq!(router.output(0).unwrap().len(), n);
        for (x, out) in original.iter().zip(router.output(2).unwrap()) {
            assert_eq!(out[0], x[0] * 0.25);
            assert_eq!(out[1], x[1] * 0.25 + 0.1);
        }
    }
}

#[test]
fn source_bounds_cycles_and_invalid_controls_fail_at_build_time() {
    for (source, expected) in [
        (Tap::Input(1), RoutingError::InvalidTap { bus: 0, tap: Tap::Input(1) }),
        (Tap::Bus(0), RoutingError::InvalidTap { bus: 0, tap: Tap::Bus(0) }),
    ] {
        let build = GraphSpec { input_count: 1, buses: vec![passthrough(source)] }.build(48_000, 128);
        assert_eq!(build.err(), Some(expected));
    }
    let invalid = Ducking { release_ms: f32::NAN, ..Ducking::default() };
    let spec = GraphSpec {
        input_count: 1,
        buses: vec![BusSpec {
            sends: vec![Send::unity(Tap::Input(0))],
            effects: vec![EffectSpec::Duck { key: Tap::Input(0), settings: invalid }],
        }],
    };
    assert_eq!(spec.build(48_000, 128).err(), Some(RoutingError::InvalidEffect { bus: 0 }));
}

struct Offset;
impl PacketEffect for Offset {
    fn process(&mut self, stereo: &mut [[f32; 2]], _: Option<Input<'_>>) {
        for frame in stereo {
            frame[1] += 0.5;
        }
    }
}

#[test]
fn custom_effects_run_in_order_and_input_failures_do_not_disturb_previous_packet() {
    let mut r = Router::new(48_000, 3, 2, vec![Bus {
        sends: vec![Send::unity(Tap::Input(0))],
        effects: vec![Effect::Custom(Box::new(Offset)), Effect::Gain(2.0)],
    }])
    .unwrap();
    let input = [[0.0, 0.25]; 2];
    r.process_stereo(&[&input, &input]).unwrap();
    assert_eq!(r.output(0).unwrap(), [[0.0, 1.5]; 2]);
    assert_eq!(r.process_stereo(&[&input]), Err(RoutingError::InputCount));
    assert_eq!(r.process(&[Input::Stereo(&input), Input::Silence(1)]), Err(RoutingError::UnequalFrames));
    assert_eq!(r.process(&[Input::Silence(4), Input::Silence(4)]), Err(RoutingError::TooManyFrames));
    assert_eq!(r.output(0).unwrap(), [[0.0, 1.5]; 2]);
}

#[test]
fn ducking_uses_only_the_key_and_keeps_its_envelope_across_unequal_packet_lengths() {
    let settings = Ducking { threshold_db: -20.0, attenuation_db: 6.020_6, attack_ms: 4.0, hold_ms: 2.0, release_ms: 4.0 };
    let spec = GraphSpec {
        input_count: 2,
        buses: vec![passthrough(Tap::Input(0)), BusSpec {
            sends: vec![Send::unity(Tap::Input(1))],
            effects: vec![EffectSpec::Duck { key: Tap::Bus(0), settings }],
        }],
    };
    let mut router = spec.build(1000, 16).unwrap();
    let key = [0.0, 0.0, 1.0, -0.8, 0.9, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let source = [[1.0, -0.5]; 12];
    let expected = [1.0, 1.0, 0.875, 0.75, 0.625, 0.5, 0.5, 0.5, 0.625, 0.75, 0.875, 1.0];
    for (start, end) in [(0, 5), (5, 6), (6, 9), (9, 12)] {
        router.process(&[Input::Mono(&key[start..end]), Input::Stereo(&source[start..end])]).unwrap();
        for (out, &gain) in router.output(1).unwrap().iter().zip(&expected[start..end]) {
            assert!((out[0] - gain).abs() < 1e-6);
            assert!((out[1] + 0.5 * gain).abs() < 1e-6);
        }
    }
    router.process(&[Input::Mono(&key), Input::Silence(12)]).unwrap();
    assert_eq!(router.output(1).unwrap(), [[0.0; 2]; 12], "key never leaks into a silent bus");
}

#[test]
fn observed_gain_and_duck_report_their_own_signal_and_preserve_the_sidechain() {
    let (gain, mic) = Effect::Gain(0.5).observe();
    let (duck, music) =
        Effect::Duck { key: Tap::Input(1), settings: Ducking { attack_ms: 0.0, attenuation_db: 6.020_6, ..Default::default() } }.observe();
    assert_eq!(duck.key(), Some(Tap::Input(1)));
    let bus = Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![gain, duck] };
    let mut router = Router::new(48_000, 128, 2, vec![bus]).unwrap();
    assert_eq!(music.read().output_peak, None);
    router.process(&[Input::Stereo(&[[0.25, -0.5]; 128]), Input::Mono(&[0.2; 128])]).unwrap();
    let before = mic.read();
    let after = music.read();
    assert_eq!((before.input_peak, before.output_peak, before.gain), (Some(0.5), Some(0.25), Some(0.5)));
    assert_eq!(after.input_peak, before.output_peak);
    assert!((after.gain.unwrap() - 0.5).abs() < 1e-6);
    assert!((after.output_peak.unwrap() - 0.125).abs() < 1e-6);
}
