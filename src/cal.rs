//! `icloud cal ...` — CalDAV calendar read/write.
use crate::ctx::Ctx;
use crate::dav::{self, Dav, DavResponse, xml_escape};
use crate::error::{AppError, Result};
use crate::event::{self, EventPatch, When, add_secs};
use crate::output::{Out, paginate};
use crate::timeexpr;
use clap::{Args, Subcommand};
use jiff::{Timestamp, Zoned};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Subcommand, Debug)]
pub enum CalCmd {
    /// Manage calendars (collections)
    #[command(subcommand)]
    Calendars(CalendarsCmd),
    /// Read and write events
    #[command(subcommand)]
    Events(EventsCmd),
    /// Compute free time slots
    Free(FreeArgs),
}

#[derive(Subcommand, Debug)]
pub enum CalendarsCmd {
    /// List calendars that hold events (reminder lists are excluded)
    List,
    /// Create a calendar
    Create {
        #[arg(long)]
        name: String,
        /// Color like #FF2968
        #[arg(long)]
        color: Option<String>,
    },
    /// Delete a calendar and ALL its events (needs --yes)
    Delete {
        /// Calendar id or name
        calendar: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct EventFields {
    #[arg(long)]
    pub title: Option<String>,
    /// Start (ISO 8601 or relative, e.g. 2026-10-16T15:00, tomorrow, +1d)
    #[arg(long, allow_hyphen_values = true)]
    pub start: Option<String>,
    /// End (exclusive). Alternative: --duration
    #[arg(long, allow_hyphen_values = true)]
    pub end: Option<String>,
    /// Duration like 1h, 30m, 1h30m (default 1h; all-day: 1d)
    #[arg(long)]
    pub duration: Option<String>,
    /// All-day event (start/end are dates)
    #[arg(long)]
    pub all_day: bool,
    /// Location ("" clears on update)
    #[arg(long)]
    pub location: Option<String>,
    /// Notes / description ("" clears on update)
    #[arg(long)]
    pub notes: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    /// Alarm offset relative to start, repeatable: -15m, -1d. Use --no-alarms to clear.
    #[arg(long = "alarm", allow_hyphen_values = true)]
    pub alarms: Vec<String>,
    #[arg(long)]
    pub no_alarms: bool,
    /// RFC 5545 RRULE, e.g. FREQ=WEEKLY;BYDAY=MO ("" clears on update)
    #[arg(long)]
    pub rrule: Option<String>,
    /// Mark as free (transparent) instead of busy
    #[arg(long)]
    pub free: bool,
}

#[derive(Subcommand, Debug)]
pub enum EventsCmd {
    /// List events in a time window (recurring events expanded)
    List {
        #[arg(long, default_value = "today", allow_hyphen_values = true)]
        from: String,
        #[arg(long, default_value = "+7d", allow_hyphen_values = true)]
        to: String,
        /// Calendar id or name (repeatable; default all)
        #[arg(long = "calendar")]
        calendars: Vec<String>,
        /// Return master events with RRULE instead of expanded instances
        #[arg(long)]
        no_expand: bool,
    },
    /// Get one event by id
    Get {
        id: String,
        /// Include raw iCalendar text
        #[arg(long)]
        raw: bool,
    },
    /// Create an event
    Create {
        #[arg(long)]
        calendar: Option<String>,
        #[command(flatten)]
        f: EventFields,
        /// Makes retries safe: same key => same UID => no duplicate
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Update fields of an event (whole series for recurring events)
    Update {
        id: String,
        #[command(flatten)]
        f: EventFields,
        /// Fail with E_CONFLICT if the event's etag differs
        #[arg(long)]
        if_match: Option<String>,
    },
    /// Delete an event (whole series). Needs --yes
    Delete {
        id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        if_match: Option<String>,
    },
    /// Incremental changes since a token (RFC 6578 sync-collection)
    Sync {
        #[arg(long = "calendar")]
        calendars: Vec<String>,
        /// Token from a previous sync; omit for a full initial listing
        #[arg(long)]
        since_token: Option<String>,
    },
    /// Search title/location/notes/attendees within a window
    Search {
        query: String,
        #[arg(long, default_value = "-90d", allow_hyphen_values = true)]
        from: String,
        #[arg(long, default_value = "+365d", allow_hyphen_values = true)]
        to: String,
        #[arg(long = "calendar")]
        calendars: Vec<String>,
    },
}

#[derive(Args, Debug)]
pub struct FreeArgs {
    #[arg(long, default_value = "now", allow_hyphen_values = true)]
    from: String,
    #[arg(long, default_value = "+7d", allow_hyphen_values = true)]
    to: String,
    /// Minimum slot length
    #[arg(long, default_value = "30m")]
    min: String,
    /// Daily working window, e.g. 09:00-18:00
    #[arg(long)]
    work_hours: Option<String>,
    /// Skip Saturday/Sunday
    #[arg(long)]
    weekdays_only: bool,
    /// Treat all-day events as busy
    #[arg(long)]
    all_day_busy: bool,
    #[arg(long = "calendar")]
    calendars: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CalInfo {
    pub id: String,
    pub name: String,
    pub url: Url,
    pub color: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub comps: Vec<String>,
}

impl CalInfo {
    fn json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "color": self.color, "components": self.comps, "ctag": self.ctag, "sync_token": self.sync_token })
    }
    fn has_events(&self) -> bool {
        self.comps.is_empty() || self.comps.iter().any(|c| c.eq_ignore_ascii_case("VEVENT"))
    }
}

pub struct Cal {
    pub dav: Dav,
    pub home: Url,
}

impl Cal {
    pub async fn connect(ctx: &Ctx) -> Result<Cal> {
        let (dav, base) = ctx.dav("caldav").await?;
        let home = if let Ok(h) = std::env::var("ICLOUD_CALDAV_HOME") {
            Url::parse(&h).map_err(|e| AppError::new("E_CONFIG", e.to_string()))?
        } else {
            dav::discover_home(&dav, &base, "calendar-home-set").await?
        };
        Ok(Cal { dav, home })
    }

