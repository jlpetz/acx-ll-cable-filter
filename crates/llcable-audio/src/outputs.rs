//! The control thread prepares the destination list. At a packet boundary the audio thread moves retained
//! writers into the prepared slots and returns retired workers for the control thread to release.
use std::sync::Arc;

use llcable_routing::Router;

use super::edits::StreamFormat;
use super::playback::External;
use super::{CableId, Meter, RouteConfig, inputs, sinks};

#[derive(Clone)]
enum Endpoint {
    Cable(Arc<sinks::Control>),
    Playback { selector: String, id: String, origin: String },
}

#[derive(Clone)]
pub(super) struct Destination {
    endpoint: Endpoint,
    pub meter: Arc<Meter>,
}

impl Destination {
    pub fn cable(&self, id: CableId) -> Option<&sinks::Control> {
        match &self.endpoint {
            Endpoint::Cable(c) if c.id == id => Some(c),
            _ => None,
        }
    }

    fn playback(&self, selector: &str) -> bool {
        matches!(&self.endpoint, Endpoint::Playback { selector: known, .. } if known.eq_ignore_ascii_case(selector))
    }

    fn identity(&self) -> String {
        match &self.endpoint {
            Endpoint::Cable(control) => control.id.to_string(),
            Endpoint::Playback { origin, .. } => origin.clone(),
        }
    }
}

pub(super) struct Plan {
    pub destinations: Vec<Destination>,
    buses: Vec<usize>,
    retained: Vec<(usize, usize)>,
}

impl Plan {
    pub fn prepare(config: &RouteConfig, current: &[Destination], origins: &[Option<String>], paused: &[CableId]) -> Result<Self, String> {
        if config.cables.len() != config.output_buses.len() || config.cables.is_empty() && config.playback.is_empty() {
            return Err("Select at least one destination. Each direct cable needs one output bus.".into());
        }
        let mut destinations = Vec::with_capacity(config.cables.len() + config.playback.len());
        let mut retained = Vec::new();
        for &id in &config.cables {
            if let Some((old, c)) = current.iter().enumerate().find(|(_, c)| c.cable(id).is_some()) {
                retained.push((old, destinations.len()));
                destinations.push(c.clone());
            } else {
                let control = Arc::new(sinks::Control::new(id, !paused.contains(&id)));
                destinations.push(Destination { endpoint: Endpoint::Cable(control), meter: Arc::default() });
            }
        }
        // Use the identity of a running device even if it has disappeared since startup. Resolve only new selectors.
        let current_playback = |selector: &str| current.iter().position(|d| d.playback(selector));
        let new_selectors: Vec<_> = config.playback.iter().filter(|(s, _)| current_playback(s).is_none()).map(|(s, _)| s.clone()).collect();
        let (ids, identities) = if new_selectors.is_empty() { (vec![], vec![]) } else { inputs::resolve(&new_selectors, true)? };
        let mut resolved = ids.into_iter().zip(identities);
        for (selector, _) in &config.playback {
            if let Some(old) = current_playback(selector) {
                retained.push((old, destinations.len()));
                destinations.push(current[old].clone());
            } else {
                let (id, origin) = resolved.next().expect("each new playback selector was resolved");
                // A different spelling or selector for an already-open Windows endpoint must reuse that worker.
                let found = current.iter().enumerate().find_map(|(index, d)| match &d.endpoint {
                    Endpoint::Playback { id: previous, origin, .. } if previous.eq_ignore_ascii_case(&id) => Some((index, d, origin)),
                    _ => None,
                });
                let (meter, origin) = if let Some((old, d, known_origin)) = found {
                    retained.push((old, destinations.len()));
                    (d.meter.clone(), known_origin.clone())
                } else {
                    (Arc::default(), origin.expect("every playback endpoint has a loop identity"))
                };
                destinations.push(Destination { endpoint: Endpoint::Playback { selector: selector.clone(), id, origin }, meter });
            }
        }
        let targets: Vec<_> = destinations.iter().map(Destination::identity).collect();
        if targets.iter().enumerate().any(|(i, id)| targets[..i].contains(id)) {
            return Err("An endpoint is already selected as a destination. Combine its sends into one output.".into());
        }
        let buses: Vec<_> = config.output_buses.iter().copied().chain(config.playback.iter().map(|(_, bus)| *bus)).collect();
        inputs::check_loops(origins, &targets, &buses, &config.buses)?;
        Ok(Self { destinations, buses, retained })
    }

    pub fn open(&self, format: &StreamFormat) -> Result<Vec<Output>, String> {
        self.destinations
            .iter()
            .zip(&self.buses)
            .enumerate()
            .map(|(i, (d, &bus))| {
                let writer = if self.retained.iter().any(|&(_, new)| new == i) {
                    None
                } else {
                    Some(match &d.endpoint {
                        Endpoint::Cable(c) => Writer::Cable(sinks::Sink::open(c, &d.meter)?),
                        Endpoint::Playback { id, .. } => {
                            Writer::Playback(External::open(id.clone(), d.meter.clone(), format.max_frames, format.period)?)
                        }
                    })
                };
                Ok(Output { bus, meter: d.meter.clone(), writer })
            })
            .collect()
    }

