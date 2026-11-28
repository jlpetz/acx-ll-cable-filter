//! Geometry for the three-source, three-destination patchbay. A wire only shares a segment with another at a real split or mix.
use super::{BRANCH_WIDTH, DESTS, GAP, NODE_HEIGHT, SOURCE_Y, SOURCES, Scene, Scope};

pub(super) const STAGE_STEP: f64 = NODE_HEIGHT + 16.0;

pub(super) fn boundary_y(stage_count: usize) -> f64 {
    SOURCE_Y + NODE_HEIGHT + 41.0 + 64.0 * stage_count as f64
}

#[derive(Clone, Default)]
pub(super) struct SourceGroup {
    pub(super) x: f64,
    pub(super) sends: Vec<(usize, usize, f64)>, // destination, shared-stage boundary, lane centre
    pub(super) tail: Option<f64>,               // room for shared stages downstream of the last active tap
}

pub(super) struct Layout {
    pub(super) width: f64,
    pub(super) sources: Vec<SourceGroup>,
    pub(super) outputs: [f64; 3],
    pub(super) branch_y: f64,
    pub(super) exit_y: f64,
    pub(super) mix_y: f64,
    pub(super) post_mix_y: f64,
    pub(super) output_y: f64,
}

fn span(group: &SourceGroup) -> usize {
    (group.sends.len() + usize::from(group.tail.is_some())).max(1)
}

/// With the inputs in their familiar positions, there are just six possible output orders.
/// Taps stay grouped by shared-stage boundary so a dry lead never intersects the downstream source chain.
fn arrange(groups: &mut [SourceGroup], order: [usize; 3], pitch: f64) -> [f64; 3] {
    let mut ranks = [0; 3];
    for (rank, d) in order.into_iter().enumerate() {
        ranks[d] = rank;
    }
    let mut starts = [0_usize; 3];
    let mut sums = [0.0; 3];
    let mut lane = 0;
    for group in groups {
        group.sends.sort_by_key(|&(d, tap, _)| (tap, ranks[d]));
        group.x = (lane as f64 + span(group) as f64 / 2.0) * pitch;
        for (i, (d, _, x)) in group.sends.iter_mut().enumerate() {
            *x = (lane as f64 + i as f64 + 0.5) * pitch;
            sums[*d] += *x;
            starts[*d] += 1;
        }
        lane += span(group);
        if let Some(x) = &mut group.tail {
            *x = (lane as f64 - 0.5) * pitch;
        }
    }
    let width = lane as f64 * pitch;
    let spacing = BRANCH_WIDTH + GAP;
    let mut targets = [0.0_f64; 3];
    let mut left = spacing / 2.0;
    for (rank, d) in order.into_iter().enumerate() {
        let target = if starts[d] > 0 { sums[d] / starts[d] as f64 } else { (rank as f64 + 0.5) * width / 3.0 };
        targets[d] = target.max(left);
        left = targets[d] + spacing;
    }
    let mut right = width - spacing / 2.0;
    for d in order.into_iter().rev() {
        targets[d] = targets[d].min(right);
        right = targets[d] - spacing;
    }
    targets
}

fn score(groups: &[SourceGroup], outputs: &[f64; 3]) -> (usize, i64) {
    let mut seen = Vec::new();
    let mut crossings = 0;
    let mut distance = 0.0;
    for &(d, _, x) in groups.iter().flat_map(|g| &g.sends) {
        crossings += seen.iter().filter(|&&previous| outputs[previous] > outputs[d]).count();
        distance += (x - outputs[d]).abs();
        seen.push(d);
    }
    (crossings, (distance * 100.0).round() as i64)
}

