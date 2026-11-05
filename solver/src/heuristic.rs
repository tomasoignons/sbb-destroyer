//! Admissible lower bounds on the remaining travel time.
//!
//! `dist[s * 26 + c]` = minimum possible time [s] to get from stop `s` to *any* stop in
//! canton `c`, on a static graph whose edge weights are the fastest scheduled ride
//! between two consecutive stops (or the walking time). Waiting and transfer times are
//! ignored, so this never overestimates → safe for pruning.
//!
//! For a label at stop `s` missing the canton set M, the remaining time is at least
//! `max_{c in M} dist[s][c]`.
//!
//! With a mandatory end station E (`set_end`): `end_dist[s]` = min time from s to E, and
//! `ce[c]` = min time from any stop of canton c to E. The bound then also includes
//! `end_dist[s]` and `max_{c in M} dist[s][c] + ce[c]` (visit c, then still reach E).

use crate::data::*;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

pub const INF: u32 = u32::MAX / 4;

pub struct Bounds {
    pub dist: Vec<u32>,
    /// cc[a * 26 + b] = min time from any stop of canton a to any stop of canton b
    pub cc: Vec<u32>,
    /// use the pairwise bound when at most this many cantons are missing
    pub pair_limit: u32,
    /// reverse adjacency of the static graph (kept for `set_end`)
    deg: Vec<u32>,
    radj: Vec<(u32, u32)>,
    extra_sources: Vec<(u32, u32)>,
    /// mandatory end station: min time from each stop to it, and from each canton to it
    pub end_dist: Option<Vec<u32>>,
    pub ce: [u32; 26],
}

/// Multi-source reverse Dijkstra: min time from every stop to any of `sources`.
fn reverse_dijkstra(n: usize, deg: &[u32], radj: &[(u32, u32)], sources: impl Iterator<Item = u32>) -> Vec<u32> {
    let mut dist = vec![INF; n];
    let mut heap = BinaryHeap::new();
    for s in sources {
        if dist[s as usize] != 0 {
            dist[s as usize] = 0;
            heap.push(Reverse((0u32, s)));
        }
    }
    while let Some(Reverse((d, v))) = heap.pop() {
        if d > dist[v as usize] {
            continue;
        }
        for &(u, wt) in &radj[deg[v as usize] as usize..deg[v as usize + 1] as usize] {
            let nd = d + wt;
            if nd < dist[u as usize] {
                dist[u as usize] = nd;
                heap.push(Reverse((nd, u)));
            }
        }
    }
    dist
}

impl Bounds {
    /// `extra_sources`: (stop, canton mask) pairs that also count as being in those cantons
    /// (bonus "run for it" stations), so the bound stays admissible.
    pub fn compute(tt: &Timetable, extra_sources: &[(u32, u32)]) -> Bounds {
        let n = tt.n_stops();
        // min ride time per (from, to) pair
        let mut w: HashMap<u64, u32> = HashMap::new();
        for c in 0..tt.n_conns() {
            let k = ((tt.from[c] as u64) << 32) | tt.to[c] as u64;
            let d = tt.arr[c] - tt.dep[c];
            w.entry(k).and_modify(|x| *x = (*x).min(d)).or_insert(d);
        }
        for s in 0..n as u32 {
            for (t, d, _) in tt.footpaths(s) {
                let k = ((s as u64) << 32) | t as u64;
                w.entry(k).and_modify(|x| *x = (*x).min(d)).or_insert(d);
            }
        }
        // reverse CSR: for each v, list of (u, weight) with edge u -> v
        let mut deg = vec![0u32; n + 1];
        for &k in w.keys() {
            deg[(k & 0xffff_ffff) as usize + 1] += 1;
        }
        for i in 0..n {
            deg[i + 1] += deg[i];
        }
        let mut pos = deg.clone();
        let mut radj = vec![(0u32, 0u32); w.len()];
        for (&k, &d) in &w {
            let (u, v) = ((k >> 32) as u32, (k & 0xffff_ffff) as usize);
            radj[pos[v] as usize] = (u, d);
            pos[v] += 1;
        }

        use rayon::prelude::*;
        let per_canton: Vec<Vec<u32>> = (0..26u8)
            .into_par_iter()
            .map(|c| {
                let sources = (0..n as u32)
                    .filter(|&s| tt.stop_canton[s as usize] == c)
                    .chain(extra_sources.iter().filter(|(_, m)| m & (1 << c) != 0).map(|&(s, _)| s));
                reverse_dijkstra(n, &deg, &radj, sources)
            })
            .collect();
        let mut dist = vec![INF; n * 26];
        let mut cc = vec![INF; 26 * 26];
        for (c, d) in per_canton.iter().enumerate() {
            for s in 0..n {
                dist[s * 26 + c] = d[s];
                let a = tt.stop_canton[s];
                if a != NO_CANTON {
                    let k = a as usize * 26 + c;
                    cc[k] = cc[k].min(d[s]);
                }
                // a bonus station is also "in" its bonus cantons for canton-to-canton bounds
                for &(es, cantons) in extra_sources {
                    if es as usize == s {
                        for a2 in 0..26 {
                            if cantons & (1 << a2) != 0 {
                                let k = a2 * 26 + c;
                                cc[k] = cc[k].min(d[s]);
                            }
                        }
                    }
                }
            }
        }
        Bounds {
            dist,
            cc,
            pair_limit: 8,
            deg,
            radj,
            extra_sources: extra_sources.to_vec(),
            end_dist: None,
            ce: [INF; 26],
        }
    }

