mod data;
mod heuristic;
mod output;
mod solver;

use clap::Parser;
use data::Timetable;
use output::{fmt_dur, fmt_time};
use rayon::prelude::*;
use heuristic::Bounds;
use solver::{solve, Bonus, Evict, Node, Params, Rules, Solution, Stats, MAX_VIA};
use std::sync::Arc;
use std::cmp::Reverse;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Find the fastest public-transport journey that steps into all 26 Swiss cantons.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// timetable directory produced by prep/build_day.py (e.g. data/days/2026-11-14)
    #[arg(long)]
    data: PathBuf,

    /// start station name, exactly as in the timetable (e.g. "Genève", "Zürich HB")
    #[arg(long)]
    from: Option<String>,

    /// list stations whose name contains this text, then exit
    #[arg(long)]
    find: Option<String>,

    /// earliest start time, HH:MM (may exceed 24:00)
    #[arg(long, default_value = "05:00")]
    start: String,

    /// also try later start times up to this HH:MM (in parallel), keep the shortest trip
    #[arg(long)]
    sweep_end: Option<String>,

    /// step between swept start times [minutes]
    #[arg(long, default_value_t = 10)]
    sweep_step: u32,

    /// beam widths (max labels per stop / per vehicle) of successive passes, e.g. "8,32,128".
    /// Each pass prunes with the best arrival found so far, so wide final passes stay cheap.
    #[arg(long, default_value = "8,32", value_delimiter = ',')]
    beams: Vec<usize>,

    /// minimum scheduled dwell [s] for hop-off/hop-on at an intermediate stop to count.
    /// 0 = any scheduled passenger stop counts; 60 = only stops with >= 1 min dwell
    #[arg(long, default_value_t = 0)]
    min_dwell: u32,

    /// give up after this many hours
    #[arg(long, default_value_t = 36.0)]
    max_hours: f64,

    /// disable walking between nearby stops (changes within a station still need a walk link)
    #[arg(long)]
    no_walk: bool,

    /// which labels to keep when a stop exceeds the beam width
    #[arg(long, value_enum, default_value_t = Evict::Bound)]
    evict: Evict,

    /// --evict spread: minutes of estimated cost per missing canton
    #[arg(long, default_value_t = 30.0)]
    lambda: f64,

    /// use the pairwise lower bound when at most this many cantons are missing
    /// (stronger pruning, slower per label; 0 = off)
    #[arg(long, default_value_t = 8)]
    pair_limit: u32,

    /// prune with a known upper bound on the arrival, HH:MM (e.g. from an earlier run)
    #[arg(long)]
    upper_bound: Option<String>,

    /// disable bike links (walking stays allowed)
    #[arg(long)]
    no_bike: bool,

    /// station the route must pass through, in order (repeat the flag: --via A --via B).
    /// "Pass through" = board, alight, hop off during a halt, or walk/bike there.
    #[arg(long, action = clap::ArgAction::Append)]
    via: Vec<String>,

    /// mandatory end station: the route must finish here, after all required cantons
    /// (and via stations); trip time stops on arrival here
    #[arg(long)]
    to: Option<String>,

    /// the via stations are the FIRST places the route goes to: fastest way to each, in
    /// order, then the free canton search continues from the last one
    #[arg(long)]
    via_first: bool,

    /// "run for it" stations: TSV file  station<TAB>minutes<TAB>canton[,canton]
    /// (alight there and leave >= minutes later -> those cantons count too)
    #[arg(long, default_value = "bonus_stations.tsv")]
    bonus_file: PathBuf,

    /// ignore the bonus stations file
    #[arg(long)]
    no_bonus: bool,

    /// detour rules: TSV file  station<TAB>canton[,canton]  ("at this station without these
    /// cantons -> board only trains that still stop there, and get off only once reached")
    #[arg(long, default_value = "detour_rules.tsv")]
    detour_file: PathBuf,

    /// ignore the detour rules file
    #[arg(long)]
    no_detour: bool,

    /// cantons already validated before this part of the trip (comma-separated names or
    /// codes, e.g. "GE,VD,Neuchâtel"); the search only has to collect the others
    #[arg(long, default_value = "")]
    have: String,

    /// comma-separated canton names NOT required (for experiments)
    #[arg(long, default_value = "")]
    skip: String,

    /// after the search, also try later start times (1-min steps) for up to this many
    /// minutes after the chosen first departure — a later start can mean a shorter trip
    /// (0 = off). With --sweep-end, refines up to the next swept start time instead.
    #[arg(long, default_value_t = 30)]
    refine: u32,

    /// worker threads for parallel start times (default: all cores). Each concurrent
    /// search needs ~1 GB of RAM at beam 32.
    #[arg(long)]
    threads: Option<usize>,

    /// write the best itinerary as JSON to this file
    #[arg(long)]
    json: Option<PathBuf>,
}

