//! Check the public processing entry points under allocation counting, including the first packet.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use llcable_routing::{BusSpec, Ducking, EffectSpec, GraphSpec, Input, Send, Tap};

thread_local! {
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountAlloc;
// SAFETY: unchanged layout/pointer forwarded to System, thread-local accounting has no allocator dependency.
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
fn dry_split_mix_and_sidechain_neither_allocate_nor_free_on_first_or_later_packets() {
    let mut router = GraphSpec {
        input_count: 2,
        buses: vec![BusSpec { sends: vec![Send::unity(Tap::Input(0))], effects: vec![] }, BusSpec {
            sends: vec![Send::unity(Tap::Bus(0)), Send::unity(Tap::Input(1))],
            effects: vec![EffectSpec::Duck { key: Tap::Input(0), settings: Ducking::default() }],
        }],
    }
    .build(48_000, 480)
    .unwrap();
    let stereo = [[0.6, 0.2]; 480];
    let before = CALLS.get();
    for frames in [1, 128, 480, 3, 0, 160] {
        router.process(&[Input::Stereo(&stereo[..frames]), Input::Silence(frames)]).unwrap();
        router.process_stereo(&[&stereo[..frames], &stereo[..frames]]).unwrap();
        std::hint::black_box(router.output(1).unwrap());
    }
    let after = CALLS.get();
    assert_eq!(after, before);
}
