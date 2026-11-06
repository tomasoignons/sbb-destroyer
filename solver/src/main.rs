mod data;
mod heuristic;
mod output;
mod repair;
mod solver;

use clap::Parser;
use data::Timetable;
use output::{fmt_dur, fmt_time};
use rayon::prelude::*;
use heuristic::Bounds;
use solver::{solve, Bonus, Evict, Excursion, Node, Params, Rules, Solution, Stats, MAX_VIA};
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

    /// ignore the bonus stations, finish stations and excursions files (except "tight"
    /// lines: see --no-tight)
    #[arg(long)]
    no_bonus: bool,

    /// finishing runs: TSV file  station<TAB>minutes<TAB>canton[,canton]  (get off there
    /// when only those cantons are missing -> run across the border, trip ends minutes later)
    #[arg(long, default_value = "finish_stations.tsv")]
    finish_file: PathBuf,

    /// bike excursions: TSV file  from<TAB>to<TAB>minutes<TAB>canton[,canton][<TAB>tight]
    /// (get off at `from`, cycle through those cantons, arrive at `to` minutes later)
    #[arg(long, default_value = "excursions.tsv")]
    excursion_file: PathBuf,

    /// leave out the tight tricks (lines marked "tight" in the bonus / excursions files)
    #[arg(long)]
    no_tight: bool,

    /// change-time overrides: TSV file  station<TAB>minutes  (time to change trains at
    /// this station, same platform or between its platforms; e.g. Cheyres 0 = single-track
    /// crossing, the train waits for the other one)
    #[arg(long, default_value = "change_times.tsv")]
    change_file: PathBuf,

    /// 0-minute transfers everywhere: changing at the same stop or between platforms of the
    /// same station takes no time (walks to differently named stops keep their time)
    #[arg(long)]
    zero_change: bool,

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

    /// leave out this run-for-it station of the bonus file (repeatable), e.g. a tight one
    #[arg(long)]
    skip_bonus: Vec<String>,

    /// cantons that only count when all are stepped on during one single ride
    /// (comma-separated names or codes), e.g. "BS,BL,JU,SO,BE" = the Basel SBB ↔
    /// Biel/Bienne train. Forces that ride; the beam then doesn't chase them one by one.
    #[arg(long, default_value = "")]
    ride_together: String,

    /// comma-separated canton names NOT required (for experiments)
    #[arg(long, default_value = "")]
    skip: String,

    /// after the search, also try later start times (1-min steps) for up to this many
    /// minutes after the chosen first departure — a later start can mean a shorter trip
    /// (0 = off). With --sweep-end, refines up to the next swept start time instead.
    #[arg(long, default_value_t = 60)]
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

/// Sorted times `t` at which starting at `t` makes a new first boarding possible: a
/// departure at a start stop, or one at a stop reachable on foot / by bike from a start
/// stop (departure minus the link time). Between two consecutive such times, a later
/// start lets nothing new happen first, so it gives the same search.
fn first_boarding_times(tt: &Timetable, start_stops: &[u32], walking: bool, bike: bool) -> Vec<u32> {
    // per stop: smallest offset (link time) from any start stop, u32::MAX = unreachable
    let mut off = vec![u32::MAX; tt.n_stops()];
    for &st in start_stops {
        off[st as usize] = 0;
        if walking {
            for (w, d, kind) in tt.footpaths(st) {
                if kind == 1 && !bike {
                    continue;
                }
                off[w as usize] = off[w as usize].min(d);
            }
        }
    }
    let mut times: Vec<u32> = (0..tt.n_conns())
        .filter(|&c| tt.flags[c] & data::F_BOARD != 0)
        .filter_map(|c| {
            let o = off[tt.from[c] as usize];
            (o != u32::MAX).then(|| tt.dep[c].saturating_sub(o))
        })
        .collect();
    times.sort_unstable();
    times.dedup();
    times
}

/// Start minutes in `lo..=hi` (1-min steps) that differ from the minute before: some first
/// boarding becomes impossible between them (see `first_boarding_times`).
fn distinct_minutes(times: &[u32], lo: u32, hi: u32) -> Vec<u32> {
    (lo..=hi)
        .step_by(60)
        .filter(|&m| {
            let i = times.partition_point(|&t| t < m.saturating_sub(60));
            i < times.len() && times[i] < m
        })
        .collect()
}

