//! A single control caller prepares each replacement and collects the displaced graph. The audio thread only swaps
//! prepared buffers between packets. Both queues have one slot; the control side waits for the reply before sending again.
use std::sync::{Mutex, RwLock};
use std::thread;
use std::time::Duration;

use llcable_routing::{Router, RoutingError};
use rtrb::{Consumer, Producer, RingBuffer};

use super::RouteConfig;
use super::outputs::{Destination, Output, Plan};

pub(super) struct StreamFormat {
    pub max_frames: usize,
    pub period: usize,
    pub origins: Vec<Option<String>>,
}

struct Edit {
    router: Router,
    destinations: Plan,
    outputs: Vec<Output>,
    retained: Vec<(usize, usize)>,
    result: Result<(), RoutingError>,
}

pub(super) struct Control(Mutex<Option<Pending>>);
struct Pending {
    send: Producer<Edit>,
    replies: Consumer<Edit>,
}
pub(super) struct Audio {
    receive: Consumer<Edit>,
    reply: Producer<Edit>,
}

pub(super) fn channel() -> (Control, Audio) {
    let (send, receive) = RingBuffer::new(1);
    let (reply, replies) = RingBuffer::new(1);
    (Control(Mutex::new(Some(Pending { send, replies }))), Audio { receive, reply })
}

