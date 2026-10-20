// A small ICS reader: VEVENT with DTSTART/DTEND/DURATION, TZID or UTC, EXDATE, RECURRENCE-ID and
// RRULE of DAILY, WEEKLY or MONTHLY (by day or month day) with INTERVAL, COUNT and UNTIL.
// Any other rule is kept as a single occurrence flagged "unsupported rule", never guessed.

use jiff::{
    civil::{Date, DateTime, Weekday},
    tz::TimeZone,
    Span,
};

/// A wall-clock time with the zone it was written in (`None` = floating, read in local time).
#[derive(Clone)]
pub struct Wall {
    dt: DateTime,
    tz: Option<TimeZone>,
    all_day: bool,
}

impl Wall {
    fn zone(&self, local: &TimeZone) -> TimeZone {
        self.tz.clone().unwrap_or_else(|| local.clone())
    }
    fn ts(&self, local: &TimeZone) -> Option<i64> {
        Some(self.dt.to_zoned(self.zone(local)).ok()?.timestamp().as_second())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Clone)]
pub struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<Wall>,
    byday: Vec<(i8, Weekday)>,
    bymonthday: Vec<i8>,
}

pub struct Event {
    pub uid: String,
    pub title: String,
    pub location: String,
    pub description: String,
    pub link: Option<String>,
    start: Wall,
    end: Option<Wall>,
    dur: i64,
    pub rule: Option<Result<Rule, String>>,
    exdates: Vec<Wall>,
    recurrence_id: Option<Wall>,
}

/// One occurrence, in Unix seconds.
#[derive(Clone)]
pub struct Occ {
    pub title: String,
    pub location: String,
    pub description: String,
    pub link: Option<String>,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// Why the event repeats in a way this reader does not expand.
    pub unsupported: Option<String>,
}

type Prop = (String, Vec<(String, String)>, String);

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_wall(v: &str, tzid: Option<&str>) -> Option<Wall> {
    let v = v.trim();
    let num = |a: usize, b: usize| -> Option<i32> { v.get(a..b)?.parse().ok() };
    let date = Date::new(num(0, 4)? as i16, num(4, 6)? as i8, num(6, 8)? as i8).ok()?;
    if v.len() == 8 {
        return Some(Wall { dt: date.at(0, 0, 0, 0), tz: None, all_day: true });
    }
    if v.as_bytes().get(8) != Some(&b'T') {
        return None;
    }
    let dt = date.at(num(9, 11)? as i8, num(11, 13)? as i8, num(13, 15).unwrap_or(0).min(59) as i8, 0);
    // debt: a TZID this reader does not know (Windows zone names) is read as local time
    let tz = if v.ends_with('Z') { Some(TimeZone::UTC) } else { tzid.and_then(|t| TimeZone::get(t.trim_matches('"')).ok()) };
    Some(Wall { dt, tz, all_day: false })
}

fn parse_duration(v: &str) -> Option<i64> {
    let (mut total, mut n, mut time) = (0i64, 0i64, false);
    for c in v.trim_start_matches(['+', '-']).chars().skip(1) {
        match c {
            'T' => time = true,
            '0'..='9' => n = n * 10 + c as i64 - '0' as i64,
            'W' => (total, n) = (total + n * 604800, 0),
            'D' => (total, n) = (total + n * 86400, 0),
            'H' if time => (total, n) = (total + n * 3600, 0),
            'M' if time => (total, n) = (total + n * 60, 0),
            'S' if time => (total, n) = (total + n, 0),
            _ => return None,
        }
    }
    Some(total)
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "MO" => Weekday::Monday,
        "TU" => Weekday::Tuesday,
        "WE" => Weekday::Wednesday,
        "TH" => Weekday::Thursday,
        "FR" => Weekday::Friday,
        "SA" => Weekday::Saturday,
        "SU" => Weekday::Sunday,
        _ => return None,
    })
}