    /// Enable the end-station terms of the bound.
    pub fn set_end(&mut self, tt: &Timetable, end_stops: &[u32]) {
        let n = tt.n_stops();
        let ed = reverse_dijkstra(n, &self.deg, &self.radj, end_stops.iter().copied());
        let mut ce = [INF; 26];
        for s in 0..n {
            let c = tt.stop_canton[s];
            if c != NO_CANTON {
                ce[c as usize] = ce[c as usize].min(ed[s]);
            }
        }
        for &(s, cantons) in &self.extra_sources {
            for c in 0..26 {
                if cantons & (1 << c) != 0 {
                    ce[c] = ce[c].min(ed[s as usize]);
                }
            }
        }
        self.end_dist = Some(ed);
        self.ce = ce;
    }

    /// Lower bound including the trip to the end station (if one is set and still needed).
    #[inline]
    pub fn h_end(&self, stop: u32, missing: u32, need_end: bool) -> u32 {
        let mut best = if missing == 0 { 0 } else { self.h(stop, missing) };
        if need_end {
            if let Some(ed) = &self.end_dist {
                best = best.max(ed[stop as usize]);
                let row = &self.dist[stop as usize * 26..stop as usize * 26 + 26];
                let mut m = missing;
                while m != 0 {
                    let c = m.trailing_zeros() as usize;
                    best = best.max(row[c].saturating_add(self.ce[c]));
                    m &= m - 1;
                }
            }
        }
        best
    }

    /// Lower bound on the time still needed from `stop` to collect all cantons in `missing`:
    /// the farthest single missing canton, and — when few are missing — the best order to
    /// visit the hardest pair (a then b, or b then a).
    #[inline]
    pub fn h(&self, stop: u32, missing: u32) -> u32 {
        let row = &self.dist[stop as usize * 26..stop as usize * 26 + 26];
        let mut m = missing;
        let mut best = 0;
        while m != 0 {
            let c = m.trailing_zeros() as usize;
            best = best.max(row[c]);
            m &= m - 1;
        }
        if best < INF && missing.count_ones() <= self.pair_limit {
            let mut ma = missing;
            while ma != 0 {
                let a = ma.trailing_zeros() as usize;
                ma &= ma - 1;
                let mut mb = ma;
                while mb != 0 {
                    let b = mb.trailing_zeros() as usize;
                    mb &= mb - 1;
                    let ab = row[a].saturating_add(self.cc[a * 26 + b]);
                    let ba = row[b].saturating_add(self.cc[b * 26 + a]);
                    best = best.max(ab.min(ba));
                }
            }
        }
        best
    }
}
