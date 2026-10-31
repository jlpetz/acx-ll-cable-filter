//! Handing a ring's producer to the audio thread and back, lock-free (engine.md "Latency test", "The chain recorder").
//!
//! The control side allocates a ring (and touches its pages) and hands its `Producer` over on a one-slot ring of producers.
//! The audio thread keeps it while it's wanted and hands it back on a second one-slot ring, so the ring is freed on the
//! control side and never on the audio thread. The latency test's tap and the chain recorder each have a pair per run. What
//! goes over is a [`Handed`]: the producer itself (the tap's), or the producer with what travels with it (the recorder's).

use rtrb::{Consumer, Producer, PushError, RingBuffer};

/// What a hand-over carries: a ring's producer, finished when the ring's consumer has gone.
pub(crate) trait Handed {
    fn is_abandoned(&self) -> bool;
}

impl<T> Handed for Producer<T> {
    fn is_abandoned(&self) -> bool {
        Producer::is_abandoned(self)
    }
}

/// The control side of a hand-over: producers go in on `cmd` and come back on `ret`.
pub(crate) struct Port<H> {
    pub(crate) cmd: Producer<H>,
    pub(crate) ret: Consumer<H>,
}

/// The audio thread's side of a hand-over.
pub(crate) struct End<H> {
    pub(crate) cmd: Consumer<H>,
    pub(crate) ret: Producer<H>,
}

/// One run's hand-over rings (allocated on the control thread).
pub(crate) fn channel<H>() -> (Port<H>, End<H>) {
    let (cmd_tx, cmd_rx) = RingBuffer::new(1);
    let (ret_tx, ret_rx) = RingBuffer::new(1);
    (Port { cmd: cmd_tx, ret: ret_rx }, End { cmd: cmd_rx, ret: ret_tx })
}

/// Once per wake, on the audio thread: takes a new producer from the control side into `slot`, and hands a finished one back
/// so that its ring is freed on the control side, never here. A producer is finished when the control side asks for it
/// (`stop`) or its consumer has gone. `returning` holds one the return ring had no room for, for the next wake. Lock-free
/// and allocation-free.
pub(crate) fn service<H: Handed>(slot: &mut Option<H>, end: &mut End<H>, stop: bool, returning: &mut Option<H>) {
    // Only take a new one while nothing waits to go back, so a replaced one is never dropped here.
    if returning.is_none()
        && let Ok(p) = end.cmd.pop()
    {
        *returning = slot.replace(p);
    }
    let done = slot.as_ref().is_some_and(|p| p.is_abandoned() || stop);
    if done && returning.is_none() {
        *returning = slot.take();
    }
    if let Some(p) = returning.take()
        && let Err(PushError::Full(p)) = end.ret.push(p)
    {
        *returning = Some(p);
    }
}

/// When a run ends, on its audio thread: hands back a producer the run never took (the start failed before the loop, or
/// the run stopped before its next wake), so the control side can keep it for the next run. (Not real-time: the run is
/// over. With the return ring full, it's dropped here; that happens only if the control side hadn't collected the last
/// one handed back.)
pub(crate) fn give_back<H>(end: &mut End<H>) {
    if let Ok(p) = end.cmd.pop() {
        let _ = end.ret.push(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_producer_goes_in_and_comes_back() {
        let (mut port, mut end) = channel::<Producer<u32>>();
        let (mut slot, mut returning) = (None, None);
        let (tx, rx) = RingBuffer::<u32>::new(4);
        assert!(port.cmd.push(tx).is_ok());
        service(&mut slot, &mut end, false, &mut returning);
        assert!(slot.is_some() && returning.is_none(), "taken");
        service(&mut slot, &mut end, false, &mut returning);
        assert!(slot.is_some(), "kept while it's wanted");
        // The control side asks for it back.
        service(&mut slot, &mut end, true, &mut returning);
        assert!(slot.is_none() && returning.is_none());
        assert!(port.ret.pop().is_ok(), "returned to the control side");
        drop(rx);
    }

    #[test]
    fn an_abandoned_producer_is_returned_not_dropped() {
        let (mut port, mut end) = channel::<Producer<u32>>();
        let (mut slot, mut returning) = (None, None);
        let (tx, rx) = RingBuffer::<u32>::new(1);
        assert!(port.cmd.push(tx).is_ok());
        service(&mut slot, &mut end, false, &mut returning);
        drop(rx);
        service(&mut slot, &mut end, false, &mut returning);
        assert!(slot.is_none());
        assert!(port.ret.pop().is_ok());
    }

    #[test]
    fn a_full_return_ring_keeps_the_producer_for_later() {
        let (mut port, mut end) = channel::<Producer<u32>>();
        let (mut slot, mut returning) = (None, None);
        // Fill the return ring with an unrelated producer the control side hasn't collected.
        let (stale, _stale_rx) = RingBuffer::<u32>::new(1);
        assert!(end.ret.push(stale).is_ok());
        let (tx, _rx) = RingBuffer::<u32>::new(1);
        assert!(port.cmd.push(tx).is_ok());
        service(&mut slot, &mut end, false, &mut returning);
        service(&mut slot, &mut end, true, &mut returning);
        assert!(returning.is_some(), "held, not dropped");
        assert!(port.ret.pop().is_ok());
        service(&mut slot, &mut end, true, &mut returning);
        assert!(returning.is_none());
        assert!(port.ret.pop().is_ok());
    }

    /// A new producer while one waits to go back stays in the command ring until that one is returned.
    #[test]
    fn a_new_producer_waits_for_the_last_to_go_back() {
        let (mut port, mut end) = channel::<Producer<u32>>();
        let (mut slot, mut returning) = (None, None);
        let (stale, _stale_rx) = RingBuffer::<u32>::new(1);
        assert!(end.ret.push(stale).is_ok());
        let (a, _a_rx) = RingBuffer::<u32>::new(1);
        assert!(port.cmd.push(a).is_ok());
        service(&mut slot, &mut end, false, &mut returning);
        // `a` is asked back while the return ring is full; `b` comes in meanwhile.
        service(&mut slot, &mut end, true, &mut returning);
        let (b, _b_rx) = RingBuffer::<u32>::new(1);
        assert!(port.cmd.push(b).is_ok());
        service(&mut slot, &mut end, false, &mut returning);
        assert!(slot.is_none() && returning.is_some(), "b waits");
        assert!(port.ret.pop().is_ok(), "the stale one");
        service(&mut slot, &mut end, false, &mut returning);
        assert!(port.ret.pop().is_ok(), "a, returned");
        service(&mut slot, &mut end, false, &mut returning);
        assert!(slot.is_some(), "b taken");
    }

    /// A run that ends without taking its producer (a failed start) gives it back; one it took is left alone.
    #[test]
    fn a_producer_the_run_never_took_is_given_back() {
        let (mut port, mut end) = channel::<Producer<u32>>();
        let (tx, _rx) = RingBuffer::<u32>::new(1);
        assert!(port.cmd.push(tx).is_ok());
        give_back(&mut end);
        assert!(port.ret.pop().is_ok_and(|p| !p.is_abandoned()), "back, alive");
        give_back(&mut end);
        assert!(port.ret.pop().is_err(), "nothing else");
    }
}