fn parse_rule(v: &str, tzid: Option<&str>) -> Result<Rule, String> {
    let mut r = Rule { freq: Freq::Daily, interval: 1, count: None, until: None, byday: vec![], bymonthday: vec![] };
    let mut freq = None;
    for part in v.split(';') {
        let (k, val) = part.split_once('=').unwrap_or((part, ""));
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match val {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    other => return Err(format!("FREQ={other}")),
                })
            }
            "INTERVAL" => r.interval = val.parse::<i64>().ok().filter(|n| *n > 0).ok_or("INTERVAL")?,
            "COUNT" => r.count = Some(val.parse().map_err(|_| "COUNT")?),
            "UNTIL" => r.until = Some(parse_wall(val, tzid).ok_or("UNTIL")?),
            "WKST" => {}
            "BYDAY" => {
                for d in val.split(',') {
                    let split = d.len().saturating_sub(2);
                    let ord = match d.get(..split) {
                        Some("") => 0,
                        Some(o) => o.trim_start_matches('+').parse::<i8>().map_err(|_| "BYDAY")?,
                        None => return Err("BYDAY".into()),
                    };
                    r.byday.push((ord, weekday(&d[split..]).ok_or("BYDAY")?));
                }
            }
            "BYMONTHDAY" => {
                for d in val.split(',') {
                    r.bymonthday.push(d.parse::<i8>().ok().filter(|n| *n != 0 && n.abs() <= 31).ok_or("BYMONTHDAY")?);
                }
            }
            other => return Err(other.to_string()),
        }
    }
    r.freq = freq.ok_or("FREQ")?;
    if r.byday.iter().any(|(o, _)| *o != 0) && r.freq != Freq::Monthly {
        return Err("BYDAY with a number".into());
    }
    if r.freq == Freq::Daily && !(r.byday.is_empty() && r.bymonthday.is_empty()) {
        return Err("DAILY with BYDAY".into());
    }
    if r.freq != Freq::Monthly && !r.bymonthday.is_empty() {
        return Err("BYMONTHDAY".into());
    }
    Ok(r)
}

fn build(props: &[Prop]) -> Option<Event> {
    let get = |n: &str| props.iter().find(|p| p.0 == n);
    let tzid = |p: &Prop| p.1.iter().find(|(k, _)| k == "TZID").map(|(_, v)| v.clone());
    if get("STATUS").is_some_and(|p| p.2.eq_ignore_ascii_case("CANCELLED")) {
        return None;
    }
    let ds = get("DTSTART")?;
    let ds_tz = tzid(ds);
    let start = parse_wall(&ds.2, ds_tz.as_deref())?;
    let end = get("DTEND").and_then(|p| parse_wall(&p.2, tzid(p).or(ds_tz.clone()).as_deref()));
    let utc = TimeZone::UTC;
    let dur = match (&end, get("DURATION")) {
        (Some(e), _) => e.ts(&utc).zip(start.ts(&utc)).map_or(0, |(e, s)| (e - s).max(0)),
        (None, Some(p)) => parse_duration(&p.2).unwrap_or(0),
        _ => 0,
    };
    let rule = get("RRULE").map(|p| parse_rule(&p.2, ds_tz.as_deref()));
    let mut exdates = vec![];
    for p in props.iter().filter(|p| p.0 == "EXDATE") {
        let z = tzid(p).or(ds_tz.clone());
        exdates.extend(p.2.split(',').filter_map(|v| parse_wall(v, z.as_deref())));
    }
    let text = |n: &str| get(n).map(|p| unescape(&p.2)).unwrap_or_default();
    let (location, description, url) = (text("LOCATION"), text("DESCRIPTION"), text("URL"));
    Some(Event {
        link: meeting_link(&[&location, &description, &url]),
        // debt: cut at 1500 characters; a feed's description can be a page long.
        description: description.trim().chars().take(1500).collect(),
        uid: get("UID").map(|p| p.2.clone()).unwrap_or_default(),
        title: get("SUMMARY").map(|p| unescape(&p.2)).filter(|s| !s.is_empty()).unwrap_or_else(|| "(no title)".into()),
        location,
        recurrence_id: get("RECURRENCE-ID").and_then(|p| parse_wall(&p.2, tzid(p).or(ds_tz.clone()).as_deref())),
        start,
        end,
        dur,
        rule,
        exdates,
    })
}

