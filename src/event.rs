//! VEVENT <-> JSON event model, recurrence expansion and builders.
use crate::error::{AppError, Result};
use crate::ical::{self, Component, Prop, escape};
use crate::timeexpr;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use serde_json::{Value, json};

/// A point in time as written in iCalendar.
#[derive(Debug, Clone, PartialEq)]
pub enum When {
    Date(Date),
    /// zoned instant (from TZID or Z), keeps its original tz name if any
    Zoned(Zoned, Option<String>),
    /// floating local time
    Floating(DateTime),
}

impl When {
    pub fn parse(p: &Prop, fallback: &TimeZone) -> Option<When> {
        let v = p.value.trim();
        if p.param("VALUE")
            .map(|x| x.eq_ignore_ascii_case("DATE"))
            .unwrap_or(false)
            || v.len() == 8
        {
            return Date::strptime("%Y%m%d", v).ok().map(When::Date);
        }
        if let Some(utc) = v.strip_suffix('Z') {
            let dt = DateTime::strptime("%Y%m%dT%H%M%S", utc).ok()?;
            return dt
                .to_zoned(TimeZone::UTC)
                .ok()
                .map(|z| When::Zoned(z, None));
        }
        let dt = DateTime::strptime("%Y%m%dT%H%M%S", v).ok()?;
        match p.param("TZID") {
            Some(tzid) => {
                let tz = TimeZone::get(tzid.trim_start_matches('/'))
                    .unwrap_or_else(|_| fallback.clone());
                dt.to_zoned(tz)
                    .ok()
                    .map(|z| When::Zoned(z, Some(tzid.to_string())))
            }
            None => Some(When::Floating(dt)),
        }
    }
    pub fn is_date(&self) -> bool {
        matches!(self, When::Date(_))
    }
    /// Instant for overlap math; dates are midnight in `tz`.
    pub fn instant(&self, tz: &TimeZone) -> Timestamp {
        match self {
            When::Date(d) => d
                .to_zoned(tz.clone())
                .map(|z| z.timestamp())
                .unwrap_or(Timestamp::UNIX_EPOCH),
            When::Zoned(z, _) => z.timestamp(),
            When::Floating(dt) => dt
                .to_zoned(tz.clone())
                .map(|z| z.timestamp())
                .unwrap_or(Timestamp::UNIX_EPOCH),
        }
    }
    pub fn to_prop(&self, name: &str) -> Prop {
        match self {
            When::Date(d) => {
                Prop::new(name, d.strftime("%Y%m%d").to_string()).with_param("VALUE", "DATE")
            }
            When::Zoned(z, Some(tzid)) if TimeZone::get(tzid).is_ok() => {
                Prop::new(name, z.strftime("%Y%m%dT%H%M%S").to_string()).with_param("TZID", tzid)
            }
            When::Zoned(z, _) => Prop::new(
                name,
                z.with_time_zone(TimeZone::UTC)
                    .strftime("%Y%m%dT%H%M%SZ")
                    .to_string(),
            ),
            When::Floating(dt) => Prop::new(name, dt.strftime("%Y%m%dT%H%M%S").to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub uid: String,
    pub summary: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub status: Option<String>,
    pub transp: Option<String>,
    pub start: When,
    pub end: When,
    pub rrule: Option<String>,
    pub exdates: Vec<When>,
    pub recurrence_id: Option<When>,
    pub alarms: Vec<String>,
    pub attendees: Vec<Value>,
    pub organizer: Option<String>,
    pub categories: Vec<String>,
}

impl Event {
    pub fn from_vevent(c: &Component, tz: &TimeZone) -> Option<Event> {
        let start = When::parse(c.get("DTSTART")?, tz)?;
        let end = match c.get("DTEND").and_then(|p| When::parse(p, tz)) {
            Some(e) => e,
            None => {
                let dur = c
                    .get("DURATION")
                    .and_then(|p| timeexpr::parse_duration(&p.value).ok());
                add_secs(
                    &start,
                    dur.unwrap_or(if start.is_date() { 86400 } else { 0 }),
                    tz,
                )
            }
        };
        let mut exdates = Vec::new();
        for p in c.get_all("EXDATE") {
            for v in p.value.split(',') {
                let mut q = p.clone();
                q.value = v.to_string();
                if let Some(w) = When::parse(&q, tz) {
                    exdates.push(w);
                }
            }
        }
        Some(Event {
            uid: c.text("UID").unwrap_or_default(),
            summary: c.text("SUMMARY"),
            location: c.text("LOCATION"),
            description: c.text("DESCRIPTION"),
            url: c.text("URL"),
            status: c.text("STATUS"),
            transp: c.text("TRANSP"),
            start,
            end,
            rrule: c.get("RRULE").map(|p| p.value.clone()),
            exdates,
            recurrence_id: c.get("RECURRENCE-ID").and_then(|p| When::parse(p, tz)),
            alarms: c.children_named("VALARM").filter_map(|a| a.get("TRIGGER").map(|t| t.value.clone())).collect(),
            attendees: c
                .get_all("ATTENDEE")
                .map(|p| {
                    json!({
                        "email": p.value.trim_start_matches("mailto:").trim_start_matches("MAILTO:"),
                        "name": p.param("CN"),
                        "status": p.param("PARTSTAT"),
                    })
                })
                .collect(),
            organizer: c.get("ORGANIZER").map(|p| p.value.trim_start_matches("mailto:").to_string()),
            categories: c.get_all("CATEGORIES").flat_map(|p| p.value.split(',').map(ical::unescape)).collect(),
        })
    }

    pub fn duration_secs(&self, tz: &TimeZone) -> i64 {
        self.end.instant(tz).as_second() - self.start.instant(tz).as_second()
    }

    /// JSON representation; `start`/`end` override for expanded instances.
    pub fn to_json(&self, tz: &TimeZone, inst: Option<(&When, &When)>) -> Value {
        let (s, e) = inst.unwrap_or((&self.start, &self.end));
        let all_day = s.is_date();
        let fmt_when = |w: &When| -> Value {
            match w {
                When::Date(d) => json!(d.to_string()),
                _ => json!(timeexpr::fmt(&w.instant(tz).to_zoned(tz.clone()))),
            }
        };
        let mut v = json!({
            "uid": self.uid,
            "title": self.summary,
            "start": fmt_when(s),
            "end": fmt_when(e),
            "all_day": all_day,
            "location": self.location,
            "notes": self.description,
            "url": self.url,
            "status": self.status.as_deref().map(|x| x.to_ascii_lowercase()),
            "busy": self.transp.as_deref().map(|t| !t.eq_ignore_ascii_case("TRANSPARENT")).unwrap_or(true),
            "recurring": self.rrule.is_some() || self.recurrence_id.is_some() || inst.is_some(),
            "rrule": self.rrule,
            "alarms": self.alarms,
            "attendees": self.attendees,
            "organizer": self.organizer,
            "categories": self.categories,
        });
        if let Some(t) = &self.transp {
            v["transparency"] = json!(t.to_ascii_lowercase());
        }
        if inst.is_some() || self.recurrence_id.is_some() {
            v["recurrence_id"] = fmt_when(self.recurrence_id.as_ref().unwrap_or(s));
        }
        if all_day {
            // inclusive last day is friendlier for agents; `end` stays exclusive per RFC 5545
            if let When::Date(d) = e {
                v["last_day"] = json!(d.yesterday().map(|x| x.to_string()).unwrap_or_default());
            }
        }
        v
    }
}

pub fn add_secs(w: &When, secs: i64, tz: &TimeZone) -> When {
    match w {
        When::Date(d) => When::Date(d.checked_add((secs / 86400).days()).unwrap_or(*d)),
        When::Zoned(z, n) => When::Zoned(
            z.checked_add(secs.seconds()).unwrap_or(z.clone()),
            n.clone(),
        ),
        When::Floating(dt) => {
            let z = dt.to_zoned(tz.clone()).unwrap();
            When::Floating(z.checked_add(secs.seconds()).unwrap().datetime())
        }
    }
}

/// One occurrence of an event within a window.
pub struct Occurrence<'a> {
    pub event: &'a Event,
    pub start: When,
    pub end: When,
    pub expanded: bool,
}

fn rrule_dt(w: &When) -> String {
    match w {
        When::Date(d) => format!(":{}T000000Z", d.strftime("%Y%m%d")),
        When::Zoned(z, Some(_)) if chrono_tz_ok(z.time_zone().iana_name()) => {
            format!(
                ";TZID={}:{}",
                z.time_zone().iana_name().unwrap(),
                z.strftime("%Y%m%dT%H%M%S")
            )
        }
        When::Zoned(z, _) => format!(
            ":{}",
            z.with_time_zone(TimeZone::UTC).strftime("%Y%m%dT%H%M%SZ")
        ),
        When::Floating(dt) => format!(":{}Z", dt.strftime("%Y%m%dT%H%M%S")),
    }
}
fn chrono_tz_ok(n: Option<&str>) -> bool {
    n.map(|n| n.parse::<chrono_tz::Tz>().is_ok())
        .unwrap_or(false)
}

fn chrono_to_when(d: &chrono::DateTime<rrule::Tz>, like: &When) -> When {
    let ts = Timestamp::from_second(d.timestamp()).unwrap();
    match like {
        When::Date(_) => When::Date(ts.to_zoned(TimeZone::UTC).date()),
        When::Zoned(z, n) => When::Zoned(ts.to_zoned(z.time_zone().clone()), n.clone()),
        When::Floating(_) => When::Floating(ts.to_zoned(TimeZone::UTC).datetime()),
    }
}

/// Expand masters + overrides of one resource into occurrences overlapping [from, to).
pub fn occurrences<'a>(
    events: &'a [Event],
    from: Timestamp,
    to: Timestamp,
    tz: &TimeZone,
    expand: bool,
) -> Vec<Occurrence<'a>> {
    let mut out = Vec::new();
    let overlaps = |s: &When, e: &When| {
        let (si, mut ei) = (s.instant(tz), e.instant(tz));
        if ei <= si {
            ei = si + jiff::SignedDuration::from_secs(1);
        }
        si < to && ei > from
    };
    let overrides: Vec<&Event> = events
        .iter()
        .filter(|e| e.recurrence_id.is_some())
        .collect();
    for ev in events.iter().filter(|e| e.recurrence_id.is_none()) {
        match (&ev.rrule, expand) {
            (Some(rule), true) => {
                let dur = ev.duration_secs(tz);
                let mut spec = format!("DTSTART{}\nRRULE:{}", rrule_dt(&ev.start), rule);
                if let When::Date(_) = ev.start {
                    // UNTIL must be UTC when DTSTART is UTC
                    if let Some(i) = spec.find("UNTIL=") {
                        let rest = &spec[i + 6..];
                        let val: String = rest
                            .chars()
                            .take_while(|c| *c != ';' && *c != '\n')
                            .collect();
                        if val.len() == 8 {
                            spec = spec.replacen(
                                &format!("UNTIL={val}"),
                                &format!("UNTIL={val}T235959Z"),
                                1,
                            );
                        }
                    }
                }
                for x in &ev.exdates {
                    spec.push_str(&format!("\nEXDATE{}", rrule_dt(x)));
                }
                let set: std::result::Result<rrule::RRuleSet, _> = spec.parse();
                let Ok(set) = set else {
                    // unparseable rule: fall back to the master instance
                    if overlaps(&ev.start, &ev.end) {
                        out.push(Occurrence {
                            event: ev,
                            start: ev.start.clone(),
                            end: ev.end.clone(),
                            expanded: false,
                        });
                    }
                    continue;
                };
                let lo = from - jiff::SignedDuration::from_secs(dur.max(0) + 1);
                let lo = chrono::DateTime::from_timestamp(lo.as_second(), 0)
                    .unwrap()
                    .with_timezone(&rrule::Tz::UTC);
                let hi = chrono::DateTime::from_timestamp(to.as_second(), 0)
                    .unwrap()
                    .with_timezone(&rrule::Tz::UTC);
                let res = set.after(lo).before(hi).all(2000);
                for d in res.dates {
                    let s = chrono_to_when(&d, &ev.start);
                    let si = s.instant(tz);
                    if overrides.iter().any(|o| {
                        o.uid == ev.uid
                            && o.recurrence_id.as_ref().map(|r| r.instant(tz)) == Some(si)
                    }) {
                        continue;
                    }
                    let e = add_secs(&s, dur, tz);
                    if overlaps(&s, &e) {
                        out.push(Occurrence {
                            event: ev,
                            start: s,
                            end: e,
                            expanded: true,
                        });
                    }
                }
            }
            _ => {
                if overlaps(&ev.start, &ev.end) || ev.rrule.is_some() {
                    out.push(Occurrence {
                        event: ev,
                        start: ev.start.clone(),
                        end: ev.end.clone(),
                        expanded: false,
                    });
                }
            }
        }
    }
    for o in overrides {
        if overlaps(&o.start, &o.end) {
            out.push(Occurrence {
                event: o,
                start: o.start.clone(),
                end: o.end.clone(),
                expanded: false,
            });
        }
    }
    out
}