    pub fn commit(&self, outputs: &mut Vec<Output>, prepared: &mut Vec<Output>) {
        // Startup and edits are serialized. Targets are unique and Plan::open reserved a vacant slot for each retained writer.
        for &(old, new) in &self.retained {
            std::mem::swap(&mut outputs[old].writer, &mut prepared[new].writer);
        }
        std::mem::swap(outputs, prepared);
    }
}

enum Writer {
    Cable(sinks::Sink),
    Playback(External),
}

pub(super) struct Output {
    pub bus: usize,
    meter: Arc<Meter>,
    writer: Option<Writer>,
}

impl Output {
    pub fn write(&mut self, router: &Router) {
        let packet = router.output(self.bus).expect("output buses were validated");
        match self.writer.as_mut().expect("all active output slots have a writer") {
            Writer::Cable(c) => c.write(packet, &self.meter),
            Writer::Playback(p) => p.write(packet, &self.meter),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;

    use llcable_routing::{Bus, Send, Tap};

    use super::*;

    fn route(cables: &[CableId]) -> RouteConfig {
        RouteConfig {
            inputs: vec!["mic".into()],
            cables: cables.into(),
            buses: vec![Bus { sends: vec![Send::unity(Tap::Input(0))], ..Bus::default() }],
            output_buses: vec![0; cables.len()],
            playback: Vec::new(),
        }
    }

    #[test]
    fn inserting_and_removing_direct_outputs_keeps_the_surviving_worker_and_meter() {
        let (a, b) = (CableId(1), CableId(2));
        let format = StreamFormat { max_frames: 128, period: 128, origins: vec![None] };
        // Paused attachment workers exercise the actual swap and retirement paths without accessing a driver.
        let first = Plan::prepare(&route(&[a]), &[], &format.origins, &[a]).unwrap();
        let mut live = first.open(&format).unwrap();
        let meter = first.destinations[0].meter.clone();
        meter.frames.store(8192, Relaxed);
        let both = Plan::prepare(&route(&[b, a]), &first.destinations, &format.origins, &[b]).unwrap();
        let mut prepared = both.open(&format).unwrap();
        assert!(prepared[0].writer.is_some() && prepared[1].writer.is_none());
        both.commit(&mut live, &mut prepared);
        assert!(prepared[0].writer.is_none());
        assert!(live.iter().all(|o| o.writer.is_some()));
        assert!(Arc::ptr_eq(&meter, &live[1].meter));
        assert_eq!(live[1].meter.frames.load(Relaxed), 8192);

        let last = Plan::prepare(&route(&[a]), &both.destinations, &format.origins, &[]).unwrap();
        let mut prepared = last.open(&format).unwrap();
        last.commit(&mut live, &mut prepared);
        assert!(prepared[0].writer.is_some() && prepared[1].writer.is_none());
        assert!(live[0].writer.is_some() && Arc::ptr_eq(&meter, &live[0].meter));
        assert!(!last.destinations[0].cable(a).unwrap().enabled());
    }

    #[test]
    fn live_validation_rejects_a_new_feedback_loop_and_duplicate_destination_before_opening() {
        let (a, b) = (CableId(1), CableId(2));
        let origins = [Some(b.to_string())];
        let current = Plan::prepare(&route(&[a]), &[], &origins, &[]).unwrap().destinations;
        assert!(matches!(Plan::prepare(&route(&[a, b]), &current, &origins, &[]), Err(e) if e.contains("Feedback loop")));
        assert!(matches!(Plan::prepare(&route(&[a, a]), &current, &origins, &[]), Err(e) if e.contains("already selected")));
        assert_eq!(current.len(), 1);
        assert!(current[0].cable(a).unwrap().enabled());
    }

    #[test]
    fn moving_external_playback_keeps_its_independent_clock_and_meter_slots() {
        let speaker = |s: &str| Destination {
            endpoint: Endpoint::Playback { selector: s.into(), id: s.into(), origin: s.into() },
            meter: Arc::default(),
        };
        let current = vec![speaker("headphones"), speaker("monitor")];
        let mut config = route(&[CableId(3)]);
        config.playback = vec![("MONITOR".into(), 0), ("headphones".into(), 0)];
        let edit = Plan::prepare(&config, &current, &[None], &[]).unwrap();
        assert_eq!(edit.retained, [(1, 1), (0, 2)]);
        for &(old, new) in &edit.retained {
            assert!(Arc::ptr_eq(&current[old].meter, &edit.destinations[new].meter));
        }
    }
}