impl Control {
    pub fn apply(
        &self, format: &StreamFormat, current: &RwLock<Vec<Destination>>, config: RouteConfig, retained: Vec<(usize, usize)>,
    ) -> Result<(), String> {
        let mut channel = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let pending = channel.as_mut().ok_or("The routing session has stopped.")?;
        if pending.replies.is_abandoned() {
            return Err("The routing session has stopped.".into());
        }
        let previous = current.read().unwrap_or_else(|e| e.into_inner()).clone();
        let destinations = Plan::prepare(&config, &previous, &format.origins, &[])?;
        let router = Router::new(48_000, format.max_frames, format.origins.len(), config.buses).map_err(|e| e.to_string())?;
        let outputs = destinations.open(format)?;
        let edit = Edit { router, destinations, outputs, retained, result: Ok(()) };
        pending.send.push(edit).map_err(|_| "A previous route change is still pending.".to_owned())?;
        loop {
            // Check abandonment before the final pop. A worker can send its last reply and exit between the two reads.
            let ended = pending.replies.is_abandoned();
            if let Ok(edit) = pending.replies.pop() {
                edit.result.map_err(|e| e.to_string())?;
                *current.write().unwrap_or_else(|e| e.into_inner()) = edit.destinations.destinations;
                return Ok(());
            }
            if ended {
                // The audio side can exit before consuming the request. Reclaim that prepared graph on the control thread.
                *channel = None;
                return Err("The routing session ended before the change could be applied.".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Audio {
    pub fn apply_pending(&mut self, router: &mut Router, outputs: &mut Vec<Output>) {
        if let Ok(mut edit) = self.receive.pop() {
            edit.result = router.activate_edit(&mut edit.router, &edit.retained);
            if edit.result.is_ok() {
                edit.destinations.commit(outputs, &mut edit.outputs);
            }
            // There is only one outstanding edit. The caller has consumed the previous reply before it can enqueue another.
            assert!(self.reply.push(edit).is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread::ThreadId;
    use std::time::Instant;

    use llcable_routing::{Bus, Effect, Input, PacketEffect, Send, Tap};

    use super::*;
    use crate::CableId;

    struct ReportDrop(mpsc::Sender<ThreadId>);

    impl PacketEffect for ReportDrop {
        fn process(&mut self, _: &mut [[f32; 2]], _: Option<Input<'_>>) {}
    }

    impl Drop for ReportDrop {
        fn drop(&mut self) {
            let _ = self.0.send(thread::current().id());
        }
    }

    fn config(effect: Effect) -> RouteConfig {
        let bus = |effect| Bus { sends: vec![Send::unity(Tap::Input(0))], effects: vec![effect] };
        RouteConfig {
            inputs: vec!["source".into()],
            cables: vec![CableId(1), CableId(2)],
            output_buses: vec![1, 0],
            buses: vec![bus(effect), bus(Effect::Gain(0.5))],
            playback: vec![],
        }
    }

    fn wait_for_request(audio: &Audio) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while audio.receive.is_empty() {
            assert!(Instant::now() < deadline, "control did not enqueue the prepared edit");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn applying_an_edit_moves_the_outputs_together_and_drops_the_displaced_graph_on_control() {
        let (control, audio) = channel();
        let (drop_notice, dropped) = mpsc::channel();
        let mut old_config = config(Effect::Custom(Box::new(ReportDrop(drop_notice))));
        old_config.output_buses = vec![0, 1];
        let format = StreamFormat { max_frames: 2, period: 2, origins: vec![None] };
        let old = Plan::prepare(&old_config, &[], &format.origins, &old_config.cables).unwrap();
        let mut outputs = old.open(&format).unwrap();
        let current = RwLock::new(old.destinations);
        let mut router = Router::new(48_000, 2, 1, old_config.buses).unwrap();
        thread::scope(|scope| {
            let mut audio = audio;
            let work = scope.spawn(|| control.apply(&format, &current, config(Effect::Gain(2.)), vec![]));
            let caller = work.thread().id();
            wait_for_request(&audio);
            audio.apply_pending(&mut router, &mut outputs);
            drop(audio);
            work.join().unwrap().unwrap();
            assert_eq!(dropped.try_recv().unwrap(), caller);
        });
        router.process_stereo(&[&[[1., -1.]]]).unwrap();
        assert_eq!(router.output(outputs[0].bus).unwrap(), [[0.5, -0.5]]);
        assert_eq!(router.output(outputs[1].bus).unwrap(), [[2., -2.]]);
    }

    #[test]
    fn a_rejected_rack_change_leaves_removed_outputs_and_status_in_place() {
        let (control, mut audio) = channel();
        let format = StreamFormat { max_frames: 2, period: 2, origins: vec![None] };
        let initial = config(Effect::Gain(1.));
        let old = Plan::prepare(&initial, &[], &format.origins, &initial.cables).unwrap();
        let mut outputs = old.open(&format).unwrap();
        let current = RwLock::new(old.destinations);
        let mut router = Router::new(48_000, 2, 1, initial.buses).unwrap();
        let mut replacement = config(Effect::Gain(2.));
        replacement.cables.pop();
        replacement.output_buses.pop();
        thread::scope(|scope| {
            let work = scope.spawn(|| control.apply(&format, &current, replacement, vec![(0, 999)]));
            wait_for_request(&audio);
            audio.apply_pending(&mut router, &mut outputs);
            assert!(work.join().unwrap().is_err());
        });
        assert_eq!(current.read().unwrap().len(), 2);
        assert_eq!(outputs.len(), 2);
        router.process_stereo(&[&[[1., -1.]]]).unwrap();
        assert_eq!(router.output(outputs[1].bus).unwrap(), [[1., -1.]]);
        // Both original writers still own their paused slots and can accept the next packet.
        for output in &mut outputs {
            output.write(&router);
        }
    }

    #[test]
    fn shutdown_before_an_edit_is_consumed_releases_its_effects_on_the_waiting_caller() {
        let (control, audio) = channel();
        let (drop_notice, dropped) = mpsc::channel();
        let format = StreamFormat { max_frames: 2, period: 2, origins: vec![None] };
        let initial = config(Effect::Gain(1.));
        let current = RwLock::new(Plan::prepare(&initial, &[], &format.origins, &initial.cables).unwrap().destinations);
        thread::scope(|scope| {
            let work = scope.spawn(|| control.apply(&format, &current, config(Effect::Custom(Box::new(ReportDrop(drop_notice)))), vec![]));
            let caller = work.thread().id();
            wait_for_request(&audio);
            drop(audio);
            assert!(work.join().unwrap().unwrap_err().contains("ended"));
            assert_eq!(dropped.try_recv().unwrap(), caller);
        });
    }
}
