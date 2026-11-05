//! Turning a solution path into a readable itinerary / JSON.

use crate::data::*;
use crate::solver::*;
use serde_json::json;

pub fn fmt_time(t: u32) -> String {
    let d = t / 86400;
    let r = t % 86400;
    let s = format!("{:02}:{:02}", r / 3600, (r % 3600) / 60);
    if d > 0 {
        format!("{s}+{d}")
    } else {
        s
    }
}

pub fn fmt_dur(s: u32) -> String {
    format!("{}h{:02}", s / 3600, (s % 3600) / 60)
}

/// A gained canton: (canton index, stop, time, how)
pub struct Gain {
    pub canton: u8,
    /// Some(k) if this is "via station k reached" instead of a canton
    pub via: Option<u8>,
    pub stop: u32,
    pub time: u32,
    pub how: &'static str,
}

pub struct Leg {
    pub kind: &'static str, // "start" | "ride" | "walk" | "bike"
    pub trip: u32,
    pub from: u32,
    pub to: u32,
    pub dep: u32,
    pub arr: u32,
    pub stayed_seated: bool,
    pub gains: Vec<Gain>,
    /// every stop of the leg with its time (arrival; departure for the first one)
    pub path: Vec<(u32, u32)>,
}

fn trip_conns(tt: &Timetable, trip: u32, hint_last: usize) -> Vec<usize> {
    // connections are sorted by departure; scan backwards from the trip's last connection
    let mut v: Vec<usize> = (0..=hint_last).rev().filter(|&c| tt.trip[c] == trip).collect();
    v.reverse();
    v
}

/// `Gain::via` value meaning "end station reached"
pub const END_MARK: u8 = u8::MAX;

pub fn legs(tt: &Timetable, sol: &Solution, min_dwell: u32, rules: &Rules) -> Vec<Leg> {
    let mut mask = rules.initial;
    let mut out = vec![];
    // replay the solver's mask updates to find where each canton / via station was reached
    let gain = |mask: &mut u32, stop: u32, time: u32, how: &'static str, gains: &mut Vec<Gain>| {
        let new = rules.adv(tt, *mask, stop);
        let diff = new & !*mask;
        *mask = new;
        for c in 0..26u8 {
            if diff & (1 << c) != 0 {
                gains.push(Gain { canton: c, via: None, stop, time, how });
            }
        }
        for k in 0..MAX_VIA as u8 {
            if diff & (1 << (VIA_SHIFT + k as u32)) != 0 {
                gains.push(Gain { canton: 255, via: Some(k), stop, time, how });
            }
        }
        if diff & END_BIT != 0 {
            gains.push(Gain { canton: 255, via: Some(END_MARK), stop, time, how });
        }
    };
    let mut prev_time = 0;
    for n in &sol.path {
        match n.kind {
            Kind::Start => {
                let mut g = vec![];
                gain(&mut mask, n.a, n.time, "start", &mut g);
                out.push(Leg {
                    kind: "start",
                    trip: NONE,
                    from: n.a,
                    to: n.a,
                    dep: n.time,
                    arr: n.time,
                    stayed_seated: false,
                    gains: g,
                    path: vec![(n.a, n.time)],
                });
            }
            Kind::Walk => {
                let bike = tt.link_kind(n.a, n.b) == 1;
                let mut g = vec![];
                gain(&mut mask, n.b, n.time, if bike { "bike" } else { "walk" }, &mut g);
                out.push(Leg {
                    kind: if bike { "bike" } else { "walk" },
                    trip: NONE,
                    from: n.a,
                    to: n.b,
                    dep: prev_time,
                    arr: n.time,
                    stayed_seated: false,
                    gains: g,
                    path: vec![(n.a, prev_time), (n.b, n.time)],
                });
            }
            Kind::Run => {
                // run out of a bonus station into the neighbouring canton(s) and back
                let mut g = vec![];
                for c in 0..26u8 {
                    let b = 1u32 << c;
                    if n.b & b != 0 && mask & b == 0 {
                        mask |= b;
                        g.push(Gain { canton: c, via: None, stop: n.a, time: n.time, how: "run" });
                    }
                }
                gain(&mut mask, n.a, n.time, "run", &mut g); // may complete -> end station
                out.push(Leg {
                    kind: "run",
                    trip: NONE,
                    from: n.a,
                    to: n.a,
                    dep: prev_time,
                    arr: n.time,
                    stayed_seated: false,
                    gains: g,
                    path: vec![(n.a, prev_time)],
                });
            }
            Kind::Ride => {
                let alight = n.b as usize;
                let trip = tt.trip[alight];
                let cont = n.a & CONT_BIT != 0;
                let bc = (n.a & !CONT_BIT) as usize;
                let (bstop, btime) = if cont { (tt.to[bc], tt.arr[bc]) } else { (tt.from[bc], tt.dep[bc]) };
                let conns = trip_conns(tt, trip, alight);
                let start = conns
                    .iter()
                    .position(|&c| tt.from[c] == bstop && tt.dep[c] >= btime)
                    .unwrap_or(0);
                let mut g = vec![];
                let mut path = vec![(bstop, btime)];
                gain(&mut mask, bstop, btime, "board", &mut g);
                for &c in &conns[start..] {
                    path.push((tt.to[c], tt.arr[c]));
                    if c == alight {
                        gain(&mut mask, tt.to[c], tt.arr[c], "alight", &mut g);
                        break;
                    }
                    if tt.flags[c] & F_HALT != 0 && tt.dwell[c] >= min_dwell {
                        gain(&mut mask, tt.to[c], tt.arr[c], "hop off/on", &mut g);
                    }
                    // bonus station: run across the border while the train waits long enough
                    if tt.flags[c] & F_HALT != 0 {
                        if let Some(b) = rules.bonus_at(tt.to[c]) {
                            if tt.dwell[c] >= b.secs {
                                for k in 0..26u8 {
                                    if b.cantons & (1 << k) != 0 && mask & (1 << k) == 0 {
                                        mask |= 1 << k;
                                        g.push(Gain {
                                            canton: k,
                                            via: None,
                                            stop: tt.to[c],
                                            time: tt.arr[c],
                                            how: "run while the train waits",
                                        });
                                    }
                                }
                                if tt.dwell[c] >= min_dwell {
                                    gain(&mut mask, tt.to[c], tt.arr[c], "hop off/on", &mut g); // end station
                                }
                            }
                        }
                    }
                }
                out.push(Leg {
                    kind: "ride",
                    trip,
                    from: bstop,
                    to: tt.to[alight],
                    dep: if cont { tt.arr[bc] } else { tt.dep[bc] },
                    arr: tt.arr[alight],
                    stayed_seated: cont,
                    gains: g,
                    path,
                });
            }
        }
        prev_time = n.time;
    }
    out
}