pub fn parse(text: &str) -> Vec<Event> {
    let mut lines: Vec<String> = vec![];
    for l in text.lines() {
        match lines.last_mut() {
            Some(prev) if l.starts_with([' ', '\t']) => prev.push_str(&l[1..]),
            _ => lines.push(l.to_string()),
        }
    }
    let (mut out, mut cur, mut alarm): (Vec<Event>, Option<Vec<Prop>>, bool) = (vec![], None, false);
    for l in &lines {
        let Some((head, value)) = l.split_once(':') else { continue };
        let mut parts = head.split(';');
        let name = parts.next().unwrap_or("").to_ascii_uppercase();
        let params: Vec<(String, String)> =
            parts.filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect();
        match (name.as_str(), value.trim()) {
            ("BEGIN", "VEVENT") => cur = Some(vec![]),
            ("BEGIN", "VALARM") => alarm = true,
            ("END", "VALARM") => alarm = false,
            ("END", "VEVENT") => {
                if let Some(e) = cur.take().and_then(|p| build(&p)) {
                    out.push(e);
                }
            }
            _ => {
                if let (Some(p), false) = (cur.as_mut(), alarm) {
                    p.push((name, params, value.to_string()));
                }
            }
        }
    }
    // An edited single instance replaces that instance of its series.
    let moved: Vec<(String, Wall)> = out.iter().filter_map(|e| Some((e.uid.clone(), e.recurrence_id.clone()?))).collect();
    for e in out.iter_mut().filter(|e| e.recurrence_id.is_none()) {
        e.exdates.extend(moved.iter().filter(|(u, _)| *u == e.uid).map(|(_, w)| w.clone()));
    }
    out
}

fn each_date(r: &Rule, s: Date, f: &mut dyn FnMut(Date) -> bool) {
    let add = |d: Date, n: i64| d.checked_add(Span::new().days(n)).ok();
    match r.freq {
        Freq::Daily => {
            for k in 0..50_000i64 {
                match add(s, k * r.interval) {
                    Some(d) if f(d) => {}
                    _ => return,
                }
            }
        }
        Freq::Weekly => {
            let Some(w0) = add(s, -(s.weekday().to_monday_zero_offset() as i64)) else { return };
            let mut days: Vec<i64> = if r.byday.is_empty() {
                vec![s.weekday().to_monday_zero_offset() as i64]
            } else {
                r.byday.iter().map(|(_, w)| w.to_monday_zero_offset() as i64).collect()
            };
            days.sort();
            days.dedup();
            // debt: weeks start on Monday whatever WKST says
            for k in 0..10_000i64 {
                for &o in &days {
                    let Some(d) = add(w0, k * 7 * r.interval + o) else { return };
                    if d >= s && !f(d) {
                        return;
                    }
                }
            }
        }
        Freq::Monthly => {
            for k in 0..2_400i64 {
                let m0 = s.year() as i64 * 12 + (s.month() as i64 - 1) + k * r.interval;
                let (y, m) = ((m0 / 12) as i16, (m0 % 12 + 1) as i8);
                let Ok(first) = Date::new(y, m, 1) else { return };
                let mut c: Vec<Date> = vec![];
                if !r.byday.is_empty() {
                    for &(ord, wd) in &r.byday {
                        if ord == 0 {
                            c.extend((1..=first.days_in_month()).filter_map(|d| Date::new(y, m, d).ok()).filter(|d| d.weekday() == wd));
                        } else if let Ok(d) = first.nth_weekday_of_month(ord, wd) {
                            c.push(d);
                        }
                    }
                } else if !r.bymonthday.is_empty() {
                    for &md in &r.bymonthday {
                        let dd = if md > 0 { md } else { first.days_in_month() + md + 1 };
                        c.extend(Date::new(y, m, dd).ok());
                    }
                } else {
                    c.extend(Date::new(y, m, s.day()).ok());
                }
                c.sort();
                c.dedup();
                for d in c {
                    if d >= s && !f(d) {
                        return;
                    }
                }
            }
        }
    }
}