/// 0-minute transfers: no change time at any stop, and free links between stops (platforms)
/// with the same name.
fn zero_change_times(tt: &mut Timetable) {
    tt.stop_change.iter_mut().for_each(|c| *c = 0);
    let mut n = 0;
    for s in 0..tt.n_stops() {
        for k in tt.foot_off[s] as usize..tt.foot_off[s + 1] as usize {
            let w = tt.foot_to[k] as usize;
            if tt.foot_kind.get(k).copied().unwrap_or(0) == 0 && tt.stop_name[w] == tt.stop_name[s] {
                tt.foot_dur[k] = 0;
                n += 1;
            }
        }
    }
    eprintln!("0-minute transfers: everywhere ({n} platform links)");
}

/// Change-time overrides (`station<TAB>minutes`): the change time at every platform of the
/// station, and the platform-to-platform links between them (links to other stops keep
/// their time).
fn apply_change_overrides(tt: &mut Timetable, path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        let stops = tt.stops_named(f[0]);
        let mins: Option<f64> = f.get(1).and_then(|m| m.parse().ok());
        let (Some(mins), false) = (mins, stops.is_empty()) else {
            eprintln!("change times file: skipping {line:?} (unknown station or bad minutes)");
            continue;
        };
        let secs = (mins * 60.0) as u32;
        for &s in &stops {
            tt.stop_change[s as usize] = secs;
            let (a, b) = (tt.foot_off[s as usize] as usize, tt.foot_off[s as usize + 1] as usize);
            for k in a..b {
                if stops.contains(&tt.foot_to[k]) && tt.foot_kind.get(k).copied().unwrap_or(0) == 0 {
                    tt.foot_dur[k] = secs;
                }
            }
        }
        eprintln!("change time at {}: {mins} min ({} platform(s))", f[0], stops.len());
    }
}

fn parse_cantons(tt: &Timetable, list: &str, what: &str) -> u32 {
    let mut cantons = 0u32;
    for cn in list.split(',').map(str::trim) {
        match tt.canton_names.iter().position(|n| n.eq_ignore_ascii_case(cn)) {
            Some(c) => cantons |= 1 << c,
            None => eprintln!("{what} file: unknown canton {cn:?}"),
        }
    }
    cantons
}

/// Bike excursions file: `from<TAB>to<TAB>minutes<TAB>canton[,canton][<TAB>tight]`.
/// Returns (from stops, excursion per destination platform, tight).
fn read_excursions(tt: &Timetable, path: &std::path::Path) -> Vec<(Vec<u32>, Vec<Excursion>, bool)> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let mut out = vec![];
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 4 {
            eprintln!("excursions file: skipping malformed line {line:?}");
            continue;
        }
        let (from, to) = (tt.stops_named(f[0]), tt.stops_named(f[1]));
        let mins: f64 = f[2].parse().unwrap_or(-1.0);
        let cantons = parse_cantons(tt, f[3], "excursions");
        if from.is_empty() || to.is_empty() || mins < 0.0 {
            eprintln!("excursions file: skipping {line:?} (unknown station or bad minutes)");
            continue;
        }
        let tight = f.get(4).is_some_and(|x| x.eq_ignore_ascii_case("tight"));
        let xs = to
            .iter()
            .map(|&t| Excursion {
                from_name: f[0].to_owned(),
                to_name: f[1].to_owned(),
                to: t,
                secs: (mins * 60.0) as u32,
                cantons,
            })
            .collect();
        out.push((from, xs, tight));
    }
    out
}

