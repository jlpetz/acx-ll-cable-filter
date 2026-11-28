use llcable_effects::{echo_cancellation, gate, noise_reduction, sonora_noise_reduction};
use llcable_routing::{Bus, Effect, Input, Router, RoutingError, Send, Tap};

fn routed(effect: Effect) -> Router {
    let dry = Send::unity(Tap::Input(0));
    let buses = vec![Bus { sends: vec![dry], effects: vec![] }, Bus { sends: vec![dry], effects: vec![effect] }];
    let mut router = Router::new(48_000, 480, 1, buses).unwrap();
    router.prepare_thread();
    router
}

fn signal(n: usize) -> Vec<[f32; 2]> {
    // Voiced harmonics plus a deterministic, nonperiodic background. Different channels verify the dry split.
    let mut r = 31_419u32;
    (0..n)
        .map(|i| {
            r = r.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = 0.02 * (r as i32 as f32 / i32::MAX as f32);
            let t = i as f32 / 48_000.0;
            let s = 0.2 * (t * 2.0 * std::f32::consts::PI * 220.0).sin() + 0.08 * (t * 2.0 * std::f32::consts::PI * 440.0).sin();
            [s + noise, s * 0.8 - 0.5 * noise]
        })
        .collect()
}

#[test]
fn rnnoise_and_webrtc_are_continuous_across_device_packets_and_do_not_delay_the_dry_split() {
    for create in [noise_reduction as fn() -> Effect, || sonora_noise_reduction().unwrap()] {
        let input = signal(96_000);
        let mut whole = routed(create());
        let mut split = routed(create());
        let mut reference = Vec::with_capacity(input.len());
        for chunk in input.chunks(480) {
            whole.process_stereo(&[chunk]).unwrap();
            reference.extend_from_slice(whole.output(1).unwrap());
        }
        assert!(reference[..480].iter().all(|&v| v == [0.0; 2]));
        assert!(reference[4800..].iter().any(|v| v[0].abs() > 0.001), "voice stage produced no signal");
        assert!(reference.iter().all(|v| v[0].is_finite() && v[0] == v[1]));
        for (chunk, expected) in input.chunks(127).zip(reference.chunks(127)) {
            split.process_stereo(&[chunk]).unwrap();
            assert_eq!(split.output(0).unwrap(), chunk);
            assert_eq!(split.output(1).unwrap(), expected);
        }
        let mut silence = routed(create());
        for _ in 0..100 {
            silence.process(&[Input::Silence(480)]).unwrap();
            assert!(silence.output(0).unwrap().iter().all(|&v| v == [0.0; 2]));
            assert!(silence.output(1).unwrap().iter().all(|v| v[0].abs() < 1e-7 && v[1].abs() < 1e-7));
        }
    }
}

#[test]
fn rnnoise_after_speech_settles_below_the_dither_floor_while_dry_stays_exact() {
    let mut router = routed(noise_reduction());
    for x in signal(48_000).chunks(128) {
        router.process_stereo(&[x]).unwrap();
    }
    let mut peak = 0.0f32;
    for i in 0..1000 {
        router.process(&[Input::Silence(480)]).unwrap();
        assert!(router.output(0).unwrap().iter().all(|x| x[0].to_bits() == 0 && x[1].to_bits() == 0));
        if i >= 500 {
            for frame in router.output(1).unwrap() {
                peak = peak.max(frame[0].abs());
            }
        }
    }
    assert!(peak < 1e-8, "RNNoise tail peak {peak:e} exceeds −160 dBFS after five silent seconds");
}

#[test]
fn echo_cancellation_uses_an_aligned_reference_across_packet_boundaries() {
    let len = 7 * 48_000;
    let room = nf_stages::harness::scenes::echo_room(len, 0.15, 1);
    let mic = room.mic();
    let (fx, meter) = echo_cancellation(Tap::Input(1)).unwrap().observe();
    let mut router = Router::new(48_000, 127, 2, vec![Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![fx] }]).unwrap();
    router.prepare_thread();
    let mut filtered = Vec::with_capacity(len);
    for (mic, reference) in mic.chunks(127).zip(room.far.samples.chunks(127)) {
        router.process(&[Input::Mono(mic), Input::Mono(reference)]).unwrap();
        filtered.extend(router.output(0).unwrap().iter().map(|s| s[0]));
    }
    let lag = nf_core::BLOCK + nf_stages::AEC3_LATENCY as usize;
    let span = 3 * 48_000..len - lag;
    let energy = |samples: &[f32]| samples.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
    let attenuation_db = 10.0 * (energy(&mic[span.clone()]) / energy(&filtered[span.start + lag..])).log10();
    assert!(attenuation_db > 15.0, "AEC3 attenuated the synthetic room echo by {attenuation_db:.1} dB");
    let reading = meter.read();
    assert!(reading.echo_delay_ms.is_some_and(|delay| (30.0..50.0).contains(&delay)));
    assert!(reading.erle_db.is_some_and(|db| db > 5.0));
}

#[test]
fn stereo_gate_links_the_loudest_channel_and_opens_without_a_block_wait() {
    assert!(gate(f32::NAN).is_err());
    let (effect, meter) = gate(-30.0).unwrap().observe();
    let mut router = routed(effect);
    let below = [[0.0001, -0.0001]; 480];
    for _ in 0..30 {
        router.process_stereo(&[&below]).unwrap();
    }
    let quiet = router.output(1).unwrap()[479];
    assert!(quiet[0] < 0.000002 && quiet[1] == -quiet[0]);
    let reading = meter.read();
    assert_eq!(reading.gate_open, Some(false));
    assert_eq!(reading.output_peak, Some(quiet[0]));
    assert_eq!(reading.input_peak, Some(0.0001));
    assert!(reading.gain.is_some_and(|gain| gain < 0.02));
    let signal = [[0.4, -0.0001]; 160];
    router.process_stereo(&[&signal]).unwrap();
    let audible = router.output(1).unwrap();
    assert!(audible[0][0] > 0.0);
    assert_eq!(audible[159], signal[159], "loud left opens the right too");
    let reading = meter.read();
    assert_eq!((reading.gate_open, reading.gain), (Some(true), Some(1.0)));
}

#[test]
fn fused_denoisers_report_separate_hop_levels_and_only_rnnoise_reports_speech_probability() {
    let (first, voice) = noise_reduction().observe();
    let (second, web_rtc) = sonora_noise_reduction().unwrap().observe();
    let bus = Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![first, second] };
    let mut router = Router::new(48_000, 128, 1, vec![bus]).unwrap();
    router.prepare_thread();
    assert_eq!(voice.read().input_peak, None);
    let mut observed_speech = false;
    for chunk in signal(96_000).chunks(128) {
        router.process_stereo(&[chunk]).unwrap();
        let a = voice.read();
        let b = web_rtc.read();
        if let Some(probability) = a.speech_probability {
            assert!((0.0..=1.0).contains(&probability));
            observed_speech |= probability > 0.1;
            assert_eq!(a.output_peak, b.input_peak);
            assert!(b.output_peak.is_some());
            assert_eq!(b.speech_probability, None);
        }
    }
    assert!(observed_speech);
}
#[test]
fn voice_stages_require_their_actual_processing_rate() {
    for effect in [noise_reduction(), sonora_noise_reduction().unwrap(), gate(-40.0).unwrap()] {
        let result = Router::new(44_100, 128, 1, vec![Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![effect] }]);
        assert_eq!(result.err(), Some(RoutingError::EffectSampleRate { bus: 0, required: 48_000 }));
    }
}
