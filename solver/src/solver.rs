//! Canton-collecting Connection Scan Algorithm.
//!
//! Classic CSA scans all connections once in departure-time order. Here every label
//! additionally carries a 32-bit mask:
//!
//! * bits 0..25  = cantons "stepped on" so far (bit i = KANTONSNUMMER i+1);
//! * bits 26..31 = progress through the ordered `via` stations, in unary: via station k is
//!   reached only once 0..k-1 are, so "more progress" = "superset" and the usual
//!   dominance test keeps working.
//!
//! What adds to the mask:
//! * boarding or alighting at a stop adds its canton;
//! * riding through a stop where the vehicle *halts* for passengers (pickup and drop-off
//!   allowed, dwell >= `min_dwell`) adds its canton (hop off / hop back on);
//! * walking or cycling to a stop adds the canton of the destination stop;
//! * stops a vehicle merely passes through are not in the GTFS, so they never count;
//! * bonus ("run for it") stations: alighting there and leaving at least `secs` later
//!   adds extra cantons (e.g. Ziegelbrücke ≥ 1 min → Glarus). Modelled as a Run step.
//!
//! Labels at a stop are kept Pareto-optimal for (ready time ↓, mask ⊇): a label is
//! discarded if another label is ready no later *and* already has all its bits. The
//! exact Pareto set can be huge, so each stop/trip keeps at most `beam_stop`/`beam_trip`
//! labels (see `Evict`). Labels that provably cannot beat `upper_bound` are pruned.
//!
//! Because connections are scanned by departure time, the first time we can stop is when
//! the next departure is later than the best arrival with all required bits.

use crate::data::*;
use crate::heuristic::{Bounds, INF};
use std::cmp::Reverse;
use std::sync::Arc;

pub const CONT_BIT: u32 = 1 << 31; // board conn index flag: "stayed seated from previous trip"
pub const CANTONS: u32 = (1 << 26) - 1;
pub const VIA_SHIFT: u32 = 26;
pub const MAX_VIA: usize = 5;
/// mask bit: the mandatory end station was reached after everything else
pub const END_BIT: u32 = 1 << 31;
/// in a `Kind::Run` node's `b`: one-way finishing run (see `Rules::finish`), not there-and-back
pub const FINISH_FLAG: u32 = 1 << 29;
/// in a `Kind::Run` node's `b`: bike excursion, the low bits are the index into `Rules::excursions`
pub const EXCURSION_FLAG: u32 = 1 << 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Start,
    /// a = boarding connection (or `CONT_BIT | conn` of the previous trip for in-seat
    /// continuations), b = alighting connection
    Ride,
    /// a = from stop, b = to stop
    Walk,
    /// run out of a bonus station and back: a = stop, b = cantons granted
    Run,
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: u32,
    pub kind: Kind,
    pub a: u32,
    pub b: u32,
    /// actual arrival time at the end of this step
    pub time: u32,
}

/// `Node` packed into 16 bytes (instead of 20) for the search's node arena, which holds
/// up to hundreds of millions of entries: `kind` lives in the top 2 bits of `b`
/// (b is a stop, a connection or a canton mask, all < 2^30).
#[derive(Clone, Copy)]
struct PNode {
    parent: u32,
    a: u32,
    kb: u32,
    time: u32,
}

const KIND_SHIFT: u32 = 30;
const B_MASK: u32 = (1 << KIND_SHIFT) - 1;

impl From<Node> for PNode {
    #[inline]
    fn from(n: Node) -> PNode {
        debug_assert!(n.b <= B_MASK);
        let k = match n.kind {
            Kind::Start => 0,
            Kind::Ride => 1,
            Kind::Walk => 2,
            Kind::Run => 3,
        };
        PNode { parent: n.parent, a: n.a, kb: (k << KIND_SHIFT) | n.b, time: n.time }
    }
}

impl From<PNode> for Node {
    #[inline]
    fn from(p: PNode) -> Node {
        let kind = match p.kb >> KIND_SHIFT {
            0 => Kind::Start,
            1 => Kind::Ride,
            2 => Kind::Walk,
            _ => Kind::Run,
        };
        Node { parent: p.parent, kind, a: p.a, b: p.kb & B_MASK, time: p.time }
    }
}