/// Official canton codes, in KANTONSNUMMER order (index = canton bit).
const CODES: [&str; 26] = [
    "ZH", "BE", "LU", "UR", "SZ", "OW", "NW", "GL", "ZG", "FR", "SO", "BS", "BL", "SH", "AR", "AI", "SG",
    "GR", "AG", "TG", "TI", "VD", "VS", "NE", "GE", "JU",
];

/// lowercase + strip the accents used in canton names
fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Canton index from a name ("Genève", "geneve") or an official code ("GE").
fn canton_index(tt: &Timetable, name: &str) -> usize {
    let f = fold(name.trim());
    if let Some(i) = CODES.iter().position(|c| c.eq_ignore_ascii_case(&f)) {
        return i;
    }
    tt.canton_names.iter().position(|n| fold(n) == f).unwrap_or_else(|| {
        eprintln!("unknown canton {name:?}; use a code ({}) or one of: {}", CODES.join(" "), tt.canton_names.join(", "));
        std::process::exit(1);
    })
}

fn parse_hm(s: &str) -> u32 {
    let mut it = s.split(':');
    let h: u32 = it.next().unwrap().trim().parse().expect("time HH:MM");
    let m: u32 = it.next().unwrap_or("0").trim().parse().expect("time HH:MM");
    h * 3600 + m * 60
}