/// `station<TAB>minutes<TAB>canton[,canton]` lines (bonus / finish stations files).
fn read_station_file(tt: &Timetable, path: &std::path::Path, what: &str) -> Vec<(Vec<u32>, Bonus, bool)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("note: no {what} stations file at {}", path.display());
        return vec![];
    };
    let mut out = vec![];
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 3 {
            eprintln!("{what} file: skipping malformed line {line:?}");
            continue;
        }
        let mut cantons = 0u32;
        for cn in f[2].split(',').map(str::trim) {
            match tt.canton_names.iter().position(|n| n.eq_ignore_ascii_case(cn)) {
                Some(c) => cantons |= 1 << c,
                None => eprintln!("{what} file: unknown canton {cn:?}"),
            }
        }
        let stops = tt.stops_named(f[0]);
        let mins: f64 = f[1].parse().unwrap_or(0.0);
        if stops.is_empty() || cantons == 0 {
            eprintln!("{what} file: skipping {line:?} (unknown station or no canton)");
            continue;
        }
        let tight = f.get(3).is_some_and(|x| x.eq_ignore_ascii_case("tight"));
        out.push((stops, Bonus { name: f[0].to_owned(), secs: (mins * 60.0) as u32, cantons }, tight));
    }
    out
}

fn canton_list(tt: &Timetable, mask: u32) -> String {
    (0..26).filter(|c| mask & (1 << c) != 0).map(|c| tt.canton_names[c].as_str()).collect::<Vec<_>>().join(", ")
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
    let mut tt = Timetable::load(&args.data);
    apply_change_overrides(&mut tt, &args.change_file);
    if args.zero_change {
        zero_change_times(&mut tt);
    }
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
    for name in args.ride_together.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        rules.ride_group |= 1 << canton_index(&tt, name);
    }
    rules.ride_group &= required & !initial; // nothing to force if already validated / not needed
    if rules.ride_group != 0 {
        eprintln!("one ride must step into all of: {}", args.ride_together);
    }
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
    // tight tricks (4th / 5th column "tight") follow --no-tight, the others --no-bonus
    let use_line = |tight: bool| if tight { !args.no_tight } else { !args.no_bonus };
    for (stops, b, tight) in read_station_file(&tt, &args.bonus_file, "bonus") {
        if !use_line(tight) || args.skip_bonus.iter().any(|x| x.eq_ignore_ascii_case(&b.name)) {
            eprintln!("bonus: {} left out", b.name);
            continue;
        }
        eprintln!("bonus: alight at {} and leave ≥ {} min later → +{}{}", b.name, b.secs / 60,
            canton_list(&tt, b.cantons), if tight { "  (tight)" } else { "" });
        for s in stops {
            rules.bonus_of_stop[s as usize] = rules.bonus.len() as u32;
        }
        rules.bonus.push(b);
    }
    for (stops, f, tight) in read_station_file(&tt, &args.finish_file, "finish") {
        if !use_line(tight) {
            continue;
        }
        eprintln!("finish: get off at {} with only {} missing → run there, done {} min later",
            f.name, canton_list(&tt, f.cantons), f.secs / 60);
        for s in stops {
            rules.finish_of_stop[s as usize] = rules.finish.len() as u32;
        }
        rules.finish.push(f);
    }
    for (from, xs, tight) in read_excursions(&tt, &args.excursion_file) {
        let x0 = &xs[0];
        if !use_line(tight) {
            eprintln!("excursion: {} → {} left out", x0.from_name, x0.to_name);
            continue;
        }
        eprintln!("excursion: get off at {}, bike via {} → {} in {} min{}", x0.from_name,
            canton_list(&tt, x0.cantons), x0.to_name, x0.secs / 60, if tight { "  (tight)" } else { "" });
        for x in xs {
            let e = rules.excursions.len() as u32;
            for &s in &from {
                rules.excursions_of_stop.entry(s).or_default().push(e);
            }
            rules.excursions.push(x);
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
        .filter_map(|s| {
            let c = rules.bonus_at(s).map_or(0, |b| b.cantons)
                | rules.finish_at(s).map_or(0, |f| f.cantons)
                | rules.excursions_of_stop.get(&s).map_or(0, |l| {
                    l.iter().fold(0, |m, &e| m | rules.excursions[e as usize].cantons)
                });
            (c != 0).then_some((s, c))
        })
        .collect();
    let mut bounds = Bounds::compute(&tt, &extra);
    bounds.pair_limit = args.pair_limit;
    if !end_stops.is_empty() {
        bounds.set_end(&tt, &end_stops);
    }
    eprintln!("lower bounds computed in {:.1}s", t1.elapsed().as_secs_f64());

    let sweep_step = args.sweep_step.max(1) * 60;
    let mut starts: Vec<u32> = match &args.sweep_end {
        None => vec![base.start_time],
        Some(e) => (base.start_time..=parse_hm(e)).step_by(sweep_step as usize).collect(),
    };
    let s0 = base.start_time;
    let boardings = first_boarding_times(&tt, &base.start_stops, base.walking, base.bike);

    // Single start time + refine: rather than run the main search alone on one core and only
    // then the refine starts, estimate the first departure with a quick narrow pass and run
    // the likely refine starts in parallel with the main search. The refine step below
    // still fills in whatever the estimate missed.
    if args.refine > 0 && args.sweep_end.is_none() && !(args.via_first && !rules.via_names.is_empty()) {
        let k = args.beams[0];
        let quick = solve(&tt, &bounds, &Params { beam_stop: k, beam_trip: k, ..base.clone() });
        if quick.complete {
            let d0 = quick.first_departure(&tt);
            let spec = distinct_minutes(&boardings, d0 + 60, d0 + args.refine * 60 - 1);
            eprintln!(
                "quick pass (beam {k}): first departure {} → {} start times in parallel ({:.1}s)",
                fmt_time(d0),
                spec.len() + 1,
                quick.stats.millis as f64 / 1000.0
            );
            starts.extend(spec);
        }
    }
    // per-beam details for a lone start time, or for the requested one next to the refine starts
    let solo = starts.len() == 1;
    let verbose_for = |s: u32| solo || (args.sweep_end.is_none() && s == s0);

    // one start time: successive beams, each pruned by the best arrival so far
    let done = AtomicUsize::new(0);
    let total = AtomicUsize::new(starts.len());
    let beam_solve = |mut p: Params| -> Solution {
        let verbose = verbose_for(p.start_time);
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
    // (no idle time before boarding); try every minute until the next swept start time.
    // Without a sweep, refine after the requested start's own result (the extra starts
    // run above are only there to keep the cores busy). Minutes that make no new first
    // boarding possible give the same search as the minute before and are skipped.
    if args.refine > 0 {
        let (sa, r0) = if args.sweep_end.is_some() {
            results.iter().max_by_key(|(_, r)| rank(r)).unwrap()
        } else {
            results.iter().find(|(x, _)| *x == s0).unwrap()
        };
        if r0.complete {
            let d = r0.first_departure(&tt);
            let until = if args.sweep_end.is_some() { sa + sweep_step } else { d + args.refine * 60 };
            let all = (d + 60..until).step_by(60).count();
            let fine: Vec<u32> = distinct_minutes(&boardings, d + 60, until.saturating_sub(1))
                .into_iter()
                .filter(|s| !results.iter().any(|(x, _)| x == s))
                .collect();
            if !fine.is_empty() {
                total.fetch_add(fine.len(), Ordering::Relaxed);
                eprintln!(
                    "phase refine: {} later start times after {} ({} already done or nothing new departs)",
                    fine.len(),
                    fmt_time(d),
                    all - fine.len()
                );
                results.extend(fine.par_iter().map(|&s| run_start(s)).collect::<Vec<_>>());
            }
        }
    }
    // use the bike only where nothing else is as fast (same next connections, so the trip
    // never gets longer)
    if !args.no_bike {
        let before: usize = results.iter().map(|(_, r)| repair::bike_legs(&tt, r)).sum();
        let replaced: usize = results
            .iter_mut()
            .filter(|(_, r)| r.complete)
            .map(|(_, r)| repair::reduce_bike(&tt, r, &rules))
            .sum();
        if before > 0 {
            eprintln!("bike: {replaced} of {before} bike leg(s) replaced by trains/walking that are just as fast");
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

    // same trip time: fewer bike legs wins
    let (_, best) = results
        .iter()
        .max_by_key(|(_, r)| {
            (r.complete, r.mask.count_ones(), Reverse(r.duration(&tt)), Reverse(repair::bike_legs(&tt, r)), Reverse(r.arrival))
        })
        .unwrap();

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
