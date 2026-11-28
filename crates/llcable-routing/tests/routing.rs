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
fn tapping_a_running_stage_keeps_the_input_clock_and_follows_exact_logical_positions() {
    let source = || rack(0, vec![Effect::Gain(1.), Effect::Gain(0.5), Effect::Gain(2.)]);
    let mut live = Router::new(48_000, 8, 1, vec![source()]).unwrap();
    let mut next = Router::new(48_000, 8, 1, vec![source(), Bus { sends: vec![Send::unity(Tap::Stage(0, 1))], effects: vec![] }]).unwrap();
    live.activate_edit(&mut next, &[(0, 0)]).unwrap();
    let signal = [[0.125, -0.5]; 7];
    live.process_stereo(&[&signal]).unwrap();
    assert_eq!(live.output(0).unwrap(), signal);
    assert_eq!(live.output(1).unwrap(), [[0.0625, -0.25]; 7]);
    let mut shorter = Router::new(48_000, 8, 1, vec![rack(0, vec![Effect::Gain(1.), Effect::Gain(1.), Effect::ActivePrefix(2)])]).unwrap();
    live.activate_edit(&mut shorter, &[(0, 0)]).unwrap();
    live.process_stereo(&[&signal[..2]]).unwrap();
    assert_eq!(live.output(0).unwrap(), [[0.0625, -0.25]; 2]);
    let tap = Tap::Stage(0, 1);
    assert_eq!(
        Router::new(48_000, 8, 1, vec![rack(0, vec![Effect::Gain(1.), Effect::Gain(2.), Effect::ActivePrefix(1)]), Bus {
            sends: vec![Send::unity(tap)],
            effects: vec![],
        }])
        .err(),
        Some(RoutingError::InvalidTap { bus: 1, tap })
    );
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

struct CountPackets(u32);

impl PacketEffect for CountPackets {
    fn process(&mut self, stereo: &mut [[f32; 2]], _: Option<Input<'_>>) {
        self.0 += 1;
        for f in stereo {
            f[0] += self.0 as f32;
        }
    }
    // Uses the default live-activation refusal. A retained instance is already running on its prepared thread.
}

fn rack(input: usize, effects: Vec<Effect>) -> Bus {
    Bus { sends: vec![Send::unity(Tap::Input(input))], effects }
}

#[test]
fn hot_move_preserves_processor_history_and_meter_and_uses_the_new_sends() {
    let (original, meter) = Effect::Custom(Box::new(CountPackets(0))).observe();
    let mut active = Router::new(48_000, 3, 2, vec![rack(0, vec![original])]).unwrap();
    let source_a = [[2., -3.]];
    let source_b = [[8., -6.]];
    let inputs = [&source_a[..], &source_b[..]];
    active.prepare_thread();
    active.process_stereo(&inputs).unwrap();
    assert_eq!(active.output(0).unwrap(), [[3., -3.]]);

    let placeholder = Effect::Custom(Box::new(CountPackets(100)));
    let mut next = Router::new(48_000, 3, 2, vec![rack(1, vec![Effect::Gain(0.5)]), rack(1, vec![placeholder])]).unwrap();
    active.activate_edit(&mut next, &[(0, 1)]).unwrap();
    active.process_stereo(&inputs).unwrap();
    assert_eq!(active.output(0).unwrap(), [[4., -3.]]);
    assert_eq!(active.output(1).unwrap(), [[10., -6.]]);
    assert_eq!(meter.read().output_peak, Some(10.));
}

#[test]
fn late_thread_setup_refuses_the_whole_edit_before_any_rack_moves() {
    let counter = |n| Effect::Custom(Box::new(CountPackets(n)));
    let mut active = Router::new(48_000, 1, 1, vec![rack(0, vec![counter(0)])]).unwrap();
    let mut next = Router::new(48_000, 1, 1, vec![rack(0, vec![Effect::Gain(2.)]), rack(0, vec![counter(99)])]).unwrap();
    let input = [&[[1., 1.]][..]];
    active.process_stereo(&input).unwrap();
    assert_eq!(active.activate_edit(&mut next, &[(0, 0)]), Err(RoutingError::EffectNeedsThreadSetup { bus: 1 }));
    active.process_stereo(&input).unwrap();
    next.process_stereo(&input).unwrap();
    assert_eq!(active.output(0).unwrap(), [[3., 1.]]);
    assert_eq!(next.output(0).unwrap(), [[2., 2.]]);
}

#[test]
fn a_retained_sidechain_cannot_follow_an_unrelated_rack_into_its_old_bus_index() {
    let duck = || Effect::Duck { key: Tap::Bus(0), settings: Ducking::default() };
    let mut running = Router::new(48_000, 128, 1, vec![rack(0, vec![]), rack(0, vec![duck()])]).unwrap();
    let mut next = Router::new(48_000, 128, 1, vec![rack(0, vec![Effect::Gain(0.)]), rack(0, vec![]), rack(0, vec![duck()])]).unwrap();
    assert_eq!(running.activate_edit(&mut next, &[(0, 1), (1, 2)]), Err(RoutingError::InvalidRetainedRack { old: 1, new: 2 }));
}

#[test]
fn malformed_rack_mappings_and_changed_keys_or_stream_format_leave_both_graphs_intact() {
    let router = |rate, frames, count, gain, key| {
        let settings = Ducking { attenuation_db: 0., ..Default::default() };
        Router::new(rate, frames, count, vec![
            rack(0, vec![Effect::Gain(gain)]),
            rack(0, vec![Effect::Duck { key, settings }]),
            rack(0, vec![]),
        ])
        .unwrap()
    };
    let mut active = router(48_000, 2, 2, 2., Tap::Input(0));
    let mut next = router(48_000, 2, 2, 3., Tap::Input(1));
    for bad in [(0, 3), (3, 0), (0, 2), (2, 0), (1, 1)] {
        assert_eq!(active.activate_edit(&mut next, &[(0, 0), bad]), Err(RoutingError::InvalidRetainedRack { old: bad.0, new: bad.1 }));
        for (r, expected_gain) in [(&mut active, 2.), (&mut next, 3.)] {
            r.process_stereo(&[&[[1., -1.]], &[[0., 0.]]]).unwrap();
            assert_eq!(r.output(0).unwrap(), [[expected_gain, -expected_gain]]);
        }
    }
    // Move matching unkeyed racks after refusals, proving that the scratch flags from failed attempts are harmless.
    active.activate_edit(&mut next, &[(0, 0)]).unwrap();
    for (rate, frames, count) in [(44_100, 2, 2), (48_000, 1, 2), (48_000, 2, 1)] {
        let mut incompatible = router(rate, frames, count, 1., Tap::Input(0));
        assert_eq!(active.activate_edit(&mut incompatible, &[]), Err(RoutingError::IncompatibleStream));
    }
}