/// Parse a calendar object resource into events.
pub fn parse_resource(ics: &str, tz: &TimeZone) -> Result<(Component, Vec<Event>)> {
    let tops = ical::parse(ics)
        .map_err(|e| AppError::new("E_UPSTREAM", format!("invalid iCalendar data: {e}")))?;
    let cal = tops
        .into_iter()
        .find(|c| c.name == "VCALENDAR")
        .ok_or_else(|| AppError::new("E_UPSTREAM", "no VCALENDAR in resource"))?;
    let evs = cal
        .children_named("VEVENT")
        .filter_map(|c| Event::from_vevent(c, tz))
        .collect();
    Ok((cal, evs))
}

/// Fields an agent can set on create/update.
#[derive(Debug, Default, Clone)]
pub struct EventPatch {
    pub title: Option<String>,
    pub start: Option<When>,
    pub end: Option<When>,
    pub location: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub url: Option<Option<String>>,
    pub rrule: Option<Option<String>>,
    pub alarms: Option<Vec<i64>>,
    pub busy: Option<bool>,
}

fn set_opt_text(c: &mut Component, name: &str, v: &Option<Option<String>>, raw: bool) {
    match v {
        None => {}
        Some(None) => c.remove(name),
        Some(Some(s)) if s.is_empty() => c.remove(name),
        Some(Some(s)) => c.set(Prop::new(name, if raw { s.clone() } else { escape(s) })),
    }
}