pub fn print_itinerary(tt: &Timetable, sol: &Solution, min_dwell: u32, rules: &Rules) {
    let legs = legs(tt, sol, min_dwell, rules);
    let mut count = rules.initial.count_ones() as usize;
    if rules.initial != 0 {
        let names: Vec<&str> = (0..26).filter(|c| rules.initial & (1 << c) != 0)
            .map(|c| tt.canton_names[c].as_str()).collect();
        println!("ALREADY VALIDATED ({count}): {}", names.join(", "));
    }
    let mut i = 0;
    let n_via = rules.via_names.len();
    // one line per canton gained / via station reached
    let gain_lines = |l: &Leg, count: &mut usize| {
        for g in &l.gains {
            match g.via {
                Some(END_MARK) => println!(
                    "        ■ END: {} reached ({} at {})",
                    rules.end_name.as_deref().unwrap_or("?"),
                    g.how,
                    fmt_time(g.time)
                ),
                Some(k) => println!(
                    "        → via {}/{n_via}: {:<17} {} at {} ({})",
                    k + 1,
                    rules.via_names[k as usize],
                    g.how,
                    tt.stop_name[g.stop as usize],
                    fmt_time(g.time)
                ),
                None => {
                    *count += 1;
                    println!(
                        "        + {:<24} {} at {} ({})  ({count}/26)",
                        tt.canton_names[g.canton as usize],
                        g.how,
                        tt.stop_name[g.stop as usize],
                        fmt_time(g.time)
                    );
                }
            }
        }
    };
    for l in &legs {
        match l.kind {
            "start" => {
                println!(
                    "START  {}  at {}  [{}]",
                    tt.stop_name[l.from as usize],
                    fmt_time(l.dep),
                    tt.canton_name(l.from)
                );
                gain_lines(l, &mut count);
            }
            "run" => {
                i += 1;
                println!(
                    "{i:3}. {} → {}  RUN   from {} across the border and back (leave ≥ {} min after arriving)",
                    fmt_time(l.dep),
                    fmt_time(l.arr),
                    tt.stop_name[l.from as usize],
                    (l.arr - l.dep + 59) / 60
                );
                gain_lines(l, &mut count);
            }
            "walk" | "bike" => {
                if l.gains.is_empty() && tt.stop_name[l.from as usize] == tt.stop_name[l.to as usize] {
                    continue; // platform change inside a station
                }
                i += 1;
                println!(
                    "{i:3}. {} → {}  {} {:.1} km  {} → {}",
                    fmt_time(l.dep),
                    fmt_time(l.arr),
                    l.kind.to_uppercase(),
                    tt.link_km(l.from, l.to),
                    tt.stop_name[l.from as usize],
                    tt.stop_name[l.to as usize],
                );
                gain_lines(l, &mut count);
            }
            _ => {
                i += 1;
                let t = &tt.trips[l.trip as usize];
                let seated = if l.stayed_seated { " (stay seated)" } else { "" };
                println!(
                    "{i:3}. {} → {}  {:<10} {} → {}   (dir. {}{}){seated}",
                    fmt_time(l.dep),
                    fmt_time(l.arr),
                    tt.vehicle(l.trip),
                    tt.stop_name[l.from as usize],
                    tt.stop_name[l.to as usize],
                    t.headsign,
                    if t.trip_short_name.is_empty() { String::new() } else { format!(", n° {}", t.trip_short_name) },
                );
                gain_lines(l, &mut count);
            }
        }
    }
    if let Some(e) = &rules.end_name {
        if !sol.complete {
            println!("END NOT REACHED: {e}");
        }
    }
    if (sol.via_reached as usize) < n_via {
        println!("VIA NOT REACHED: {}", rules.via_names[sol.via_reached as usize..].join(" → "));
    }
    let missing: Vec<&str> = (0..26)
        .filter(|&c| sol.mask & (1 << c) == 0)
        .map(|c| tt.canton_names[c].as_str())
        .collect();
    if !missing.is_empty() {
        println!("MISSING: {}", missing.join(", "));
    }
}