#[derive(Clone, Copy)]
struct StopLabel {
    mask: u32,
    /// earliest time this label can board a vehicle at the stop
    ready: u32,
    node: u32,
    /// beam priority, smaller = better (see `Evict`)
    key: u64,
}

/// Which labels survive when a stop / trip exceeds its beam width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Evict {
    /// keep labels with the most cantons (ties: earliest)
    Popcount,
    /// keep labels with the most cantons (ties: smallest lower bound on the final arrival)
    ///
    /// Note: ordering primarily by a time-based estimate (e.g. ready + lower bound) does
    /// NOT work: a label's key is frozen when it is inserted, so labels that arrived early
    /// with few cantons look artificially good and fill the beam, and the search stalls.
    Bound,
    /// keep labels with the smallest  lambda * missing_cantons + lower_bound(stop, missing)
    /// (ties: earliest). Both terms are time-independent, so keys don't go stale.
    Spread,
}

#[derive(Clone, Copy)]
struct TripLabel {
    mask: u32,
    /// node of the stop label we boarded from
    parent: u32,
    board: u32,
    /// cantons that must be reached before getting off (detour rules), 0 = none
    need: u32,
    /// ride-group cantons stepped on during this ride (see `Rules::ride_group`)
    ride: u32,
}

impl TripLabel {
    /// still-unsatisfied part of `need`
    #[inline]
    fn open_need(&self) -> u32 {
        self.need & !self.mask
    }

    /// ride-group progress (the group's cantons already in the mask count as seen)
    #[inline]
    fn ride_seen(&self) -> u32 {
        self.ride | self.mask
    }

    /// `self` is at least as good as `o`: more cantons, more ride-group progress and no
    /// more obligations
    #[inline]
    fn dominates(&self, o: &TripLabel) -> bool {
        (self.mask & o.mask) == o.mask
            && (self.ride_seen() & o.ride_seen()) == o.ride_seen()
            && (self.open_need() & !o.open_need()) == 0
    }
}

/// A "run for it" station: alight there, leave at least `secs` later → `cantons` count too.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Bonus {
    pub name: String,
    pub secs: u32,
    pub cantons: u32,
}

/// Bike excursion: get off at `from`, cycle through `cantons`, arrive at stop `to` `secs`
/// later (e.g. Hundwil, Sonderau → Appenzell Innerrhoden → Herisau, 35 min).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Excursion {
    pub from_name: String,
    pub to_name: String,
    pub to: u32,
    pub secs: u32,
    pub cantons: u32,
}

/// Route-shape rules shared by all searches of one run (built once in main).
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// per stop: bit k set if the stop belongs to via station k
    pub via_of_stop: Vec<u8>,
    pub via_names: Vec<String>,
    /// per stop: index into `bonus`, or NONE
    pub bonus_of_stop: Vec<u32>,
    pub bonus: Vec<Bonus>,
    /// cantons already validated before this (partial) trip starts
    pub initial: u32,
    /// via stations are visited first and directly (display only; see main.rs)
    pub via_first: bool,
    /// cantons required (needed to know when the end station may be "reached")
    pub required: u32,
    /// mandatory end station: per stop 1 if it belongs to it
    pub end_of_stop: Vec<u8>,
    pub end_name: Option<String>,
    /// detour rules, per stop: "if you are here without these cantons, go get them first"
    /// (e.g. Lausanne / Renens VD -> Genève). 0 = no rule.
    pub detour_of_stop: Vec<u32>,
    /// cantons that only count when all of them are stepped on during one single ride
    /// (e.g. BS, BL, JU, SO, BE: the Basel SBB ↔ Biel/Bienne train). Stepping on one of
    /// them elsewhere adds nothing, so the beam doesn't chase them one by one. 0 = off.
    pub ride_group: u32,
    /// finishing runs, per stop: index into `finish`, or NONE. Getting off at (or walking to)
    /// such a station when its canton(s) are the only ones still missing ends the trip
    /// `secs` later, on foot across the border (e.g. Schaffhausen → Thurgau in 10 min).
    pub finish_of_stop: Vec<u32>,
    pub finish: Vec<Bonus>,
    /// bike excursions, and per departure stop the indices of those starting there
    pub excursions: Vec<Excursion>,
    pub excursions_of_stop: std::collections::HashMap<u32, Vec<u32>>,
}

