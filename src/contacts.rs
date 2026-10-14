//! `icloud contacts ...` — CardDAV, read-only in v0.1.
use crate::ctx::Ctx;
use crate::dav::{self, Dav, xml_escape};
use crate::error::{AppError, Result};
use crate::ical::{self, Component};
use crate::output::{Out, paginate};
use clap::Subcommand;
use reqwest::Url;
use serde_json::{Value, json};

#[derive(Subcommand, Debug)]
pub enum ContactsCmd {
    /// Address books
    #[command(subcommand)]
    Books(BooksCmd),
    /// List contacts (sorted by name)
    List {
        #[arg(long)]
        book: Option<String>,
    },
    /// Get one contact by id
    Get {
        id: String,
        #[arg(long)]
        raw: bool,
    },
    /// Search names, org, emails, phones, notes (case-insensitive substring)
    Search {
        query: String,
        #[arg(long)]
        book: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum BooksCmd {
    List,
}

struct Book {
    id: String,
    name: String,
    url: Url,
}

async fn connect(ctx: &Ctx) -> Result<(Dav, Url)> {
    let (dav, base) = ctx.dav("carddav").await?;
    let home = match std::env::var("ICLOUD_CARDDAV_HOME") {
        Ok(h) => Url::parse(&h).map_err(|e| AppError::new("E_CONFIG", e.to_string()))?,
        Err(_) => dav::discover_home(&dav, &base, "addressbook-home-set").await?,
    };
    Ok((dav, home))
}

async fn books(dav: &Dav, home: &Url) -> Result<Vec<Book>> {
    let (u, rs) = dav
        .propfind(home, "1", "<d:resourcetype/><d:displayname/>")
        .await?;
    let mut out = Vec::new();
    for r in rs {
        if !r
            .props
            .get("resourcetype")
            .map(|p| p.children.iter().any(|c| c == "addressbook"))
            .unwrap_or(false)
        {
            continue;
        }
        let id = dav::last_segment(&r.href);
        out.push(Book {
            name: r
                .text("displayname")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| id.clone()),
            id,
            url: u
                .join(&r.href)
                .map_err(|e| AppError::internal(e.to_string()))?,
        });
    }
    Ok(out)
}

fn pick<'a>(all: &'a [Book], w: Option<&str>) -> Result<Vec<&'a Book>> {
    match w {
        None => Ok(all.iter().collect()),
        Some(w) => all
            .iter()
            .find(|b| b.id == w || b.name.eq_ignore_ascii_case(w))
            .map(|b| vec![b])
            .ok_or_else(|| {
                AppError::not_found(format!("no address book `{w}`"))
                    .hint("Run `icloud contacts books list`.")
            }),
    }
}

fn typed(c: &Component, name: &str) -> Vec<Value> {
    c.get_all(name)
        .map(|p| {
            let ty = p.param("TYPE").map(|t| t.to_ascii_lowercase().split(',').filter(|x| *x != "internet" && *x != "pref" && *x != "voice").collect::<Vec<_>>().join(","));
            json!({ "value": ical::unescape(&p.value).trim_start_matches("tel:").to_string(), "type": ty.filter(|t| !t.is_empty()) })
        })
        .collect()
}