pub fn to_json(tt: &Timetable, sol: &Solution, min_dwell: u32, rules: &Rules) -> serde_json::Value {
    let via = &rules.via_names;
    let stop = |s: u32| {
        json!({
            "id": tt.stop_id[s as usize],
            "name": tt.stop_name[s as usize],
            "canton": tt.canton_name(s),
            "lv95": [tt.stop_xy[s as usize].0, tt.stop_xy[s as usize].1],
            "lat": tt.stop_ll[s as usize].0,
            "lon": tt.stop_ll[s as usize].1,
        })
    };
    let legs: Vec<_> = legs(tt, sol, min_dwell, rules)
        .iter()
        .map(|l| {
            let trip = if l.trip == NONE {
                serde_json::Value::Null
            } else {
                let t = &tt.trips[l.trip as usize];
                json!({
                    "vehicle": tt.vehicle(l.trip),
                    "trip_id": t.trip_id,
                    "number": t.trip_short_name,
                    "headsign": t.headsign,
                    "agency": t.agency,
                    "route_type": t.route_type,
                })
            };
            json!({
                "kind": l.kind,
                "from": stop(l.from),
                "to": stop(l.to),
                "path": l.path.iter().map(|&(s, t)| json!({
                    "name": tt.stop_name[s as usize],
                    "lat": tt.stop_ll[s as usize].0,
                    "lon": tt.stop_ll[s as usize].1,
                    "canton": tt.canton_name(s),
                    "time": fmt_time(t),
                })).collect::<Vec<_>>(),
                "dep": fmt_time(l.dep), "dep_s": l.dep,
                "arr": fmt_time(l.arr), "arr_s": l.arr,
                "stayed_seated": l.stayed_seated,
                "km": if l.kind == "walk" || l.kind == "bike" {
                    json!((tt.link_km(l.from, l.to) * 10.0).round() / 10.0)
                } else { serde_json::Value::Null },
                "trip": trip,
                "end_reached": l.gains.iter().any(|g| g.via == Some(END_MARK)),
                "via_hits": l.gains.iter().filter(|g| g.via != Some(END_MARK)).filter_map(|g| g.via.map(|k| json!({
                    "index": k,
                    "station": via[k as usize],
                    "stop": tt.stop_name[g.stop as usize],
                    "time": fmt_time(g.time),
                    "how": g.how,
                }))).collect::<Vec<_>>(),
                "new_cantons": l.gains.iter().filter(|g| g.via.is_none()).map(|g| json!({
                    "canton": tt.canton_names[g.canton as usize],
                    "stop": tt.stop_name[g.stop as usize],
                    "time": fmt_time(g.time),
                    "how": g.how,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "date": tt.date,
        "complete": sol.complete,
        "cantons_visited": sol.mask.count_ones(),
        "via": via,
        "via_first": rules.via_first,
        "end": rules.end_name,
        "already_validated": (0..26).filter(|c| rules.initial & (1 << c) != 0)
            .map(|c| tt.canton_names[c].clone()).collect::<Vec<_>>(),
        "via_reached": sol.via_reached,
        "first_departure": fmt_time(sol.first_departure(tt)),
        "arrival": fmt_time(sol.arrival),
        "duration_s": sol.duration(tt),
        "duration": fmt_dur(sol.duration(tt)),
        "legs": legs,
    })
}