impl Rules {
    pub fn new(n_stops: usize) -> Rules {
        Rules {
            via_of_stop: vec![0; n_stops],
            bonus_of_stop: vec![NONE; n_stops],
            finish_of_stop: vec![NONE; n_stops],
            detour_of_stop: vec![0; n_stops],
            end_of_stop: vec![0; n_stops],
            ..Default::default()
        }
    }

    pub fn n_via(&self) -> u32 {
        self.via_names.len() as u32
    }

    /// Mask bits for "all via stations reached".
    pub fn via_bits(&self) -> u32 {
        ((1u32 << self.n_via()) - 1) << VIA_SHIFT
    }

    /// Mask after stepping on `stop`: its canton, plus via progress if it is the next via station.
    #[inline]
    pub fn adv(&self, tt: &Timetable, mask: u32, stop: u32) -> u32 {
        let mut m = mask | (tt.bit(stop) & !self.ride_group);
        let w = self.via_of_stop[stop as usize];
        if w != 0 {
            let mut k = (m >> VIA_SHIFT).trailing_ones();
            while k < self.n_via() && (w >> k) & 1 == 1 {
                m |= 1 << (VIA_SHIFT + k);
                k += 1;
            }
        }
        // end station: only counts once everything else is done
        if self.end_name.is_some() && self.end_of_stop[stop as usize] != 0 {
            let need = self.required | self.via_bits();
            if m & need == need {
                m |= END_BIT;
            }
        }
        m
    }

    /// Ride-group cantons of `stop` (0 if none / feature off).
    #[inline]
    pub fn ride_bit(&self, tt: &Timetable, stop: u32) -> u32 {
        tt.bit(stop) & self.ride_group
    }

    /// `mask` plus the whole ride group if `ride` (with `mask`) now covers it, then the
    /// end-station check again (completing the group may complete everything).
    #[inline]
    pub fn ride_done(&self, tt: &Timetable, mask: u32, ride: u32, stop: u32) -> u32 {
        let g = self.ride_group;
        if g != 0 && mask & g != g && (ride | mask) & g == g {
            self.adv(tt, mask | g, stop)
        } else {
            mask
        }
    }

    /// Bits a complete solution must have: cantons, via stations, end station.
    pub fn goal(&self, required: u32) -> u32 {
        required | self.via_bits() | if self.end_name.is_some() { END_BIT } else { 0 }
    }

    #[inline]
    pub fn finish_at(&self, stop: u32) -> Option<&Bonus> {
        let i = *self.finish_of_stop.get(stop as usize)?;
        (i != NONE).then(|| &self.finish[i as usize])
    }

    #[inline]
    pub fn bonus_at(&self, stop: u32) -> Option<&Bonus> {
        let i = self.bonus_of_stop[stop as usize];
        (i != NONE).then(|| &self.bonus[i as usize])
    }

    /// Cantons a label with `mask` at `stop` is obliged to fetch next (detour rules).
    #[inline]
    pub fn detour_need(&self, mask: u32, stop: u32) -> u32 {
        self.detour_of_stop[stop as usize] & !mask
    }

    #[inline]
    fn touches(&self, tt: &Timetable, stop: u32) -> bool {
        tt.bit(stop) != 0 || self.via_of_stop[stop as usize] != 0 || self.end_of_stop[stop as usize] != 0
    }
}

#[derive(Clone, Debug)]
pub struct Params {
    pub start_stops: Vec<u32>,
    pub start_time: u32,
    pub beam_stop: usize,
    pub beam_trip: usize,
    /// minimum scheduled dwell [s] for a ride-through halt to count as "stepping on" a canton
    pub min_dwell: u32,
    /// give up after this many seconds
    pub max_duration: u32,
    /// cantons that must be visited (bit i = KANTONSNUMMER i+1)
    pub required: u32,
    /// allow walking between nearby stops
    pub walking: bool,
    /// allow bike links (if `walking` is on)
    pub bike: bool,
    pub evict: Evict,
    /// Spread eviction: seconds of estimated cost per missing canton
    pub lambda: u32,
    /// prune every label that provably cannot arrive before this time (u32::MAX = off)
    pub upper_bound: u32,
    /// via stations and bonus stations
    pub rules: Arc<Rules>,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub conns_scanned: u64,
    pub nodes: u64,
    pub millis: u128,
}