impl Event {
    /// The occurrences that overlap `[from, to)` (Unix seconds); floating times are read in `local`.
    pub fn expand(&self, local: &TimeZone, from: i64, to: i64) -> Vec<Occ> {
        let zone = self.start.zone(local);
        let (time, sd) = (self.start.dt.time(), self.start.dt.date());
        let days = match (&self.end, self.start.all_day) {
            (Some(e), true) => (e.dt.date() - sd).get_days().max(1) as i64,
            _ => 1,
        };
        let span = |d: Date| -> Option<(i64, i64)> {
            let s = d.to_datetime(time).to_zoned(zone.clone()).ok()?.timestamp().as_second();
            if self.start.all_day {
                let e = d.checked_add(Span::new().days(days)).ok()?.at(0, 0, 0, 0).to_zoned(zone.clone()).ok()?;
                Some((s, e.timestamp().as_second()))
            } else {
                Some((s, s + self.dur))
            }
        };
        let occ = |(s, e): (i64, i64), unsupported: Option<String>| Occ {
            title: self.title.clone(),
            location: self.location.clone(),
            description: self.description.clone(),
            link: self.link.clone(),
            start: s,
            end: e,
            all_day: self.start.all_day,
            unsupported,
        };
        let inside = |s: i64, e: i64| s < to && (e > from || s >= from);
        let rule = match &self.rule {
            None => None,
            Some(Err(why)) => {
                return span(sd).filter(|&(s, e)| inside(s, e)).map(|r| occ(r, Some(why.clone()))).into_iter().collect();
            }
            Some(Ok(r)) => Some(r),
        };
        let Some(rule) = rule else {
            return span(sd).filter(|&(s, e)| inside(s, e)).map(|r| occ(r, None)).into_iter().collect();
        };
        let ex: Vec<i64> = self.exdates.iter().filter_map(|w| w.ts(&zone)).collect();
        // A date-only UNTIL ends at the end of that day; a floating one is read in the event's zone.
        let until = rule.until.as_ref().and_then(|u| {
            let t = if u.tz.is_some() { u.ts(local) } else { u.dt.to_zoned(zone.clone()).ok().map(|z| z.timestamp().as_second()) };
            t.map(|t| if u.all_day { t + 86399 } else { t })
        });
        let (mut n, mut out) = (0u32, vec![]);
        each_date(rule, sd, &mut |d| {
            n += 1;
            if rule.count.is_some_and(|c| n > c) {
                return false;
            }
            let Some((s, e)) = span(d) else { return true };
            if until.is_some_and(|u| s > u) || s >= to {
                return false;
            }
            if !ex.contains(&s) && inside(s, e) {
                out.push(occ((s, e), None));
            }
            true
        });
        out
    }
}

/// The timed occurrence that starts within the hour after `now` (the countdown rule; the page applies the same).
pub fn countdown(occs: &[Occ], now: i64) -> Option<&Occ> {
    occs.iter().filter(|o| !o.all_day && o.start > now && o.start - now <= 3600).min_by_key(|o| o.start)
}

/// The first link to a Meet, Zoom, Teams or Webex meeting in these texts (location, description, URL).
/// Only `https`, only the services' own hosts; a look-alike host or one with a user name is refused.
pub fn meeting_link(texts: &[&str]) -> Option<String> {
    let ok = |host: &str| {
        let under = |d: &str| host == d || host.ends_with(&format!(".{d}"));
        host == "meet.google.com" || under("zoom.us") || host == "teams.microsoft.com" || host == "teams.live.com" || under("webex.com")
    };
    for t in texts {
        let low = t.to_ascii_lowercase();
        for (i, _) in low.match_indices("https://") {
            let url: String = t[i..].chars().take_while(|c| !c.is_whitespace() && !"<>\"'\\)]".contains(*c)).collect();
            let rest = &url[8..];
            let (auth, path) = rest.find(['/', '?', '#']).map_or((rest, ""), |k| rest.split_at(k));
            let host = auth.to_ascii_lowercase();
            let host = host.strip_suffix(":443").unwrap_or(&host);
            if path.len() > 1 && !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') && ok(host) {
                return Some(url);
            }
        }
    }
    None
}

/// An event as the strip sees it (Unix seconds).
#[derive(Clone)]
pub struct Item {
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub link: Option<String>,
}

/// Events that start together; `in_progress` when the start has passed.
pub struct Card {
    pub start: i64,
    pub in_progress: bool,
    pub items: Vec<Item>,
}

/// What the strip shows at `now` with a lead of `lead` seconds: meetings in progress (an event with a link,
/// started and not ended) beat upcoming events (start within the lead); events with one start make one card.
pub fn cards(items: &[Item], now: i64, lead: i64) -> Vec<Card> {
    let live = |i: &&Item| i.link.is_some() && i.start <= now && i.end > now;
    let mut pool: Vec<&Item> = items.iter().filter(live).collect();
    let in_progress = !pool.is_empty();
    if !in_progress {
        pool = items.iter().filter(|i| i.start > now && i.start - now <= lead).collect();
    }
    pool.sort_by_key(|i| i.start);
    let mut out: Vec<Card> = vec![];
    for i in pool {
        match out.last_mut() {
            Some(c) if c.start == i.start => c.items.push(i.clone()),
            _ => out.push(Card { start: i.start, in_progress, items: vec![i.clone()] }),
        }
    }
    out
}
