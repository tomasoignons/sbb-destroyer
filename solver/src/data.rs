//! Loading the binary timetable produced by `prep/build_day.py`.

use std::fs;
use std::path::Path;

pub const NO_CANTON: u8 = 255;
pub const NONE: u32 = u32::MAX;

/// Connection flags (see build_day.py).
pub const F_BOARD: u32 = 1; // passengers may board at `from`
pub const F_ALIGHT: u32 = 2; // passengers may alight at `to`
pub const F_HALT: u32 = 4; // vehicle halts at `to` for passengers and continues afterwards

pub struct TripInfo {
    pub trip_id: String,
    pub route_short_name: String,
    pub route_desc: String,
    pub route_type: u32,
    pub trip_short_name: String,
    pub headsign: String,
    pub agency: String,
}

pub struct Timetable {
    // connections, sorted by (dep, arr, trip, sequence)
    pub dep: Vec<u32>,
    pub arr: Vec<u32>,
    pub from: Vec<u32>,
    pub to: Vec<u32>,
    pub trip: Vec<u32>,
    pub flags: Vec<u32>,
    pub dwell: Vec<u32>,
    /// index of the last connection of each trip (to free its labels)
    pub trip_last_conn: Vec<u32>,
    /// per connection: cantons where this trip stops (for passengers to alight) at `to`
    /// or anywhere after it
    pub ahead: Vec<u32>,

    // stops
    pub stop_id: Vec<String>,
    pub stop_name: Vec<String>,
    pub stop_canton: Vec<u8>,
    pub stop_change: Vec<u32>,
    pub stop_xy: Vec<(f64, f64)>,
    /// WGS84 (lat, lon); (0, 0) if unknown or timetable built by an older build_day.py
    pub stop_ll: Vec<(f64, f64)>,

    // footpaths (CSR)
    pub foot_off: Vec<u32>,
    pub foot_to: Vec<u32>,
    pub foot_dur: Vec<u32>,
    /// 0 = walk, 1 = bike
    pub foot_kind: Vec<u8>,
    /// road / straight-line factors used by build_day (for displaying distances)
    pub walk_detour: f64,
    pub bike_detour: f64,

    // in-seat continuations: trip -> (next trip, stop where it happens)
    pub stay_to: Vec<u32>,
    pub stay_stop: Vec<u32>,

    pub trips: Vec<TripInfo>,
    pub canton_names: Vec<String>,
    pub date: String,
}