#[derive(Clone, Debug)]
pub struct Solution {
    /// path from the start node to the final node
    pub path: Vec<Node>,
    /// true if all required cantons (and via stations) were visited
    pub complete: bool,
    /// cantons visited (bits 0..25)
    pub mask: u32,
    /// number of via stations reached (in order)
    pub via_reached: u32,
    pub arrival: u32,
    pub stats: Stats,
}

impl Solution {
    /// The trip starts when the traveller boards the first vehicle or gets on the bike
    /// (walking or waiting before that doesn't count).
    pub fn first_departure(&self, tt: &Timetable) -> u32 {
        let mut prev_time = self.arrival;
        for n in &self.path {
            match n.kind {
                Kind::Ride if n.a & CONT_BIT == 0 => return tt.dep[n.a as usize],
                Kind::Walk if tt.link_kind(n.a, n.b) == 1 => return prev_time,
                _ => {}
            }
            prev_time = n.time;
        }
        self.arrival
    }

    /// From the first vehicle / bike until the last canton is reached (= getting off,
    /// or hopping off, in it; arriving on foot/bike; or the end of a bonus run).
    pub fn duration(&self, tt: &Timetable) -> u32 {
        self.arrival - self.first_departure(tt)
    }
}

/// Insert into a stop's label set. Returns the index of the inserted label, or None if
/// dominated or evicted by the beam.
#[inline]
fn insert_stop(set: &mut Vec<StopLabel>, mask: u32, ready: u32, key: u64, cap: usize) -> Option<usize> {
    for e in set.iter() {
        if e.ready <= ready && (e.mask & mask) == mask {
            return None;
        }
    }
    set.retain(|e| !(ready <= e.ready && (mask & e.mask) == e.mask));
    if set.len() >= cap {
        let (wi, w) = set.iter().enumerate().max_by_key(|(_, e)| e.key).unwrap();
        if w.key <= key {
            return None;
        }
        set.swap_remove(wi);
    }
    set.push(StopLabel { mask, ready, node: NONE, key });
    Some(set.len() - 1)
}

#[inline]
fn insert_trip(set: &mut Vec<TripLabel>, l: TripLabel, cap: usize, key: impl Fn(u32) -> u64) -> bool {
    for e in set.iter() {
        if e.dominates(&l) {
            return false;
        }
    }
    set.retain(|e| !l.dominates(e));
    if set.len() >= cap {
        let (wi, w) = set.iter().enumerate().max_by_key(|(_, e)| key(e.mask)).unwrap();
        if key(w.mask) <= key(l.mask) {
            return false;
        }
        set.swap_remove(wi);
    }
    set.push(l);
    true
}

/// After changing trip-label masks, some may now be subsets of others.
fn prune_trip(set: &mut Vec<TripLabel>) {
    if set.len() < 2 {
        return;
    }
    set.sort_unstable_by_key(|e| Reverse(e.mask.count_ones()));
    let mut keep: Vec<TripLabel> = Vec::with_capacity(set.len());
    for e in set.iter() {
        if !keep.iter().any(|k| k.dominates(e)) {
            keep.push(*e);
        }
    }
    *set = keep;
}

#[derive(Clone, Copy)]
struct Best {
    arrival: u32,
    node: u32,
    mask: u32,
}

/// Mutable state of one search.
struct Search<'a> {
    tt: &'a Timetable,
    bounds: &'a Bounds,
    p: &'a Params,
    rules: &'a Rules,
    req: u32,
    nodes: Vec<PNode>,
    stop_sets: Vec<Vec<StopLabel>>,
    best: Best,
    /// best partial result (most bits, then earliest), reported if no full solution exists
    partial: Best,
}