    pub async fn calendars(&self) -> Result<Vec<CalInfo>> {
        let (u, rs) = self
            .dav
            .propfind(&self.home, "1", "<d:resourcetype/><d:displayname/><ic:calendar-color/><cs:getctag/><d:sync-token/><c:supported-calendar-component-set/>")
            .await?;
        let mut out = Vec::new();
        for r in rs {
            let is_cal = r
                .props
                .get("resourcetype")
                .map(|p| p.children.iter().any(|c| c == "calendar"))
                .unwrap_or(false);
            if !is_cal {
                continue;
            }
            let url = u
                .join(&r.href)
                .map_err(|e| AppError::new("E_UPSTREAM", e.to_string()))?;
            let id = dav::last_segment(&r.href);
            out.push(CalInfo {
                name: r
                    .text("displayname")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| id.clone()),
                id,
                url,
                color: r.text("calendar-color").map(|s| s.trim().to_string()),
                ctag: r.text("getctag").map(|s| s.trim().to_string()),
                sync_token: r.text("sync-token").map(|s| s.trim().to_string()),
                comps: r
                    .props
                    .get("supported-calendar-component-set")
                    .map(|p| p.comps.clone())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub async fn event_calendars(&self) -> Result<Vec<CalInfo>> {
        Ok(self
            .calendars()
            .await?
            .into_iter()
            .filter(|c| c.has_events())
            .collect())
    }

    async fn select(&self, wanted: &[String]) -> Result<Vec<CalInfo>> {
        let all = self.event_calendars().await?;
        if wanted.is_empty() {
            return Ok(all);
        }
        wanted.iter().map(|w| resolve_cal(&all, w)).collect()
    }
}

pub fn resolve_cal(all: &[CalInfo], w: &str) -> Result<CalInfo> {
    if let Some(c) = all.iter().find(|c| c.id == w) {
        return Ok(c.clone());
    }
    let by_name: Vec<&CalInfo> = all
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(w))
        .collect();
    match by_name.len() {
        1 => Ok(by_name[0].clone()),
        0 => Err(AppError::not_found(format!("no calendar with id or name `{w}`"))
            .hint("Run `icloud cal calendars list` to see ids and names.")
            .details(json!({ "available": all.iter().map(|c| json!({"id": c.id, "name": c.name})).collect::<Vec<_>>() }))),
        _ => Err(AppError::usage(format!("calendar name `{w}` is ambiguous")).hint("Pass the calendar id instead.")),
    }
}

fn utc_stamp(t: Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string()
}

fn split_id(id: &str) -> Result<(String, String)> {
    let id = id.strip_prefix("cal:").unwrap_or(id);
    match id.split_once('/') {
        Some((c, f)) if !c.is_empty() && !f.is_empty() => Ok((c.to_string(), f.to_string())),
        _ => Err(AppError::usage(format!("invalid event id `{id}`")).hint(
            "Event ids look like `<calendar-id>/<file>.ics`; take them from `cal events list`.",
        )),
    }
}

fn event_url(cal: &CalInfo, file: &str) -> Result<Url> {
    let enc =
        percent_encoding::utf8_percent_encode(file, percent_encoding::NON_ALPHANUMERIC).to_string();
    let enc = enc
        .replace("%2E", ".")
        .replace("%2D", "-")
        .replace("%5F", "_");
    cal.url
        .join(&enc)
        .map_err(|e| AppError::internal(e.to_string()))
}

struct Fetched {
    url: Url,
    etag: Option<String>,
    body: String,
}

async fn fetch(c: &Cal, url: &Url) -> Result<Fetched> {
    let r = c.dav.request("GET", url, &[], None).await?;
    c.dav.check(&r)?;
    let etag = r
        .headers
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Ok(Fetched {
        url: url.clone(),
        etag,
        body: r.body,
    })
}

fn rows_from_resources(
    ctx: &Ctx,
    cal: &CalInfo,
    rs: &[DavResponse],
    from: Timestamp,
    to: Timestamp,
    expand: bool,
    out: &mut Vec<(Timestamp, Value)>,
) {
    for r in rs {
        let Some(data) = r.text("calendar-data") else {
            continue;
        };
        let Ok((_, evs)) = event::parse_resource(data, &ctx.tz) else {
            continue;
        };
        let file = dav::last_segment(&r.href);
        for o in event::occurrences(&evs, from, to, &ctx.tz, expand) {
            let mut j = o.event.to_json(
                &ctx.tz,
                if o.expanded {
                    Some((&o.start, &o.end))
                } else {
                    None
                },
            );
            j["id"] = json!(format!("{}/{}", cal.id, file));
            j["calendar"] = json!(cal.name);
            j["calendar_id"] = json!(cal.id);
            j["etag"] = json!(r.text("getetag").map(|s| s.trim()));
            out.push((o.start.instant(&ctx.tz), j));
        }
    }
}

async fn query_window(
    ctx: &Ctx,
    c: &Cal,
    cals: &[CalInfo],
    from: Timestamp,
    to: Timestamp,
    expand: bool,
) -> Result<Vec<Value>> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{}" end="{}"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#,
        utc_stamp(from),
        utc_stamp(to)
    );
    let futs = cals.iter().map(|cal| {
        let body = body.clone();
        async move {
            c.dav
                .report(&cal.url, "1", body)
                .await
                .map(|rs| (cal.clone(), rs))
        }
    });
    let results = futures::future::join_all(futs).await;
    let mut rows = Vec::new();
    for r in results {
        let (cal, rs) = r?;
        rows_from_resources(ctx, &cal, &rs, from, to, expand, &mut rows);
    }
    rows.sort_by_key(|a| a.0);
    Ok(rows.into_iter().map(|(_, v)| v).collect())
}

fn window(ctx: &Ctx, from: &str, to: &str) -> Result<(Timestamp, Timestamp)> {
    let f = timeexpr::parse(from, &ctx.tz)?.timestamp();
    let t = timeexpr::parse(to, &ctx.tz)?.timestamp();
    if t <= f {
        return Err(AppError::usage(format!(
            "--to ({to}) must be after --from ({from})"
        )));
    }
    Ok((f, t))
}

/// `tzid`: write local time + TZID (needed for recurring events so DST is honoured); else UTC.
fn parse_when(ctx: &Ctx, s: &str, all_day: bool, tzid: bool) -> Result<When> {
    if all_day {
        let z = timeexpr::parse(s, &ctx.tz)?;
        Ok(When::Date(z.date()))
    } else {
        let name = if tzid {
            ctx.tz.iana_name().map(|s| s.to_string())
        } else {
            None
        };
        Ok(When::Zoned(timeexpr::parse(s, &ctx.tz)?, name))
    }
}

/// Build a patch from flags. `base_start`/`base_dur` come from the existing event on update.
fn build_patch(ctx: &Ctx, f: &EventFields, existing: Option<&event::Event>) -> Result<EventPatch> {
    let all_day = f.all_day || existing.map(|e| e.start.is_date()).unwrap_or(false);
    let mut p = EventPatch {
        title: f.title.clone(),
        ..Default::default()
    };
    let recurring = f.rrule.as_deref().map(|r| !r.is_empty()).unwrap_or(false)
        || existing.map(|e| e.rrule.is_some()).unwrap_or(false);
    let start = f
        .start
        .as_deref()
        .map(|s| parse_when(ctx, s, all_day, recurring))
        .transpose()?;
    if let Some(e) = &f.end {
        let end = parse_when(ctx, e, all_day, recurring)?;
        p.end = Some(end);
    } else if let Some(d) = &f.duration {
        let secs = timeexpr::parse_duration(d)?;
        if secs <= 0 {
            return Err(AppError::usage("--duration must be positive"));
        }
        let base = start
            .clone()
            .or_else(|| existing.map(|e| e.start.clone()))
            .ok_or_else(|| AppError::usage("--duration needs --start"))?;
        p.end = Some(add_secs(&base, secs, &ctx.tz));
    } else if let Some(s) = &start {
        let dur = match existing {
            Some(e) => e.duration_secs(&ctx.tz),
            None if all_day => 86400,
            None => 3600,
        };
        p.end = Some(add_secs(s, dur, &ctx.tz));
    }
    if let (Some(s), Some(e)) = (start.as_ref().or(existing.map(|x| &x.start)), &p.end)
        && e.instant(&ctx.tz) <= s.instant(&ctx.tz)
    {
        return Err(AppError::usage("event end must be after start"));
    }
    p.start = start;
    p.location = f.location.clone().map(Some);
    p.notes = f.notes.clone().map(Some);
    p.url = f.url.clone().map(Some);
    p.rrule = f
        .rrule
        .clone()
        .map(|r| Some(r.trim_start_matches("RRULE:").to_string()));
    if let Some(Some(r)) = &p.rrule
        && !r.is_empty()
        && !r.to_ascii_uppercase().contains("FREQ=")
    {
        return Err(AppError::usage(format!("invalid --rrule `{r}`"))
            .hint("Use RFC 5545 syntax, e.g. FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10"));
    }
    if f.no_alarms {
        p.alarms = Some(vec![]);
    } else if !f.alarms.is_empty() {
        p.alarms = Some(
            f.alarms
                .iter()
                .map(|a| timeexpr::parse_duration(a))
                .collect::<Result<_>>()?,
        );
    }
    if f.free {
        p.busy = Some(false);
    }
    Ok(p)
}

fn uid_for(key: Option<&str>) -> String {
    match key {
        Some(k) => {
            let h = Sha256::digest(k.as_bytes());
            format!("icloud-cli-{}", &hex::encode(h)[..32])
        }
        None => uuid::Uuid::new_v4().to_string().to_uppercase(),
    }
}

/// Fields used to decide whether an idempotent re-create is a no-op.
fn core_fields(v: &Value) -> Value {
    json!({ "title": v["title"], "start": v["start"], "end": v["end"], "all_day": v["all_day"], "location": v["location"], "notes": v["notes"], "rrule": v["rrule"], "alarms": v["alarms"] })
}

/// Validate time/duration arguments before touching the network.
fn prevalidate(ctx: &Ctx, cmd: &CalCmd) -> Result<()> {
    let fields = |f: &EventFields| -> Result<()> {
        for t in [&f.start, &f.end].into_iter().flatten() {
            timeexpr::parse(t, &ctx.tz)?;
        }
        if let Some(d) = &f.duration {
            timeexpr::parse_duration(d)?;
        }
        for a in &f.alarms {
            timeexpr::parse_duration(a)?;
        }
        Ok(())
    };
    match cmd {
        CalCmd::Events(EventsCmd::List { from, to, .. })
        | CalCmd::Events(EventsCmd::Search { from, to, .. }) => window(ctx, from, to).map(|_| ()),
        CalCmd::Free(a) => {
            window(ctx, &a.from, &a.to)?;
            timeexpr::parse_duration(&a.min).map(|_| ())
        }
        CalCmd::Events(EventsCmd::Create { f, .. }) => {
            if f.title
                .as_deref()
                .map(|t| t.trim().is_empty())
                .unwrap_or(true)
            {
                return Err(AppError::usage("--title is required"));
            }
            if f.start.is_none() {
                return Err(AppError::usage("--start is required")
                    .hint("e.g. --start 2026-10-16T15:00 --duration 1h"));
            }
            ctx.guard_write("cal events create")?;
            fields(f)
        }
        CalCmd::Events(EventsCmd::Update { f, .. }) => {
            ctx.guard_write("cal events update")?;
            fields(f)
        }
        CalCmd::Events(EventsCmd::Delete { .. }) => ctx.guard_write("cal events delete"),
        CalCmd::Calendars(CalendarsCmd::Create { .. }) => ctx.guard_write("cal calendars create"),
        CalCmd::Calendars(CalendarsCmd::Delete { .. }) => ctx.guard_write("cal calendars delete"),
        _ => Ok(()),
    }
}

pub async fn run(ctx: &Ctx, cmd: &CalCmd) -> Result<Out> {
    prevalidate(ctx, cmd)?;
    let c = Cal::connect(ctx).await?;
    match cmd {
        CalCmd::Calendars(CalendarsCmd::List) => {
            let cals = c.event_calendars().await?;
            Ok(Out::new(
                "cal.calendars.list",
                Value::Array(cals.iter().map(|c| c.json()).collect()),
            ))
        }
        CalCmd::Calendars(CalendarsCmd::Create { name, color }) => {
            ctx.guard_write("cal calendars create")?;
            let id = uuid::Uuid::new_v4().to_string().to_uppercase();
            let url = c
                .home
                .join(&format!("{id}/"))
                .map_err(|e| AppError::internal(e.to_string()))?;
            let color_xml = color
                .as_ref()
                .map(|c| format!("<ic:calendar-color>{}</ic:calendar-color>", xml_escape(c)))
                .unwrap_or_default();
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/"><d:set><d:prop><d:displayname>{}</d:displayname><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>{color_xml}</d:prop></d:set></c:mkcalendar>"#,
                xml_escape(name)
            );
            let data = json!({ "action": "create", "id": id, "name": name, "color": color });
            if ctx.dry_run {
                return Ok(Out::new("cal.calendars.create", data).dry(true));
            }
            let r = c.dav.request("MKCALENDAR", &url, &[], Some(body)).await?;
            c.dav.check(&r)?;
            Ok(Out::new(
                "cal.calendars.create",
                json!({ "action": "created", "id": id, "name": name, "color": color }),
            ))
        }
        CalCmd::Calendars(CalendarsCmd::Delete { calendar, yes }) => {
            let all = c.event_calendars().await?;
            let cal = resolve_cal(&all, calendar)?;
            ctx.guard_destructive("cal calendars delete", *yes, cal.json())?;
            let data = json!({ "action": "delete", "calendar": cal.json(), "warning": "deletes every event in this calendar" });
            if ctx.dry_run {
                return Ok(Out::new("cal.calendars.delete", data).dry(true));
            }
            let r = c.dav.request("DELETE", &cal.url, &[], None).await?;
            c.dav.check(&r)?;
            Ok(Out::new(
                "cal.calendars.delete",
                json!({ "action": "deleted", "id": cal.id, "name": cal.name }),
            ))
        }
        CalCmd::Events(EventsCmd::List {
            from,
            to,
            calendars,
            no_expand,
        }) => {
            let (f, t) = window(ctx, from, to)?;
            let cals = c.select(calendars).await?;
            let rows = query_window(ctx, &c, &cals, f, t, !no_expand).await?;
            let (page, meta) = paginate(rows, ctx.limit, ctx.cursor.as_deref());
            Ok(Out::new("cal.events.list", Value::Array(page)).page(meta))
        }
        CalCmd::Events(EventsCmd::Search {
            query,
            from,
            to,
            calendars,
        }) => {
            let (f, t) = window(ctx, from, to)?;
            let cals = c.select(calendars).await?;
            let q = query.to_lowercase();
            let rows: Vec<Value> = query_window(ctx, &c, &cals, f, t, true)
                .await?
                .into_iter()
                .filter(|v| {
                    ["title", "location", "notes"].iter().any(|k| {
                        v[*k]
                            .as_str()
                            .map(|s| s.to_lowercase().contains(&q))
                            .unwrap_or(false)
                    }) || v["attendees"].to_string().to_lowercase().contains(&q)
                })
                .collect();
            let (page, meta) = paginate(rows, ctx.limit, ctx.cursor.as_deref());
            Ok(Out::new("cal.events.search", Value::Array(page)).page(meta))
        }
        CalCmd::Events(EventsCmd::Get { id, raw }) => {
            let (cid, file) = split_id(id)?;
            let cal = resolve_cal(&c.calendars().await?, &cid)?;
            let fe = fetch(&c, &event_url(&cal, &file)?).await?;
            let (_, evs) = event::parse_resource(&fe.body, &ctx.tz)?;
            let master = evs
                .iter()
                .find(|e| e.recurrence_id.is_none())
                .or(evs.first())
                .ok_or_else(|| AppError::not_found("resource has no VEVENT"))?;
            let mut j = master.to_json(&ctx.tz, None);
            j["id"] = json!(format!("{}/{}", cal.id, file));
            j["calendar"] = json!(cal.name);
            j["calendar_id"] = json!(cal.id);
            j["etag"] = json!(fe.etag);
            let overrides: Vec<Value> = evs
                .iter()
                .filter(|e| e.recurrence_id.is_some())
                .map(|e| e.to_json(&ctx.tz, None))
                .collect();
            if !overrides.is_empty() {
                j["overrides"] = json!(overrides);
            }
            if *raw {
                j["ics"] = json!(fe.body);
            }
            Ok(Out::new("cal.events.get", j))
        }
        CalCmd::Events(EventsCmd::Create {
            calendar,
            f,
            idempotency_key,
        }) => {
            ctx.guard_write("cal events create")?;
            let title = f
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| AppError::usage("--title is required"))?;
            let _ = title;
            if f.start.is_none() {
                return Err(AppError::usage("--start is required")
                    .hint("e.g. --start 2026-10-16T15:00 --duration 1h"));
            }
            let all = c.event_calendars().await?;
            let cal = match calendar
                .as_deref()
                .or(ctx.profile.default_calendar.as_deref())
            {
                Some(w) => resolve_cal(&all, w)?,
                None => all
                    .first()
                    .cloned()
                    .ok_or_else(|| AppError::not_found("no event calendars in this account"))?,
            };
            let patch = build_patch(ctx, f, None)?;
            let uid = uid_for(idempotency_key.as_deref());
            let now = Timestamp::now();
            let ics = event::new_calendar(&uid, &patch, &now).serialize();
            let file = format!("{uid}.ics");
            let id = format!("{}/{}", cal.id, file);
            let (_, evs) = event::parse_resource(&ics, &ctx.tz)?;
            let mut ej = evs[0].to_json(&ctx.tz, None);
            ej["id"] = json!(id);
            ej["calendar"] = json!(cal.name);
            ej["calendar_id"] = json!(cal.id);
            if ctx.dry_run {
                return Ok(Out::new(
                    "cal.events.create",
                    json!({ "action": "create", "uid": uid, "event": ej, "ics_preview": ics }),
                )
                .dry(true));
            }
            let url = event_url(&cal, &file)?;
            let r = c
                .dav
                .request(
                    "PUT",
                    &url,
                    &[
                        ("If-None-Match", "*".into()),
                        ("Content-Type", "text/calendar; charset=utf-8".into()),
                    ],
                    Some(ics),
                )
                .await?;
            if r.status.as_u16() == 412 && idempotency_key.is_some() {
                let fe = fetch(&c, &url).await?;
                let (_, ex) = event::parse_resource(&fe.body, &ctx.tz)?;
                let mut xj = ex
                    .first()
                    .map(|e| e.to_json(&ctx.tz, None))
                    .unwrap_or(Value::Null);
                if core_fields(&xj) == core_fields(&ej) {
                    xj["id"] = json!(id);
                    xj["calendar"] = json!(cal.name);
                    xj["calendar_id"] = json!(cal.id);
                    xj["etag"] = json!(fe.etag);
                    return Ok(Out::new(
                        "cal.events.create",
                        json!({ "action": "noop", "uid": uid, "event": xj }),
                    ));
                }
                return Err(AppError::new("E_CONFLICT", "an event with this idempotency key already exists with different content")
                    .hint("Use `cal events update <id>` to change it, or a new --idempotency-key for a different event.")
                    .details(json!({ "id": id, "existing": core_fields(&xj) })));
            }
            c.dav.check(&r)?;
            ej["etag"] = json!(r.headers.get("etag").and_then(|v| v.to_str().ok()));
            Ok(Out::new(
                "cal.events.create",
                json!({ "action": "created", "uid": uid, "event": ej }),
            ))
        }
        CalCmd::Events(EventsCmd::Update { id, f, if_match }) => {
            ctx.guard_write("cal events update")?;
            let (cid, file) = split_id(id)?;
            let cal = resolve_cal(&c.calendars().await?, &cid)?;
            let fe = fetch(&c, &event_url(&cal, &file)?).await?;
            if let (Some(want), Some(have)) = (if_match, &fe.etag)
                && want.trim_matches('"') != have.trim_matches('"')
            {
                return Err(AppError::new(
                    "E_CONFLICT",
                    "etag mismatch: the event changed since you read it",
                )
                .hint("Re-run `cal events get` and retry with the new etag.")
                .details(json!({ "expected": want, "current": have })));
            }
            let (mut calc, evs) = event::parse_resource(&fe.body, &ctx.tz)?;
            let master = evs
                .iter()
                .find(|e| e.recurrence_id.is_none())
                .ok_or_else(|| AppError::not_found("resource has no master VEVENT"))?
                .clone();
            let before = master.to_json(&ctx.tz, None);
            let patch = build_patch(ctx, f, Some(&master))?;
            let now = Timestamp::now();
            let vev = calc
                .children
                .iter_mut()
                .find(|c| c.name == "VEVENT" && c.get("RECURRENCE-ID").is_none())
                .ok_or_else(|| AppError::not_found("resource has no master VEVENT"))?;
            event::apply_patch(vev, &patch, &now);
            event::bump_sequence(vev);
            let ics = calc.serialize();
            let (_, evs2) = event::parse_resource(&ics, &ctx.tz)?;
            let mut after = evs2
                .iter()
                .find(|e| e.recurrence_id.is_none())
                .unwrap()
                .to_json(&ctx.tz, None);
            after["id"] = json!(format!("{}/{}", cal.id, file));
            after["calendar"] = json!(cal.name);
            after["calendar_id"] = json!(cal.id);
            let changed: Vec<String> = after
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| {
                    before.get(*k) != after.get(*k)
                        && !["id", "calendar", "calendar_id"].contains(&k.as_str())
                })
                .cloned()
                .collect();
            if ctx.dry_run {
                return Ok(Out::new("cal.events.update", json!({ "action": "update", "changed": changed, "before": before, "after": after })).dry(true));
            }
            let mut hs = vec![("Content-Type", "text/calendar; charset=utf-8".to_string())];
            if let Some(e) = &fe.etag {
                hs.push(("If-Match", e.clone()));
            }
            let r = c.dav.request("PUT", &fe.url, &hs, Some(ics)).await?;
            c.dav.check(&r)?;
            after["etag"] = json!(r.headers.get("etag").and_then(|v| v.to_str().ok()));
            Ok(Out::new(
                "cal.events.update",
                json!({ "action": "updated", "changed": changed, "event": after }),
            ))
        }
        CalCmd::Events(EventsCmd::Delete { id, yes, if_match }) => {
            let (cid, file) = split_id(id)?;
            ctx.guard_write("cal events delete")?;
            let cal = resolve_cal(&c.calendars().await?, &cid)?;
            let fe = fetch(&c, &event_url(&cal, &file)?).await?;
            let (_, evs) = event::parse_resource(&fe.body, &ctx.tz)?;
            let mut summary = evs
                .iter()
                .find(|e| e.recurrence_id.is_none())
                .or(evs.first())
                .map(|e| e.to_json(&ctx.tz, None))
                .unwrap_or(Value::Null);
            summary["id"] = json!(format!("{}/{}", cal.id, file));
            summary["calendar"] = json!(cal.name);
            ctx.guard_destructive("cal events delete", *yes, json!([summary]))?;
            if ctx.dry_run {
                return Ok(Out::new(
                    "cal.events.delete",
                    json!({ "action": "delete", "event": summary }),
                )
                .dry(true));
            }
            let etag = if_match.clone().or(fe.etag.clone());
            let hs: Vec<(&str, String)> = etag.into_iter().map(|e| ("If-Match", e)).collect();
            let r = c.dav.request("DELETE", &fe.url, &hs, None).await?;
            c.dav.check(&r)?;
            Ok(Out::new(
                "cal.events.delete",
                json!({ "action": "deleted", "id": summary["id"], "title": summary["title"] }),
            ))
        }
        CalCmd::Events(EventsCmd::Sync {
            calendars,
            since_token,
        }) => sync(ctx, &c, calendars, since_token.as_deref()).await,
        CalCmd::Free(a) => free(ctx, &c, a).await,
    }
}