fn read_u32(dir: &Path, name: &str) -> Vec<u32> {
    let p = dir.join(name);
    let bytes = fs::read(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
    bytes
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn read_tsv(dir: &Path, name: &str) -> Vec<Vec<String>> {
    let p = dir.join(name);
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
    text.lines()
        .skip(1)
        .map(|l| l.split('\t').map(str::to_owned).collect())
        .collect()
}

impl Timetable {
    pub fn load(dir: &Path) -> Timetable {
        let dep = read_u32(dir, "conn_dep.bin");
        let arr = read_u32(dir, "conn_arr.bin");
        let from = read_u32(dir, "conn_from.bin");
        let to = read_u32(dir, "conn_to.bin");
        let trip = read_u32(dir, "conn_trip.bin");
        let flags = read_u32(dir, "conn_flags.bin");
        let dwell = read_u32(dir, "conn_dwell.bin");

        let mut stop_id = vec![];
        let mut stop_name = vec![];
        let mut stop_canton = vec![];
        let mut stop_change = vec![];
        let mut stop_xy = vec![];
        let mut stop_ll = vec![];
        for (i, r) in read_tsv(dir, "stops.tsv").into_iter().enumerate() {
            // sidx stop_id stop_name canton x y change parent_station
            assert_eq!(r[0].parse::<usize>().unwrap(), i, "stops.tsv must be sorted by sidx");
            stop_id.push(r[1].clone());
            stop_name.push(r[2].clone());
            let c: i32 = r[3].parse().unwrap();
            stop_canton.push(if c < 0 { NO_CANTON } else { c as u8 });
            stop_xy.push((r[4].parse().unwrap_or(0.0), r[5].parse().unwrap_or(0.0)));
            stop_change.push(r[6].parse().unwrap_or(120));
            let f = |k: usize| r.get(k).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            stop_ll.push((f(8), f(9)));
        }

        let mut trips = vec![];
        for (i, r) in read_tsv(dir, "trips.tsv").into_iter().enumerate() {
            // tidx trip_id var day route_short_name route_desc route_type trip_short_name trip_headsign agency_name
            assert_eq!(r[0].parse::<usize>().unwrap(), i, "trips.tsv must be sorted by tidx");
            let g = |k: usize| r.get(k).cloned().unwrap_or_default();
            trips.push(TripInfo {
                trip_id: format!("{}{}", g(1), g(2)),
                route_short_name: g(4),
                route_desc: g(5),
                route_type: g(6).parse().unwrap_or(0),
                trip_short_name: g(7),
                headsign: g(8),
                agency: g(9),
            });
        }

        let mut trip_last_conn = vec![NONE; trips.len()];
        for (c, &t) in trip.iter().enumerate() {
            trip_last_conn[t as usize] = c as u32;
        }

        // cantons still ahead on each trip: scan connections backwards, per trip
        let mut ahead = vec![0u32; dep.len()];
        let mut acc = vec![0u32; trips.len()];
        for c in (0..dep.len()).rev() {
            let t = trip[c] as usize;
            if flags[c] & F_ALIGHT != 0 {
                let k = stop_canton[to[c] as usize];
                if k != NO_CANTON {
                    acc[t] |= 1 << k;
                }
            }
            ahead[c] = acc[t];
        }

        let mut stay_to = vec![NONE; trips.len()];
        let mut stay_stop = vec![NONE; trips.len()];
        let (sf, st, ss) = (
            read_u32(dir, "stay_from.bin"),
            read_u32(dir, "stay_to.bin"),
            read_u32(dir, "stay_stop.bin"),
        );
        for k in 0..sf.len() {
            stay_to[sf[k] as usize] = st[k];
            stay_stop[sf[k] as usize] = ss[k];
        }

        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("meta.json")).expect("meta.json")).unwrap();
        let canton_names = meta["cantons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();

        Timetable {
            dep,
            arr,
            from,
            to,
            trip,
            flags,
            dwell,
            trip_last_conn,
            ahead,
            stop_id,
            stop_name,
            stop_canton,
            stop_change,
            stop_xy,
            stop_ll,
            foot_off: read_u32(dir, "foot_offsets.bin"),
            foot_to: read_u32(dir, "foot_to.bin"),
            foot_kind: fs::read(dir.join("foot_kind.bin")).unwrap_or_default(),
            walk_detour: meta["args"]["walk_detour"].as_f64().unwrap_or(1.3),
            bike_detour: meta["args"]["bike_detour"].as_f64().unwrap_or(1.3),
            foot_dur: read_u32(dir, "foot_dur.bin"),
            stay_to,
            stay_stop,
            trips,
            canton_names,
            date: meta["date"].as_str().unwrap_or("").to_owned(),
        }
    }

    #[inline]
    pub fn n_conns(&self) -> usize {
        self.dep.len()
    }

    #[inline]
    pub fn n_stops(&self) -> usize {
        self.stop_id.len()
    }

    /// Bit of the canton a stop lies in (0 if abroad).
    #[inline]
    pub fn bit(&self, stop: u32) -> u32 {
        let c = self.stop_canton[stop as usize];
        if c == NO_CANTON {
            0
        } else {
            1u32 << c
        }
    }

    /// Walk / bike links from a stop: (to, duration [s], kind: 0 = walk, 1 = bike).
    #[inline]
    pub fn footpaths(&self, stop: u32) -> impl Iterator<Item = (u32, u32, u8)> + '_ {
        let a = self.foot_off[stop as usize] as usize;
        let b = self.foot_off[stop as usize + 1] as usize;
        (a..b).map(move |k| (self.foot_to[k], self.foot_dur[k], self.foot_kind.get(k).copied().unwrap_or(0)))
    }

    /// Kind of the link a -> b (0 = walk, 1 = bike).
    pub fn link_kind(&self, a: u32, b: u32) -> u8 {
        self.footpaths(a).find(|&(t, _, _)| t == b).map_or(0, |(_, _, k)| k)
    }

    /// Estimated road distance [km] of the link a -> b.
    pub fn link_km(&self, a: u32, b: u32) -> f64 {
        let (p, q) = (self.stop_xy[a as usize], self.stop_xy[b as usize]);
        let d = ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt() / 1000.0;
        d * if self.link_kind(a, b) == 1 { self.bike_detour } else { self.walk_detour }
    }

    /// All stops (platforms) whose name matches exactly (case-insensitive).
    pub fn stops_named(&self, name: &str) -> Vec<u32> {
        let n = name.to_lowercase();
        (0..self.n_stops() as u32)
            .filter(|&s| self.stop_name[s as usize].to_lowercase() == n)
            .collect()
    }

    pub fn canton_name(&self, stop: u32) -> &str {
        let c = self.stop_canton[stop as usize];
        if c == NO_CANTON {
            "abroad"
        } else {
            &self.canton_names[c as usize]
        }
    }

    /// Human-readable vehicle label, e.g. "IC 1", "S 10", "Bus 31".
    pub fn vehicle(&self, trip: u32) -> String {
        let t = &self.trips[trip as usize];
        let kind = match t.route_type {
            100..=117 => "",
            400..=405 => "Metro",
            700..=716 | 200..=209 => "Bus",
            900..=906 => "Tram",
            1000..=1021 => "Boat",
            1300..=1307 => "Cable car",
            1400 => "Funicular",
            _ => "",
        };
        let desc = if kind.is_empty() { t.route_desc.as_str() } else { kind };
        let mut s = String::new();
        if !desc.is_empty() {
            s.push_str(desc);
        }
        let num = &t.route_short_name;
        if !desc.is_empty() && num.starts_with(desc) {
            return num.clone(); // "IC" + "IC51" -> "IC51"
        }
        if !num.is_empty() && num != desc {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(num);
        }
        if s.is_empty() {
            s = t.trip_short_name.clone();
        }
        s
    }
}