impl<'a> Search<'a> {
    /// Beam priority of a label (smaller = better), or None if it provably cannot beat
    /// the upper bound (or cannot finish at all).
    #[inline]
    fn key(&self, mask: u32, stop: u32, ready: u32) -> Option<u64> {
        let missing = self.req & !mask;
        let mc = missing & CANTONS;
        let h = self.bounds.h_end(stop, mc, missing & END_BIT != 0);
        let ub = self.p.upper_bound;
        if h >= INF || (ub != u32::MAX && ready.saturating_add(h) >= ub) {
            return None;
        }
        let lack = missing.count_ones() as u64;
        Some(match self.p.evict {
            Evict::Popcount => (lack << 32) | ready as u64,
            Evict::Bound => (lack << 32) | (ready + h) as u64,
            Evict::Spread => ((self.p.lambda as u64 * lack + h as u64) << 24) | (ready as u64 & 0xff_ffff),
        })
    }

    fn consider(&mut self, mask: u32, arrival: u32, node: u32) {
        let m = mask & self.req;
        if m == self.req {
            if arrival < self.best.arrival {
                self.best = Best { arrival, node, mask };
            }
        } else {
            let (pc, bc) = (m.count_ones(), self.partial.mask.count_ones());
            if pc > bc || (pc == bc && arrival < self.partial.arrival) {
                self.partial = Best { arrival, node, mask: m };
            }
        }
    }

    /// A label reaches `stop` at `time` with `mask` and can board again at `ready`.
    /// The step that got it there is `node` (its index is created only when needed).
    /// Returns (index of the pushed node, whether the label was kept at the stop).
    /// Off the vehicle at a finish station with only its canton(s) missing: run there and
    /// stop. Returns the arrival node's index if it had to be created.
    fn try_finish(&mut self, mask: u32, stop: u32, time: u32, node: Node) -> Option<u32> {
        let f = self.rules.finish_at(stop)?;
        let (cantons, secs) = (f.cantons, f.secs);
        if mask & self.req == self.req {
            return None;
        }
        let mf = self.rules.adv(self.tt, mask | cantons, stop);
        let ta = time + secs;
        if mf & self.req != self.req || ta >= self.best.arrival {
            return None;
        }
        let idx = self.nodes.len() as u32;
        self.nodes.push(node.into());
        self.nodes.push(Node { parent: idx, kind: Kind::Run, a: stop, b: cantons | FINISH_FLAG, time: ta }.into());
        self.consider(mf, ta, idx + 1);
        Some(idx)
    }

    fn reach(&mut self, mask: u32, stop: u32, time: u32, ready: u32, node: Node) -> (Option<u32>, bool) {
        let fin = self.try_finish(mask, stop, time, node);
        let full = mask & self.req == self.req;
        let ins = if full {
            None
        } else {
            match self.key(mask, stop, ready) {
                Some(k) => insert_stop(&mut self.stop_sets[stop as usize], mask, ready, k, self.p.beam_stop),
                None => None,
            }
        };
        if !full && ins.is_none() {
            return (fin, false);
        }
        let idx = match fin {
            Some(i) => i,
            None => {
                self.nodes.push(node.into());
                self.nodes.len() as u32 - 1
            }
        };
        self.consider(mask, time, idx);
        if let Some(i) = ins {
            self.stop_sets[stop as usize][i].node = idx;
            return (Some(idx), true);
        }
        (Some(idx), false)
    }

    /// Walk / bike from `from` (leaving at `t0`) to every linked stop.
    fn relax_links(&mut self, from: u32, t0: u32, mask: u32, parent: u32) {
        if !self.p.walking {
            return;
        }
        let tt = self.tt;
        let need = self.rules.detour_need(mask, from);
        for (w, d, kind) in tt.footpaths(from) {
            if kind == 1 && !self.p.bike {
                continue;
            }
            // detour obligation open: only platform changes within the rule's stations
            if need != 0 && self.rules.detour_of_stop[w as usize] == 0 {
                continue;
            }
            let m2 = self.rules.adv(tt, mask, w);
            let ta = t0 + d;
            self.reach(m2, w, ta, ta, Node { parent, kind: Kind::Walk, a: from, b: w, time: ta });
        }
    }