fn decode_token(t: &str) -> Result<serde_json::Map<String, Value>> {
    use base64::Engine;
    let bad = || {
        AppError::usage("invalid --since-token").hint("Pass the `next_token` from a previous `cal events sync` unchanged, or omit it for a full sync.")
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .map_err(|_| bad())?;
    match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(bad()),
    }
}

fn top_level_sync_token(xml: &str) -> Option<String> {
    // the collection token is the last <sync-token> in a sync-collection multistatus
    let lower = xml;
    let mut res = None;
    let mut i = 0;
    while let Some(p) = lower[i..].find("sync-token>") {
        let start = i + p + "sync-token>".len();
        let before = &lower[..i + p];
        if !before.ends_with('/')
            && !before.ends_with("</")
            && !before
                .trim_end_matches(|c: char| c.is_alphanumeric() || c == ':')
                .ends_with("</")
            && let Some(end) = lower[start..].find('<')
        {
            let v = lower[start..start + end].trim();
            if !v.is_empty() {
                res = Some(v.to_string());
            }
        }
        i = start;
    }
    res
}

async fn sync(ctx: &Ctx, c: &Cal, wanted: &[String], since: Option<&str>) -> Result<Out> {
    use base64::Engine;
    let cals = c.select(wanted).await?;
    let prev = since.map(decode_token).transpose()?.unwrap_or_default();
    let mut next = serde_json::Map::new();
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    for cal in &cals {
        let tok = prev.get(&cal.id).and_then(|v| v.as_str()).unwrap_or("");
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:sync-collection xmlns:d="DAV:"><d:sync-token>{}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"#,
            xml_escape(tok)
        );
        let r = c
            .dav
            .request("REPORT", &cal.url, &[("Depth", "1".into())], Some(body))
            .await?;
        if matches!(r.status.as_u16(), 403 | 409) && !tok.is_empty() {
            return Err(AppError::new(
                "E_CONFLICT",
                format!("sync token for calendar `{}` is no longer valid", cal.id),
            )
            .hint("Run `cal events sync` without --since-token to start over (full listing)."));
        }
        c.dav.check(&r)?;
        let rs = dav::parse_multistatus(&r.body)?;
        if let Some(t) = top_level_sync_token(&r.body) {
            next.insert(cal.id.clone(), json!(t));
        }
        let mut hrefs = Vec::new();
        for x in &rs {
            if x.href.trim_end_matches('/') == cal.url.path().trim_end_matches('/') {
                continue;
            }
            if x.status == Some(404) {
                deleted.push(json!({ "id": format!("{}/{}", cal.id, dav::last_segment(&x.href)), "calendar_id": cal.id }));
            } else {
                hrefs.push(x.href.clone());
            }
        }
        for chunk in hrefs.chunks(50) {
            let hs: String = chunk
                .iter()
                .map(|h| format!("<d:href>{}</d:href>", xml_escape(h)))
                .collect();
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop>{hs}</c:calendar-multiget>"#
            );
            let rs = c.dav.report(&cal.url, "1", body).await?;
            for r in rs {
                let Some(data) = r.text("calendar-data") else {
                    continue;
                };
                let Ok((_, evs)) = event::parse_resource(data, &ctx.tz) else {
                    continue;
                };
                if let Some(m) = evs
                    .iter()
                    .find(|e| e.recurrence_id.is_none())
                    .or(evs.first())
                {
                    let mut j = m.to_json(&ctx.tz, None);
                    j["id"] = json!(format!("{}/{}", cal.id, dav::last_segment(&r.href)));
                    j["calendar"] = json!(cal.name);
                    j["calendar_id"] = json!(cal.id);
                    j["etag"] = json!(r.text("getetag").map(|s| s.trim()));
                    changed.push(j);
                }
            }
        }
    }
    let token =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Value::Object(next).to_string());
    Ok(Out::new(
        "cal.events.sync",
        json!({ "changed": changed, "deleted": deleted, "next_token": token, "full": since.is_none() }),
    ))
}