pub fn vcard_json(c: &Component) -> Value {
    let n: Vec<String> = c
        .get("N")
        .map(|p| p.value.split(';').map(ical::unescape).collect())
        .unwrap_or_default();
    let fname = c.text("FN").filter(|s| !s.trim().is_empty());
    let last = n.first().cloned().filter(|s| !s.is_empty());
    let first = n.get(1).cloned().filter(|s| !s.is_empty());
    let org = c
        .get("ORG")
        .map(|p| {
            p.value
                .split(';')
                .map(ical::unescape)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .filter(|s| !s.is_empty());
    let name = fname.or_else(|| match (&last, &first) {
        (Some(l), Some(f)) => Some(format!("{l}{f}")),
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        _ => org.clone(),
    });
    let bday = c.get("BDAY").map(|p| {
        let v = p.value.trim();
        if v.len() == 8 && v.chars().all(|c| c.is_ascii_digit()) {
            format!("{}-{}-{}", &v[..4], &v[4..6], &v[6..])
        } else {
            v.to_string()
        }
    });
    let addrs: Vec<Value> = c
        .get_all("ADR")
        .map(|p| {
            let parts: Vec<String> = p.value.split(';').map(ical::unescape).collect();
            let g = |i: usize| parts.get(i).cloned().filter(|s| !s.is_empty());
            json!({ "type": p.param("TYPE").map(|t| t.to_ascii_lowercase()), "street": g(2), "city": g(3), "region": g(4), "postal_code": g(5), "country": g(6) })
        })
        .collect();
    json!({
        "uid": c.text("UID"),
        "name": name,
        "first_name": first,
        "last_name": last,
        "nickname": c.text("NICKNAME"),
        "org": org,
        "title": c.text("TITLE"),
        "emails": typed(c, "EMAIL"),
        "phones": typed(c, "TEL"),
        "addresses": addrs,
        "birthday": bday,
        "notes": c.text("NOTE"),
        "urls": c.get_all("URL").map(|p| ical::unescape(&p.value)).collect::<Vec<_>>(),
        "is_group": c.get("X-ADDRESSBOOKSERVER-KIND").map(|p| p.value.eq_ignore_ascii_case("group")).unwrap_or(false),
    })
}

async fn load(dav: &Dav, b: &Book) -> Result<Vec<Value>> {
    let (_, rs) = dav
        .propfind(&b.url, "1", "<d:getetag/><d:resourcetype/>")
        .await?;
    let hrefs: Vec<String> = rs
        .iter()
        .filter(|r| {
            r.href.trim_end_matches('/') != b.url.path().trim_end_matches('/')
                && !r
                    .props
                    .get("resourcetype")
                    .map(|p| p.children.iter().any(|c| c == "collection"))
                    .unwrap_or(false)
        })
        .map(|r| r.href.clone())
        .collect();
    let mut out = Vec::new();
    for chunk in hrefs.chunks(100) {
        let hs: String = chunk
            .iter()
            .map(|h| format!("<d:href>{}</d:href>", xml_escape(h)))
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><card:address-data/></d:prop>{hs}</card:addressbook-multiget>"#
        );
        for r in dav.report(&b.url, "1", body).await? {
            let Some(data) = r.text("address-data") else {
                continue;
            };
            let Ok(cards) = ical::parse(data) else {
                continue;
            };
            for c in cards.iter().filter(|c| c.name == "VCARD") {
                let mut j = vcard_json(c);
                if j["is_group"] == json!(true) {
                    continue;
                }
                j["id"] = json!(format!("{}/{}", b.id, dav::last_segment(&r.href)));
                j["book"] = json!(b.name);
                j["etag"] = json!(r.text("getetag").map(|s| s.trim()));
                out.push(j);
            }
        }
    }
    Ok(out)
}

pub async fn run(ctx: &Ctx, cmd: &ContactsCmd) -> Result<Out> {
    let (dav, home) = connect(ctx).await?;
    let all = books(&dav, &home).await?;
    match cmd {
        ContactsCmd::Books(BooksCmd::List) => Ok(Out::new(
            "contacts.books.list",
            json!(
                all.iter()
                    .map(|b| json!({ "id": b.id, "name": b.name }))
                    .collect::<Vec<_>>()
            ),
        )),
        ContactsCmd::List { book } | ContactsCmd::Search { book, .. } => {
            let sel = pick(&all, book.as_deref())?;
            let res = futures::future::join_all(sel.iter().map(|b| load(&dav, b))).await;
            let mut rows = Vec::new();
            for r in res {
                rows.extend(r?);
            }
            let schema = if let ContactsCmd::Search { query, .. } = cmd {
                let q = query.to_lowercase();
                let qd: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
                rows.retain(|v| {
                    let hay = format!(
                        "{} {} {} {} {} {}",
                        v["name"], v["nickname"], v["org"], v["emails"], v["notes"], v["title"]
                    )
                    .to_lowercase();
                    let phones: String = v["phones"]
                        .to_string()
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect();
                    hay.contains(&q) || (qd.len() >= 3 && phones.contains(&qd))
                });
                "contacts.search"
            } else {
                "contacts.list"
            };
            rows.sort_by(|a, b| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            });
            let (page, meta) = paginate(rows, ctx.limit, ctx.cursor.as_deref());
            Ok(Out::new(schema, Value::Array(page)).page(meta))
        }
        ContactsCmd::Get { id, raw } => {
            let (bid, file) = id
                .split_once('/')
                .ok_or_else(|| AppError::usage("contact ids look like <book-id>/<file>.vcf"))?;
            let b = pick(&all, Some(bid))?[0];
            let url = b
                .url
                .join(
                    &percent_encoding::utf8_percent_encode(
                        file,
                        percent_encoding::NON_ALPHANUMERIC,
                    )
                    .to_string()
                    .replace("%2E", ".")
                    .replace("%2D", "-")
                    .replace("%5F", "_"),
                )
                .map_err(|e| AppError::internal(e.to_string()))?;
            let r = dav.request("GET", &url, &[], None).await?;
            dav.check(&r)?;
            let cards = ical::parse(&r.body).map_err(|e| AppError::new("E_UPSTREAM", e))?;
            let c = cards
                .iter()
                .find(|c| c.name == "VCARD")
                .ok_or_else(|| AppError::not_found("no VCARD in resource"))?;
            let mut j = vcard_json(c);
            j["id"] = json!(id);
            j["book"] = json!(b.name);
            j["etag"] = json!(r.headers.get("etag").and_then(|v| v.to_str().ok()));
            if *raw {
                j["vcf"] = json!(r.body);
            }
            Ok(Out::new("contacts.get", j))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vcard_fields() {
        let v = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:王;小明;;;\r\nFN:王小明\r\nORG:ACME;R&D\r\nitem1.EMAIL;type=INTERNET;type=WORK:xm@example.com\r\nTEL;type=CELL;type=VOICE:+86 138 0000 0000\r\nBDAY:19900102\r\nEND:VCARD\r\n";
        let c = &ical::parse(v).unwrap()[0];
        let j = vcard_json(c);
        assert_eq!(j["name"], "王小明");
        assert_eq!(j["first_name"], "小明");
        assert_eq!(j["org"], "ACME / R&D");
        assert_eq!(j["emails"][0]["value"], "xm@example.com");
        assert_eq!(j["birthday"], "1990-01-02");
    }
}