impl Layout {
    pub(super) fn new(scene: &Scene, visible_width: f64) -> Self {
        let mut sources: Vec<_> = (0..SOURCES.len())
            .map(|s| {
                let sends: Vec<_> = scene.sends[s]
                    .iter()
                    .enumerate()
                    .filter(|(_, edge)| edge.connected)
                    .map(|(d, _)| (d, scene.source_stages_for_send(s, d).len(), 0.0))
                    .collect();
                let last_tap = sends.iter().map(|e| e.1).max().unwrap_or(0);
                let tail = (scene.at(Scope::Source(s)).len() > last_tap).then_some(0.0);
                SourceGroup { x: 0.0, sends, tail }
            })
            .collect();
        let lanes: usize = sources.iter().map(span).sum();
        let width = visible_width.max(lanes as f64 * (BRANCH_WIDTH + GAP));
        let pitch = width / lanes as f64;
        let mut best = (usize::MAX, i64::MAX);
        let mut order = [0, 1, 2];
        for candidate in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let outputs = arrange(&mut sources, candidate, pitch);
            let current = score(&sources, &outputs);
            if current < best {
                best = current;
                order = candidate;
            }
        }
        let outputs = arrange(&mut sources, order, pitch);
        let longest_shared = (0..SOURCES.len()).map(|s| scene.at(Scope::Source(s)).len()).max().unwrap_or(0);
        let longest_branch = sources
            .iter()
            .enumerate()
            .flat_map(|(s, group)| group.sends.iter().map(move |&(d, _, _)| scene.at(Scope::Send(s, d)).len()))
            .max()
            .unwrap_or(0);
        let longest_post = (0..DESTS.len()).map(|d| scene.at(Scope::Output(d)).len()).max().unwrap_or(0);
        let branch_y = boundary_y(longest_shared) + 25.0;
        let exit_y = branch_y + longest_branch as f64 * STAGE_STEP + 38.0;
        let mix_y = exit_y + if best.0 > 0 { 170.0 } else { 54.0 };
        let post_mix_y = mix_y + 28.0;
        let output_y = post_mix_y + longest_post as f64 * STAGE_STEP;
        Self { width, sources, outputs, branch_y, exit_y, mix_y, post_mix_y, output_y }
    }
}

/// The two endpoints belong to parallel horizontal rows. A strictly interior sign change identifies an intersection.
/// Endpoints that meet at the same mix are intentional junctions.
pub(super) fn crossed_at(a: (f64, f64), b: (f64, f64)) -> Option<f64> {
    let top = a.0 - b.0;
    let bottom = a.1 - b.1;
    if top * bottom < 0.0 { Some(top / (top - bottom)) } else { None }
}

/// Return the visible fractions of this wire. Short breaks let earlier wires cross underneath later ones without a false merge.
pub(super) fn wire_spans(from: f64, to: f64, height: f64, above: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut breaks: Vec<_> = above
        .iter()
        .filter_map(|&line| {
            let t = crossed_at((from, to), line)?;
            let length = (to - from).hypot(height);
            let other_length = (line.1 - line.0).hypot(height);
            let sin_angle = (height * ((to - from) - (line.1 - line.0))).abs() / (length * other_length);
            let half = (4.0 / sin_angle).min(20.0) / length;
            Some(((t - half).max(0.0), (t + half).min(1.0)))
        })
        .collect();
    breaks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut spans = Vec::new();
    let mut begin: f64 = 0.0;
    for (before, after) in breaks {
        if begin < before {
            spans.push((begin, before));
        }
        begin = begin.max(after);
    }
    if begin < 1.0 {
        spans.push((begin, 1.0));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Tap};

    #[test]
    fn all_nine_sends_have_private_lanes_and_only_the_required_two_row_crossings() {
        let mut scene = Scene::empty(Scope::Source(0));
        for row in &mut scene.sends {
            for send in row {
                send.connected = true;
            }
        }
        let layout = Layout::new(&scene, 780.0);
        let lines: Vec<_> = layout.sources.iter().flat_map(|g| g.sends.iter().map(|&(d, _, x)| (x, layout.outputs[d]))).collect();
        assert_eq!(lines.len(), 9);
        assert!(lines.windows(2).all(|ab| ab[1].0 - ab[0].0 >= BRANCH_WIDTH + GAP));
        let mut crossings = 0;
        for (i, &line) in lines.iter().enumerate() {
            let others = &lines[i + 1..];
            let cuts = wire_spans(line.0, line.1, layout.mix_y - layout.exit_y, others);
            for &other in others {
                if let Some(t) = crossed_at(line, other) {
                    crossings += 1;
                    assert!(cuts.iter().all(|&(lo, hi)| t < lo || t > hi));
                }
            }
        }
        // Each of 3 ordered source pairs crosses once for each of 3 ordered destination pairs.
        assert_eq!(crossings, 9);
        assert_eq!(Layout::new(&scene, 780.0).outputs, layout.outputs);
    }

    #[test]
    fn a_shared_stage_after_the_last_tap_has_a_separate_dead_end() {
        let mut scene = Scene::empty(Scope::Source(0));
        scene.add(Kind::Noise);
        scene.toggle(0, 0);
        scene.sends[0][0].tap = Tap::Raw;
        let layout = Layout::new(&scene, 680.0);
        let shared = &layout.sources[0];
        assert!(shared.tail.unwrap() - shared.sends[0].2 >= BRANCH_WIDTH + GAP);
        assert_eq!(shared.sends[0].1, 0);
    }
}