fn hm(s: &str) -> Result<(i8, i8)> {
    let (h, m) = s
        .trim()
        .split_once(':')
        .ok_or_else(|| AppError::usage(format!("bad time `{s}` in --work-hours (use HH:MM)")))?;
    Ok((
        h.parse()
            .map_err(|_| AppError::usage("bad hour in --work-hours"))?,
        m.parse()
            .map_err(|_| AppError::usage("bad minute in --work-hours"))?,
    ))
}

async fn free(ctx: &Ctx, c: &Cal, a: &FreeArgs) -> Result<Out> {
    let (f, t) = window(ctx, &a.from, &a.to)?;
    let min = timeexpr::parse_duration(&a.min)?;
    let cals = c.select(&a.calendars).await?;
    let evs = query_window(ctx, c, &cals, f, t, true).await?;
    let mut busy: Vec<(Timestamp, Timestamp)> = Vec::new();
    for e in &evs {
        if e["busy"] == json!(false) || e["status"] == json!("cancelled") {
            continue;
        }
        if e["all_day"] == json!(true) && !a.all_day_busy {
            continue;
        }
        let p = |k: &str| -> Option<Timestamp> {
            let s = e[k].as_str()?;
            timeexpr::parse(s, &ctx.tz).ok().map(|z| z.timestamp())
        };
        if let (Some(s), Some(en)) = (p("start"), p("end")) {
            busy.push((s, en.max(s)));
        }
    }
    busy.sort();
    // candidate windows
    let mut windows: Vec<(Timestamp, Timestamp)> = Vec::new();
    let wh = a
        .work_hours
        .as_deref()
        .map(|w| -> Result<_> {
            let (s, e) = w
                .split_once('-')
                .ok_or_else(|| AppError::usage("--work-hours must look like 09:00-18:00"))?;
            Ok((hm(s)?, hm(e)?))
        })
        .transpose()?;
    let fz: Zoned = f.to_zoned(ctx.tz.clone());
    let mut day = fz.date();
    let last = t.to_zoned(ctx.tz.clone()).date();
    while day <= last {
        let wd = day.weekday();
        let weekend = matches!(
            wd,
            jiff::civil::Weekday::Saturday | jiff::civil::Weekday::Sunday
        );
        if !(a.weekdays_only && weekend) {
            let (ws, we) = match wh {
                Some(((sh, sm), (eh, em))) => (
                    day.at(sh, sm, 0, 0)
                        .to_zoned(ctx.tz.clone())
                        .map_err(|e| AppError::internal(e.to_string()))?
                        .timestamp(),
                    day.at(eh, em, 0, 0)
                        .to_zoned(ctx.tz.clone())
                        .map_err(|e| AppError::internal(e.to_string()))?
                        .timestamp(),
                ),
                None => (
                    day.to_zoned(ctx.tz.clone())
                        .map_err(|e| AppError::internal(e.to_string()))?
                        .timestamp(),
                    day.tomorrow()
                        .unwrap()
                        .to_zoned(ctx.tz.clone())
                        .map_err(|e| AppError::internal(e.to_string()))?
                        .timestamp(),
                ),
            };
            let (s, e) = (ws.max(f), we.min(t));
            if s < e {
                windows.push((s, e));
            }
        }
        day = day
            .tomorrow()
            .map_err(|e| AppError::internal(e.to_string()))?;
    }
    // merge adjacent windows when no work-hours (continuous range)
    let mut slots = Vec::new();
    let mut merged: Vec<(Timestamp, Timestamp)> = Vec::new();
    for w in windows {
        match merged.last_mut() {
            Some(l) if l.1 == w.0 => l.1 = w.1,
            _ => merged.push(w),
        }
    }
    for (ws, we) in merged {
        let mut cur = ws;
        for (bs, be) in &busy {
            if *be <= cur || *bs >= we {
                continue;
            }
            if *bs > cur {
                slots.push((cur, (*bs).min(we)));
            }
            cur = cur.max(*be);
            if cur >= we {
                break;
            }
        }
        if cur < we {
            slots.push((cur, we));
        }
    }
    let out: Vec<Value> = slots
        .into_iter()
        .filter(|(s, e)| e.as_second() - s.as_second() >= min)
        .map(|(s, e)| json!({ "start": timeexpr::fmt(&s.to_zoned(ctx.tz.clone())), "end": timeexpr::fmt(&e.to_zoned(ctx.tz.clone())), "minutes": (e.as_second() - s.as_second()) / 60 }))
        .collect();
    let (page, meta) = paginate(out, ctx.limit, ctx.cursor.as_deref());
    Ok(Out::new("cal.free", Value::Array(page)).page(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sync_token_extraction() {
        let x = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/a.ics</d:href></d:response><d:sync-token>http://radicale.org/ns/sync/abc</d:sync-token></d:multistatus>"#;
        assert_eq!(
            top_level_sync_token(x).as_deref(),
            Some("http://radicale.org/ns/sync/abc")
        );
        let y = r#"<multistatus xmlns="DAV:"><sync-token>HwoQEgwAAA</sync-token></multistatus>"#;
        assert_eq!(top_level_sync_token(y).as_deref(), Some("HwoQEgwAAA"));
    }
    #[test]
    fn ids_and_uids() {
        assert_eq!(
            split_id("home/A.ics").unwrap(),
            ("home".into(), "A.ics".into())
        );
        assert!(split_id("nope").is_err());
        assert_eq!(uid_for(Some("k")), uid_for(Some("k")));
        assert_ne!(uid_for(Some("k")), uid_for(Some("j")));
    }
}