fn main() {
    let args = Args::parse();
    if let Some(n) = args.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build_global().unwrap();
    }
    let t0 = std::time::Instant::now();
    let tt = Timetable::load(&args.data);
    eprintln!(
        "loaded {} connections, {} stops, {} trips in {:.1}s",
        tt.n_conns(),
        tt.n_stops(),
        tt.trips.len(),
        t0.elapsed().as_secs_f64()
    );

    if let Some(q) = &args.find {
        let q = q.to_lowercase();
        let mut names: Vec<(&str, &str)> = (0..tt.n_stops() as u32)
            .filter(|&s| tt.stop_name[s as usize].to_lowercase().contains(&q))
            .map(|s| (tt.stop_name[s as usize].as_str(), tt.canton_name(s)))
            .collect();
        names.sort();
        names.dedup();
        for (n, c) in names {
            println!("{n}  [{c}]");
        }
        return;
    }

    let from = args.from.as_deref().expect("--from <station> is required (use --find to search)");
    let start_stops = tt.stops_named(from);
    if start_stops.is_empty() {
        eprintln!("no station named {from:?}; try --find");
        std::process::exit(1);
    }

    let mut required = (1u32 << 26) - 1;
    for name in args.skip.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        required &= !(1 << canton_index(&tt, name));
    }
    let mut initial = 0u32;
    for name in args.have.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        initial |= 1 << canton_index(&tt, name);
    }

    // ---- via stations + bonus stations
    let mut rules = Rules::new(tt.n_stops());
    rules.initial = initial;
    if initial != 0 {
        eprintln!("already validated: {} canton(s), {} to go", initial.count_ones(), 26 - initial.count_ones());
    }
    if args.via.len() > MAX_VIA {
        eprintln!("at most {MAX_VIA} --via stations");
        std::process::exit(1);
    }
    for (k, name) in args.via.iter().enumerate() {
        let stops = tt.stops_named(name);
        if stops.is_empty() {
            eprintln!("no station named {name:?} (--via); try --find");
            std::process::exit(1);
        }
        for s in stops {
            rules.via_of_stop[s as usize] |= 1 << k;
        }
        rules.via_names.push(tt.stop_name[tt.stops_named(name)[0] as usize].clone());
    }
    rules.via_first = args.via_first;
    rules.required = required;
    let mut end_stops = vec![];
    if let Some(name) = &args.to {
        end_stops = tt.stops_named(name);
        if end_stops.is_empty() {
            eprintln!("no station named {name:?} (--to); try --find");
            std::process::exit(1);
        }
        for &s in &end_stops {
            rules.end_of_stop[s as usize] = 1;
        }
        rules.end_name = Some(tt.stop_name[end_stops[0] as usize].clone());
        eprintln!("end station: {}", rules.end_name.as_ref().unwrap());
    }
    if !args.via.is_empty() {
        eprintln!(
            "via (in order{}): {}",
            if args.via_first { ", first and directly" } else { "" },
            rules.via_names.join(" → ")
        );
    }
    if !args.no_bonus {
        match std::fs::read_to_string(&args.bonus_file) {
            Err(_) => eprintln!("note: no bonus stations file at {}", args.bonus_file.display()),
            Ok(text) => {
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    let f: Vec<&str> = line.split('\t').map(str::trim).collect();
                    if f.len() < 3 {
                        eprintln!("bonus file: skipping malformed line {line:?}");
                        continue;
                    }
                    let mut cantons = 0u32;
                    for cn in f[2].split(',').map(str::trim) {
                        match tt.canton_names.iter().position(|n| n.eq_ignore_ascii_case(cn)) {
                            Some(c) => cantons |= 1 << c,
                            None => eprintln!("bonus file: unknown canton {cn:?}"),
                        }
                    }
                    let stops = tt.stops_named(f[0]);
                    let mins: f64 = f[1].parse().unwrap_or(0.0);
                    if stops.is_empty() || cantons == 0 {
                        eprintln!("bonus file: skipping {line:?} (unknown station or no canton)");
                        continue;
                    }
                    let i = rules.bonus.len() as u32;
                    for s in stops {
                        rules.bonus_of_stop[s as usize] = i;
                    }
                    rules.bonus.push(Bonus { name: f[0].to_owned(), secs: (mins * 60.0) as u32, cantons });
                    eprintln!("bonus: alight at {} and leave ≥ {mins} min later → +{}", f[0], f[2]);
                }
            }
        }
    }
    if !args.no_detour {
        if let Ok(text) = std::fs::read_to_string(&args.detour_file) {
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let f: Vec<&str> = line.split('\t').map(str::trim).collect();
                if f.len() < 2 {
                    eprintln!("detour file: skipping malformed line {line:?}");
                    continue;
                }
                let mut cantons = 0u32;
                for cn in f[1].split(',').map(str::trim) {
                    match tt.canton_names.iter().position(|n| n.eq_ignore_ascii_case(cn)) {
                        Some(c) => cantons |= 1 << c,
                        None => eprintln!("detour file: unknown canton {cn:?}"),
                    }
                }
                let stops = tt.stops_named(f[0]);
                if stops.is_empty() || cantons == 0 {
                    eprintln!("detour file: skipping {line:?} (unknown station or no canton)");
                    continue;
                }
                for s in stops {
                    rules.detour_of_stop[s as usize] |= cantons;
                }
                eprintln!("detour: at {} without {} → go there first", f[0], f[1]);
            }
        }
    }
    let rules = Arc::new(rules);

    let base = Params {
        start_stops,
        start_time: parse_hm(&args.start),
        beam_stop: args.beams[0],
        beam_trip: args.beams[0],
        min_dwell: args.min_dwell,
        max_duration: (args.max_hours * 3600.0) as u32,
        required,
        walking: !args.no_walk,
        bike: !args.no_bike,
        evict: args.evict,
        lambda: (args.lambda * 60.0) as u32,
        upper_bound: args.upper_bound.as_deref().map_or(u32::MAX, parse_hm),
        rules: rules.clone(),
    };

    let t1 = std::time::Instant::now();
    let extra: Vec<(u32, u32)> = (0..tt.n_stops() as u32)
        .filter_map(|s| rules.bonus_at(s).map(|b| (s, b.cantons)))
        .collect();
    let mut bounds = Bounds::compute(&tt, &extra);
    bounds.pair_limit = args.pair_limit;
    if !end_stops.is_empty() {
        bounds.set_end(&tt, &end_stops);
    }
    eprintln!("lower bounds computed in {:.1}s", t1.elapsed().as_secs_f64());

    let sweep_step = args.sweep_step.max(1) * 60;
    let starts: Vec<u32> = match &args.sweep_end {
        None => vec![base.start_time],
        Some(e) => (base.start_time..=parse_hm(e)).step_by(sweep_step as usize).collect(),
    };
    let verbose = starts.len() == 1;

    // one start time: successive beams, each pruned by the best arrival so far
    let done = AtomicUsize::new(0);
    let total = AtomicUsize::new(starts.len());
    let beam_solve = |mut p: Params| -> Solution {
        let mut best: Option<Solution> = None;
        for &k in &args.beams {
            p.beam_stop = k;
            p.beam_trip = k;
            let r = solve(&tt, &bounds, &p);
            let better = match &best {
                None => true,
                Some(b) => (r.complete, r.mask.count_ones(), Reverse(r.arrival))
                    > (b.complete, b.mask.count_ones(), Reverse(b.arrival)),
            };
            if verbose {
                eprintln!(
                    "  beam {k:>5}: {} {} cantons, arrive {}  [{} labels, {:.1}s]",
                    if r.complete { "found" } else { "no improvement," },
                    r.mask.count_ones(),
                    fmt_time(r.arrival),
                    r.stats.nodes,
                    r.stats.millis as f64 / 1000.0
                );
            }
            if better {
                if r.complete {
                    p.upper_bound = p.upper_bound.min(r.arrival);
                }
                best = Some(r);
            }
        }
        best.unwrap()
    };

    // --via-first: go to the via stations first (fastest way to each, in order), then run
    // the free canton search from the last one with everything collected so far.
    let via_stops: Vec<Vec<u32>> = rules.via_names.iter().map(|n| tt.stops_named(n)).collect();
    let via_first_solve = |s: u32| -> Solution {
        let mut path: Vec<Node> = vec![];
        let mut stats = Stats::default();
        let mut stops = base.start_stops.clone();
        let mut t = s;
        let mut mask = rules.initial;
        let add = |seg: &Solution, first: bool, path: &mut Vec<Node>, stats: &mut Stats| {
            // later segments start with a Start node at the via station: drop it
            path.extend(seg.path.iter().skip(if first { 0 } else { 1 }).copied());
            stats.conns_scanned += seg.stats.conns_scanned;
            stats.nodes += seg.stats.nodes;
            stats.millis += seg.stats.millis;
        };
        for (k, vs) in via_stops.iter().enumerate() {
            let mut r = (*rules).clone();
            r.via_of_stop = vec![0; tt.n_stops()];
            for &x in vs {
                r.via_of_stop[x as usize] = 1;
            }
            r.via_names = vec![rules.via_names[k].clone()];
            r.detour_of_stop = vec![0; tt.n_stops()]; // the explicit plan overrides detour rules
            r.end_name = None; // the end station only matters in the final free part
            r.initial = mask;
            let p = Params {
                start_stops: stops.clone(),
                start_time: t,
                required: 0, // only the via station matters on this leg
                beam_stop: 16,
                beam_trip: 16,
                evict: Evict::Popcount, // ties on "not there yet" -> earliest kept
                upper_bound: u32::MAX,
                rules: Arc::new(r),
                ..base.clone()
            };
            let seg = solve(&tt, &bounds, &p);
            add(&seg, k == 0, &mut path, &mut stats);
            if !seg.complete {
                return Solution { path, complete: false, mask: seg.mask, via_reached: k as u32, arrival: seg.arrival, stats };
            }
            t = seg.arrival;
            mask = seg.mask;
            stops = vs.clone();
        }
        let mut r = (*rules).clone();
        r.via_of_stop = vec![0; tt.n_stops()];
        r.via_names = vec![];
        r.initial = mask;
        let fin = beam_solve(Params { start_stops: stops, start_time: t, rules: Arc::new(r), ..base.clone() });
        add(&fin, false, &mut path, &mut stats);
        Solution {
            path,
            complete: fin.complete,
            mask: fin.mask,
            via_reached: rules.n_via(),
            arrival: fin.arrival,
            stats,
        }
    };

    let run_start = |s: u32| -> (u32, Solution) {
        let sol = if args.via_first && !via_stops.is_empty() {
            via_first_solve(s)
        } else {
            beam_solve(Params { start_time: s, ..base.clone() })
        };
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("progress {n}/{}", total.load(Ordering::Relaxed));
        (s, sol)
    };
    // shortest trip wins (duration = first boarding -> last canton reached)
    let rank = |r: &Solution| (r.complete, r.mask.count_ones(), Reverse(r.duration(&tt)), Reverse(r.arrival));

    eprintln!("phase search: {} start time(s)", starts.len());
    let mut results: Vec<(u32, Solution)> = starts.par_iter().map(|&s| run_start(s)).collect();

    // refine: starting later than the chosen first departure can only shorten the trip
    // (no idle time before boarding); try every minute until the next swept start time
    if args.refine > 0 {
        let (s0, r0) = results.iter().max_by_key(|(_, r)| rank(r)).unwrap();
        if r0.complete {
            let d = r0.first_departure(&tt);
            let until = if args.sweep_end.is_some() { s0 + sweep_step } else { d + args.refine * 60 };
            let fine: Vec<u32> = (d + 60..until)
                .step_by(60)
                .filter(|s| !results.iter().any(|(x, _)| x == s))
                .collect();
            if !fine.is_empty() {
                total.fetch_add(fine.len(), Ordering::Relaxed);
                eprintln!("phase refine: {} later start times after {}", fine.len(), fmt_time(d));
                results.extend(fine.par_iter().map(|&s| run_start(s)).collect::<Vec<_>>());
            }
        }
    }
    results.sort_by_key(|(s, _)| *s);

    if results.len() > 1 {
        println!("start  first-dep  arrival  duration  cantons  time");
        for (s, r) in &results {
            println!(
                "{}  {:>9}  {:>7}  {:>8}  {:>7}  {:.1}s",
                fmt_time(*s),
                fmt_time(r.first_departure(&tt)),
                fmt_time(r.arrival),
                fmt_dur(r.duration(&tt)),
                r.mask.count_ones(),
                r.stats.millis as f64 / 1000.0
            );
        }
        println!();
    }

    let (_, best) = results.iter().max_by_key(|(_, r)| rank(r)).unwrap();

    output::print_itinerary(&tt, best, args.min_dwell, &rules);
    println!();
    println!(
        "{}: {} cantons, leave {} arrive {}  →  {}   [{} conns scanned, {} labels, {:.1}s]",
        if best.complete { "DONE" } else { "INCOMPLETE" },
        best.mask.count_ones(),
        fmt_time(best.first_departure(&tt)),
        fmt_time(best.arrival),
        fmt_dur(best.duration(&tt)),
        best.stats.conns_scanned,
        best.stats.nodes,
        best.stats.millis as f64 / 1000.0
    );

    if let Some(path) = &args.json {
        let j = output::to_json(&tt, best, args.min_dwell, &rules);
        std::fs::write(path, serde_json::to_string_pretty(&j).unwrap()).expect("write json");
        eprintln!("itinerary written to {}", path.display());
    }
}
