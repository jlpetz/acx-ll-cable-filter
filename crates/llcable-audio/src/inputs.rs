use llcable_routing::{Bus, Effect, Tap};
use llcable_sdk::driver::{CableInstance, Resolved, Snapshot};
use llcable_sdk::{CableId, EndpointKind};

pub(super) fn resolve(selectors: &[String], playback: bool) -> Result<(Vec<String>, Vec<Option<String>>), String> {
    let inventory = Snapshot::read().map_err(|e| e.to_string())?;
    let mut ids: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    for selector in selectors {
        let loopback = selector.strip_prefix("loopback:");
        if playback && loopback.is_some() {
            return Err("A playback destination cannot be a loopback recording selection.".into());
        }
        let selector = loopback.unwrap_or(selector);
        let (mut id, cable) = if let Some(port) = selector.strip_prefix("acx-ll-cable:") {
            let (id, role) = port.split_once('/').ok_or("Invalid cable endpoint selection.")?;
            let id = id.parse().map_err(|_| "Invalid cable identity.")?;
            let role = EndpointKind::from_arg(role)
                .filter(|role| role.is_render() == (playback || loopback.is_some()))
                .ok_or("Select a recording endpoint or playback monitor.")?;
            match inventory.resolve(id, role) {
                Resolved::Endpoint(port) => (port.id, Some(id)),
                Resolved::None(e) => return Err(e.to_string()),
            }
        } else {
            let cable = inventory
                .endpoints
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(selector))
                .and_then(|p| CableInstance::parse(&p.instance_id))
                .map(|c| c.id);
            (selector.to_owned(), cable)
        };
        let identity = cable.map(|c: CableId| c.to_string()).or_else(|| (playback || loopback.is_some()).then(|| id.to_ascii_lowercase()));
        if loopback.is_some() {
            id.insert_str(0, "loopback:");
        }
        if ids.iter().any(|s| s.eq_ignore_ascii_case(&id)) {
            return Err("An endpoint is selected twice. Use one source or destination with multiple sends.".into());
        }
        ids.push(id);
        sources.push(identity);
    }
    Ok((ids, sources))
}

/// Include indirect sends and sidechain keys. Reading cable A and feeding another one is fine until a path closes the loop.
pub(super) fn check_loops<T: Ord + Clone + std::fmt::Display>(
    inputs: &[Option<T>], targets: &[T], outputs: &[usize], buses: &[Bus],
) -> Result<(), String> {
    let mut origins: Vec<Vec<T>> = Vec::new();
    for bus in buses {
        let mut upstream = Vec::new();
        let keys = bus.effects.iter().filter_map(Effect::key);
        for tap in bus.sends.iter().map(|s| s.from).chain(keys) {
            match tap {
                Tap::Input(i) => upstream.extend(inputs.get(i).ok_or("A send references a missing source.")?.iter().cloned()),
                Tap::Bus(i) => upstream.extend(origins.get(i).ok_or("A send references a later or missing bus.")?.iter().cloned()),
            }
        }
        upstream.sort_unstable();
        upstream.dedup();
        origins.push(upstream);
    }
    let mut reachable = vec![vec![false; targets.len()]; targets.len()];
    for (i, &bus) in outputs.iter().enumerate() {
        let upstream = origins.get(bus).ok_or("A cable references a missing bus.")?;
        for (j, cable) in targets.iter().enumerate() {
            reachable[i][j] = upstream.contains(cable);
        }
    }
    for k in 0..targets.len() {
        for i in 0..targets.len() {
            for j in 0..targets.len() {
                reachable[i][j] |= reachable[i][k] && reachable[k][j];
            }
        }
    }
    if let Some(i) = (0..targets.len()).find(|&i| reachable[i][i]) {
        return Err(format!(
            "Feedback loop: destination {} feeds its own output through this patch. Change a source or destination.",
            targets[i]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use llcable_routing::Send;

    use super::*;

    #[test]
    fn cross_cable_routes_may_chain_but_must_not_feed_back() {
        let a = CableId(1);
        let b = CableId(2);
        let buses: Vec<_> = [0, 1].map(|i| Bus { sends: vec![Send::unity(Tap::Input(i))], ..Bus::default() }).into();
        assert!(check_loops(&[Some(a), None], &[b, a], &[0, 1], &buses).is_ok());
        assert!(check_loops(&[Some(a), Some(b)], &[b, a], &[0, 1], &buses).is_err());
        assert!(check_loops(&[Some(a), None], &[a], &[0], &buses).is_err());
    }
}
