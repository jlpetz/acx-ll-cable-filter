//! The processing thread is different from the graph builder, just as in a GUI host. Build FFT scratch there before audio starts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use llcable_effects::{EqSettings, echo_cancellation, equalizer, gate, noise_reduction, sonora_noise_reduction};
use llcable_routing::{Bus, Router, Send, Tap};

struct CountAlloc;
thread_local! { static CALLS: Cell<usize> = const { Cell::new(0) }; }

// SAFETY: each pointer and layout is forwarded unchanged to System. Const-initialized per-thread accounting cannot recurse.
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = CALLS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let _ = CALLS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.dealloc(p, layout) }
    }
}

#[global_allocator]
static ALLOC: CountAlloc = CountAlloc;

#[test]
fn voice_processing_makes_no_audio_thread_allocations_or_frees_including_the_first_packet() {
    for (name, effects) in [
        ("RNNoise", vec![noise_reduction()]),
        ("Sonora", vec![sonora_noise_reduction().unwrap()]),
        ("gate", vec![gate(-40.0).unwrap()]),
        ("stereo EQ", vec![equalizer(EqSettings { low_db: 6., mid_db: -3., high_db: 4. }).unwrap()]),
        ("AEC3", vec![echo_cancellation(Tap::Input(0)).unwrap()]),
        ("AEC3 and RNNoise", vec![echo_cancellation(Tap::Input(0)).unwrap(), noise_reduction()]),
    ] {
        let (effects, meters): (Vec<_>, Vec<_>) = effects.into_iter().map(|fx| fx.observe()).unzip();
        let mut buses = vec![Bus { sends: vec![Send::unity(Tap::Input(0))], effects }];
        if meters.len() > 1 {
            buses.push(Bus { sends: vec![Send::unity(Tap::Stage(0, 0))], effects: vec![] });
        }
        let mut router = Router::new(48_000, 128, 1, buses).unwrap();
        let count = std::thread::spawn(move || {
            router.prepare_thread();
            let inputs = std::array::from_fn::<_, 128, _>(|i| [(i as f32 * 0.25).sin() * 0.3, (i as f32 * 0.125).cos() * 0.4]);
            let silence = [[0.0; 2]; 128];
            let start = CALLS.get();
            for i in 0..720 {
                let x = if (120..=500).contains(&i) { &inputs } else { &silence };
                router.process_stereo(&[x]).unwrap();
                std::hint::black_box(router.output(0).unwrap());
                for meter in &meters {
                    std::hint::black_box(meter.read());
                }
            }
            CALLS.get() - start
        })
        .join()
        .unwrap();
        assert_eq!(count, 0, "{name} allocated or freed during processing");
    }
}