    /// Arrive at `stop` at `time` by vehicle (or start there): plain label, plus the bonus
    /// run if the stop is a bonus station. Relaxes walk/bike links from kept labels.
    /// `make_node` is the step that got us here; `pushed` is its index if already created.
    /// `plain = false`: only the bonus run is allowed here (detour obligation still open).
    fn arrive(&mut self, mask: u32, stop: u32, time: u32, change: u32, node: Node, plain: bool) {
        let (mut pushed, kept) = if plain {
            self.reach(mask, stop, time, time + change, node)
        } else {
            // detour obligation still open: a finishing run that completes everything is fine
            (self.try_finish(mask, stop, time, node), false)
        };
        if kept {
            self.relax_links(stop, time, mask, pushed.unwrap());
        }
        let rules = self.rules;
        if let Some(b) = rules.bonus_at(stop) {
            let mb = rules.adv(self.tt, mask | b.cantons, stop); // may complete -> end station
            if mb != mask {
                let parent = match pushed {
                    Some(i) => i,
                    None => {
                        self.nodes.push(node.into());
                        pushed = Some(self.nodes.len() as u32 - 1);
                        self.nodes.len() as u32 - 1
                    }
                };
                let tr = time + b.secs;
                let run = Node { parent, kind: Kind::Run, a: stop, b: b.cantons, time: tr };
                let (ri, kept_b) = self.reach(mb, stop, tr, time + change.max(b.secs), run);
                if kept_b {
                    self.relax_links(stop, tr, mb, ri.unwrap());
                }
            }
        }
        // bike excursions from here (off the vehicle, no detour obligation open). Explicit
        // tricks: they follow their own switch (tight / run-for-it), not --no-bike / --no-walk.
        if plain {
            if let Some(list) = rules.excursions_of_stop.get(&stop) {
                for &e in list {
                    let x = &rules.excursions[e as usize];
                    let (to, tx) = (x.to, time + x.secs);
                    let mx = rules.adv(self.tt, mask | x.cantons, to);
                    let parent = match pushed {
                        Some(i) => i,
                        None => {
                            self.nodes.push(node.into());
                            let i = self.nodes.len() as u32 - 1;
                            pushed = Some(i);
                            i
                        }
                    };
                    let run = Node { parent, kind: Kind::Run, a: stop, b: EXCURSION_FLAG | e, time: tx };
                    let (ri, kept_x) = self.reach(mx, to, tx, tx, run);
                    if kept_x {
                        self.relax_links(to, tx, mx, ri.unwrap());
                    }
                }
            }
        }
    }
}