pub fn apply_patch(ev: &mut Component, p: &EventPatch, now: &Timestamp) {
    if let Some(t) = &p.title {
        ev.set(Prop::new("SUMMARY", escape(t)));
    }
    if let Some(s) = &p.start {
        ev.set(s.to_prop("DTSTART"));
    }
    if let Some(e) = &p.end {
        ev.remove("DURATION");
        ev.set(e.to_prop("DTEND"));
    }
    set_opt_text(ev, "LOCATION", &p.location, false);
    set_opt_text(ev, "DESCRIPTION", &p.notes, false);
    set_opt_text(ev, "URL", &p.url, true);
    set_opt_text(ev, "RRULE", &p.rrule, true);
    if let Some(b) = p.busy {
        ev.set(Prop::new(
            "TRANSP",
            if b { "OPAQUE" } else { "TRANSPARENT" },
        ));
    }
    if let Some(alarms) = &p.alarms {
        ev.children.retain(|c| c.name != "VALARM");
        for a in alarms {
            let mut al = Component::new("VALARM");
            al.push(Prop::new("ACTION", "DISPLAY"));
            al.push(Prop::new("DESCRIPTION", "Reminder"));
            al.push(Prop::new("TRIGGER", timeexpr::ical_duration(*a)));
            ev.children.push(al);
        }
    }
    let stamp = now
        .to_zoned(TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    ev.set(Prop::new("DTSTAMP", stamp.clone()));
    ev.set(Prop::new("LAST-MODIFIED", stamp));
}

pub fn new_calendar(uid: &str, p: &EventPatch, now: &Timestamp) -> Component {
    let mut cal = Component::new("VCALENDAR");
    cal.push(Prop::new("VERSION", "2.0"));
    cal.push(Prop::new(
        "PRODID",
        format!("-//icloud-cli//{}//EN", env!("CARGO_PKG_VERSION")),
    ));
    cal.push(Prop::new("CALSCALE", "GREGORIAN"));
    let mut ev = Component::new("VEVENT");
    ev.push(Prop::new("UID", uid));
    let stamp = now
        .to_zoned(TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    ev.push(Prop::new("CREATED", stamp));
    ev.push(Prop::new("SEQUENCE", "0"));
    apply_patch(&mut ev, p, now);
    cal.children.push(ev);
    cal
}

pub fn bump_sequence(ev: &mut Component) {
    let n = ev
        .get("SEQUENCE")
        .and_then(|p| p.value.trim().parse::<i64>().ok())
        .unwrap_or(0);
    ev.set(Prop::new("SEQUENCE", (n + 1).to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:r1\r\nSUMMARY:Standup\r\nDTSTART;TZID=Asia/Shanghai:20261012T100000\r\nDTEND;TZID=Asia/Shanghai:20261012T101500\r\nRRULE:FREQ=DAILY;COUNT=10\r\nEXDATE;TZID=Asia/Shanghai:20261014T100000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:r1\r\nRECURRENCE-ID;TZID=Asia/Shanghai:20261015T100000\r\nSUMMARY:Standup (moved)\r\nDTSTART;TZID=Asia/Shanghai:20261015T110000\r\nDTEND;TZID=Asia/Shanghai:20261015T111500\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn expands_with_exdate_and_override() {
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let (_, evs) = parse_resource(ICS, &tz).unwrap();
        let from: Timestamp = "2026-10-13T00:00:00+08:00".parse().unwrap();
        let to: Timestamp = "2026-10-17T00:00:00+08:00".parse().unwrap();
        let occ = occurrences(&evs, from, to, &tz, true);
        let mut starts: Vec<String> = occ
            .iter()
            .map(|o| {
                o.event.to_json(&tz, Some((&o.start, &o.end)))["start"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        starts.sort();
        assert_eq!(
            starts,
            vec![
                "2026-10-13T10:00:00+08:00",
                "2026-10-15T11:00:00+08:00",
                "2026-10-16T10:00:00+08:00"
            ]
        );
    }

    #[test]
    fn all_day_event() {
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Trip\r\nDTSTART;VALUE=DATE:20261016\r\nDTEND;VALUE=DATE:20261018\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (_, evs) = parse_resource(ics, &tz).unwrap();
        let j = evs[0].to_json(&tz, None);
        assert_eq!(j["all_day"], true);
        assert_eq!(j["start"], "2026-10-16");
        assert_eq!(j["last_day"], "2026-10-17");
    }

    #[test]
    fn build_and_patch() {
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let now = Timestamp::now();
        let s = When::Zoned(
            "2026-10-16T15:00:00+08:00[Asia/Shanghai]".parse().unwrap(),
            None,
        );
        let e = add_secs(&s, 3600, &tz);
        let p = EventPatch {
            title: Some("和老王吃饭, 望京".into()),
            start: Some(s),
            end: Some(e),
            alarms: Some(vec![-900]),
            ..Default::default()
        };
        let cal = new_calendar("u1", &p, &now);
        let ics = cal.serialize();
        assert!(ics.contains("DTSTART:20261016T070000Z"));
        assert!(ics.contains("TRIGGER:-PT15M"));
        let (_, evs) = parse_resource(&ics, &tz).unwrap();
        assert_eq!(evs[0].summary.as_deref(), Some("和老王吃饭, 望京"));
        assert_eq!(evs[0].duration_secs(&tz), 3600);
    }
}
