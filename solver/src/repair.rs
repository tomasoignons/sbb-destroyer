//! Use the bike only where it is really needed.
//!
//! After the search, every bike leg A → B of a solution is checked: if trains / buses /
//! walking can get from A (leaving when the bike would) to B in time for whatever the route
//! does next at B (the next vehicle's departure, or the moment the bike would have arrived),
//! the bike leg is replaced by that. The rest of the route is untouched, so the trip never
//! gets longer (it can get shorter if the first leg was a bike leg: the clock then starts at
//! the first vehicle instead).

use crate::data::*;
use crate::solver::{Kind, Node, Rules, Solution, CONT_BIT, EXCURSION_FLAG};

const INF: u32 = u32::MAX;

#[derive(Clone, Copy)]
enum Pred {
    None,
    Start,
    Walk { from: u32 },
    Ride { board: u32, alight: u32 },
}

/// Number of bike legs of a solution.
pub fn bike_legs(tt: &Timetable, sol: &Solution) -> usize {
    sol.path.iter().filter(|n| n.kind == Kind::Walk && tt.link_kind(n.a, n.b) == 1).count()
}

/// Replace avoidable bike legs in place. Returns how many were replaced.
pub fn reduce_bike(tt: &Timetable, sol: &mut Solution, rules: &Rules) -> usize {
    let mut replaced = 0;
    let mut i = 1;
    while i < sol.path.len() {
        let n = sol.path[i];
        if n.kind != Kind::Walk || tt.link_kind(n.a, n.b) != 1 {
            i += 1;
            continue;
        }
        let t0 = sol.path[i - 1].time;
        // what has to happen at B, and by when
        let need = match sol.path.get(i + 1) {
            Some(x) if x.kind == Kind::Ride && x.a & CONT_BIT == 0 => Need::Ready(tt.dep[x.a as usize]),
            _ => Need::Arrive(n.time),
        };
        // cantons collected before A (for the detour rules)
        let mask = path_mask(tt, &sol.path[..i], rules);
        match transit_path(tt, rules, mask, n.a, t0, n.b, need) {
            Some(mut nodes) => {
                // staying on the vehicle we just got off: one ride, not alight + re-board
                let prev = sol.path[i - 1];
                if let Some(f) = nodes.first().copied() {
                    let (pb, fb) = (prev.b as usize, (f.a & !CONT_BIT) as usize);
                    if prev.kind == Kind::Ride && f.kind == Kind::Ride && f.a & CONT_BIT == 0
                        && tt.trip[pb] == tt.trip[fb] && tt.to[pb] == tt.from[fb]
                    {
                        sol.path[i - 1] = Node { b: f.b, time: f.time, ..prev };
                        nodes.remove(0);
                    }
                }
                let k = nodes.len();
                sol.path.splice(i..=i, nodes);
                replaced += 1;
                i += k;
            }
            None => i += 1,
        }
    }
    replaced
}

#[derive(Clone, Copy)]
enum Need {
    /// ready to board (arrival + change time) by then
    Ready(u32),
    /// physically there by then
    Arrive(u32),
}

/// Cantons stepped on along a path prefix (stops only; enough for the detour rules).
fn path_mask(tt: &Timetable, path: &[Node], rules: &Rules) -> u32 {
    let mut m = rules.initial;
    for n in path {
        let stops: [u32; 2] = match n.kind {
            Kind::Run if n.b & EXCURSION_FLAG != 0 => [n.a, rules.excursions[(n.b & !EXCURSION_FLAG) as usize].to],
            Kind::Start | Kind::Run => [n.a, n.a],
            Kind::Walk => [n.b, n.b],
            Kind::Ride => {
                let bc = (n.a & !CONT_BIT) as usize;
                [tt.from[bc], tt.to[n.b as usize]]
            }
        };
        for s in stops {
            m = rules.adv(tt, m, s);
        }
    }
    m
}

/// Earliest-arrival scan (no bike) from `a` at `t0` to `b`; returns the steps if `need` is met.
fn transit_path(tt: &Timetable, rules: &Rules, mask: u32, a: u32, t0: u32, b: u32, need: Need) -> Option<Vec<Node>> {
    let limit = match need {
        Need::Ready(t) | Need::Arrive(t) => t,
    };
    let n = tt.n_stops();
    let mut ready = vec![INF; n];
    let mut arrive = vec![INF; n];
    let mut pred = vec![Pred::None; n];
    let mut trip_board = vec![NONE; tt.trips.len()];
    // detour rules: don't stop anywhere (except B) where an obligation would open
    let allowed = |s: u32| s == b || rules.detour_need(mask, s) == 0;

    let walk_from = |s: u32, t: u32, ready: &mut Vec<u32>, arrive: &mut Vec<u32>, pred: &mut Vec<Pred>| {
        for (w, d, kind) in tt.footpaths(s) {
            let ta = t + d;
            if kind == 0 && ta < ready[w as usize] && allowed(w) {
                ready[w as usize] = ta;
                arrive[w as usize] = ta;
                pred[w as usize] = Pred::Walk { from: s };
            }
        }
    };
    ready[a as usize] = t0;
    arrive[a as usize] = t0;
    pred[a as usize] = Pred::Start;
    walk_from(a, t0, &mut ready, &mut arrive, &mut pred);

    let first = tt.dep.partition_point(|&d| d < t0);
    for c in first..tt.n_conns() {
        if tt.dep[c] > limit {
            break;
        }
        let t = tt.trip[c] as usize;
        let u = tt.from[c];
        if trip_board[t] == NONE && tt.flags[c] & F_BOARD != 0 && ready[u as usize] <= tt.dep[c] {
            trip_board[t] = c as u32;
        }
        if trip_board[t] == NONE || tt.flags[c] & F_ALIGHT == 0 {
            continue;
        }
        let v = tt.to[c];
        let ar = tt.arr[c];
        let r = ar + tt.stop_change[v as usize];
        if r < ready[v as usize] && allowed(v) {
            ready[v as usize] = r;
            arrive[v as usize] = ar;
            pred[v as usize] = Pred::Ride { board: trip_board[t], alight: c as u32 };
            walk_from(v, ar, &mut ready, &mut arrive, &mut pred);
        }
    }

    let ok = match need {
        Need::Ready(t) => ready[b as usize] <= t,
        Need::Arrive(t) => arrive[b as usize] <= t,
    };
    if !ok {
        return None;
    }
    let mut out = vec![];
    let mut s = b;
    for _ in 0..1000 {
        match pred[s as usize] {
            Pred::Start => break,
            Pred::None => return None,
            Pred::Walk { from } => {
                out.push(Node { parent: NONE, kind: Kind::Walk, a: from, b: s, time: arrive[s as usize] });
                s = from;
            }
            Pred::Ride { board, alight } => {
                out.push(Node { parent: NONE, kind: Kind::Ride, a: board, b: alight, time: tt.arr[alight as usize] });
                s = tt.from[board as usize];
            }
        }
    }
    if s != a {
        return None;
    }
    out.reverse();
    Some(out)
}