pub fn solve(tt: &Timetable, bounds: &Bounds, p: &Params) -> Solution {
    let t0 = std::time::Instant::now();
    let rules: &Rules = &p.rules;
    let mut s = Search {
        tt,
        bounds,
        p,
        rules,
        req: rules.goal(p.required),
        nodes: Vec::with_capacity(1 << 20),
        stop_sets: vec![Vec::new(); tt.n_stops()],
        best: Best { arrival: u32::MAX, node: NONE, mask: 0 },
        partial: Best { arrival: u32::MAX, node: NONE, mask: 0 },
    };
    let mut trip_sets: Vec<Vec<TripLabel>> = vec![Vec::new(); tt.trips.len()];
    let horizon = p.start_time.saturating_add(p.max_duration);

    // --- start labels (all platforms of the start station + walks/bikes from them)
    for &st in &p.start_stops {
        let mask = rules.adv(tt, rules.initial, st);
        let node = Node { parent: NONE, kind: Kind::Start, a: st, b: st, time: p.start_time };
        s.arrive(mask, st, p.start_time, 0, node, true);
    }

    // --- main scan
    let first = tt.dep.partition_point(|&d| d < p.start_time);
    let mut scanned = 0u64;
    let mut tmp: Vec<TripLabel> = Vec::new();
    for c in first..tt.n_conns() {
        let dep = tt.dep[c];
        if dep >= s.best.arrival || dep > horizon {
            break;
        }
        scanned += 1;
        let (u, v, t) = (tt.from[c], tt.to[c], tt.trip[c] as usize);
        let fl = tt.flags[c];

        // 1. boarding at u
        if fl & F_BOARD != 0 && !s.stop_sets[u as usize].is_empty() {
            let tkey = |m: u32| s.key(m, u, dep).unwrap_or(u64::MAX);
            for sl in s.stop_sets[u as usize].iter() {
                if sl.ready <= dep {
                    let m = rules.adv(tt, sl.mask, u);
                    if p.upper_bound != u32::MAX && s.key(m, u, dep).is_none() {
                        continue;
                    }
                    // detour rule: only trains that still stop in the obliged canton(s)
                    let need = rules.detour_need(m, u);
                    if need != 0 && tt.ahead[c] & need != need {
                        continue;
                    }
                    insert_trip(
                        &mut trip_sets[t],
                        TripLabel { mask: m, parent: sl.node, board: c as u32, need, ride: rules.ride_bit(tt, u) },
                        p.beam_trip,
                        tkey,
                    );
                }
            }
        }
        if trip_sets[t].is_empty() {
            continue;
        }
        let arr = tt.arr[c];

        // 2. alighting at v (+ bonus run, + walking/cycling from v)
        if fl & F_ALIGHT != 0 {
            let change = tt.stop_change[v as usize];
            for k in 0..trip_sets[t].len() {
                let tl = trip_sets[t][k];
                let m = rules.ride_done(tt, rules.adv(tt, tl.mask, v), tl.ride | rules.ride_bit(tt, v), v);
                let node = Node { parent: tl.parent, kind: Kind::Ride, a: tl.board, b: c as u32, time: arr };
                if tl.need & !m != 0 {
                    // detour rule: not allowed off before the obliged canton, except for a
                    // run-for-it that delivers it (e.g. Nyon -> Genève)
                    if let Some(b) = rules.bonus_at(v) {
                        if tl.need & !(m | b.cantons) == 0 {
                            s.arrive(m, v, arr, change, node, false);
                        }
                    }
                    continue;
                }
                s.arrive(m, v, arr, change, node, true);
            }
        }

        // 3. staying on board through v (hop off / hop on during the halt).
        //    At a bonus station, a halt of at least `secs` also grants the bonus cantons
        //    (run across the border and back while the train waits).
        if fl & F_HALT != 0 {
            let hop = tt.dwell[c] >= p.min_dwell && rules.touches(tt, v);
            let extra = match rules.bonus_at(v) {
                Some(b) if tt.dwell[c] >= b.secs => b.cantons,
                _ => 0,
            };
            if hop || extra != 0 {
                let set = &mut trip_sets[t];
                let mut changed = false;
                let rb = if hop { rules.ride_bit(tt, v) } else { 0 };
                for l in set.iter_mut() {
                    let m = if hop { rules.adv(tt, l.mask | extra, v) } else { l.mask | extra };
                    let r = l.ride | rb;
                    let m = rules.ride_done(tt, m, r, v);
                    changed |= m != l.mask || r != l.ride;
                    l.mask = m;
                    l.ride = r;
                }
                if changed {
                    prune_trip(set);
                }
            }
        }

        // 4. in-seat continuation onto the next trip at v
        let nt = tt.stay_to[t];
        if nt != NONE && tt.stay_stop[t] == v {
            tmp.clear();
            tmp.extend(trip_sets[t].iter().copied());
            for tl in tmp.iter() {
                // materialise the ride so far as a node; the next trip "boards" at v
                let node = s.nodes.len() as u32;
                s.nodes.push(Node { parent: tl.parent, kind: Kind::Ride, a: tl.board, b: c as u32, time: arr }.into());
                let r = tl.ride | rules.ride_bit(tt, v);
                let m = rules.ride_done(tt, rules.adv(tt, tl.mask, v), r, v);
                insert_trip(
                    &mut trip_sets[nt as usize],
                    TripLabel { mask: m, parent: node, board: CONT_BIT | c as u32, need: tl.need, ride: r },
                    p.beam_trip,
                    |mm| s.key(mm, v, arr).unwrap_or(u64::MAX),
                );
            }
        }

        if tt.trip_last_conn[t] as usize == c {
            trip_sets[t] = Vec::new(); // free memory
        }
    }

    let (end, complete) = if s.best.node != NONE { (s.best, true) } else { (s.partial, false) };
    let mut path = vec![];
    let mut n = end.node;
    while n != NONE {
        let node: Node = s.nodes[n as usize].into();
        path.push(node);
        n = node.parent;
    }
    path.reverse();
    Solution {
        path,
        complete,
        mask: end.mask & CANTONS,
        via_reached: (end.mask >> VIA_SHIFT).trailing_ones().min(rules.n_via()),
        arrival: end.arrival,
        stats: Stats { conns_scanned: scanned, nodes: s.nodes.len() as u64, millis: t0.elapsed().as_millis() },
    }
}
