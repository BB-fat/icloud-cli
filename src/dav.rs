//! Small WebDAV / CalDAV / CardDAV client on top of reqwest + quick-xml.
use crate::error::{AppError, Result, http_error};
use quick_xml::Reader;
use quick_xml::events::Event as XEvent;
use reqwest::{Method, StatusCode, Url, header};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct PropVal {
    pub text: String,
    pub children: Vec<String>,
    pub hrefs: Vec<String>,
    pub comps: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DavResponse {
    pub href: String,
    pub status: Option<u16>,
    pub props: HashMap<String, PropVal>,
}

impl DavResponse {
    pub fn text(&self, k: &str) -> Option<&str> {
        self.props
            .get(k)
            .map(|p| p.text.as_str())
            .filter(|s| !s.is_empty())
    }
}

pub struct Dav {
    pub http: reqwest::Client,
    pub user: String,
    pub pass: String,
    pub service: &'static str,
}

pub struct Resp {
    pub status: StatusCode,
    pub url: Url,
    pub headers: header::HeaderMap,
    pub body: String,
}

impl Dav {
    pub fn new(user: &str, pass: &str, timeout: Duration, service: &'static str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(concat!("icloud-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AppError::internal(e.to_string()))?;
        Ok(Dav {
            http,
            user: user.into(),
            pass: pass.into(),
            service,
        })
    }

    pub async fn request(
        &self,
        method: &str,
        url: &Url,
        headers: &[(&str, String)],
        body: Option<String>,
    ) -> Result<Resp> {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let mut url = url.clone();
        for _ in 0..6 {
            let mut rb = self
                .http
                .request(m.clone(), url.clone())
                .basic_auth(&self.user, Some(&self.pass));
            for (k, v) in headers {
                rb = rb.header(*k, v);
            }
            if let Some(b) = &body {
                rb = rb.body(b.clone());
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    rb = rb.header(header::CONTENT_TYPE, "application/xml; charset=utf-8");
                }
            }
            let r = rb.send().await?;
            let st = r.status();
            if st.is_redirection()
                && let Some(loc) = r
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
            {
                url = url
                    .join(loc)
                    .map_err(|e| AppError::new("E_UPSTREAM", e.to_string()))?;
                continue;
            }
            let hs = r.headers().clone();
            let final_url = r.url().clone();
            let body = r.text().await?;
            return Ok(Resp {
                status: st,
                url: final_url,
                headers: hs,
                body,
            });
        }
        Err(AppError::new("E_UPSTREAM", "too many redirects"))
    }

    pub fn check(&self, r: &Resp) -> Result<()> {
        if r.status.is_success() {
            Ok(())
        } else {
            Err(http_error(r.status.as_u16(), self.service, r.url.as_str()))
        }
    }

    pub async fn propfind(
        &self,
        url: &Url,
        depth: &str,
        props: &str,
    ) -> Result<(Url, Vec<DavResponse>)> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/"><d:prop>{props}</d:prop></d:propfind>"#
        );
        let r = self
            .request("PROPFIND", url, &[("Depth", depth.into())], Some(body))
            .await?;
        self.check(&r)?;
        Ok((r.url.clone(), parse_multistatus(&r.body)?))
    }

    pub async fn report(&self, url: &Url, depth: &str, body: String) -> Result<Vec<DavResponse>> {
        let r = self
            .request("REPORT", url, &[("Depth", depth.into())], Some(body))
            .await?;
        self.check(&r)?;
        parse_multistatus(&r.body)
    }
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

/// Namespace-agnostic multistatus parser (matches local names only; tolerant of iCloud quirks).
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>> {
    let mut rd = Reader::from_str(xml);
    rd.config_mut().trim_text(false);
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut cur: Option<DavResponse> = None;
    let mut ps_props: HashMap<String, PropVal> = HashMap::new();
    let mut ps_status: Option<u16> = None;
    let mut cur_prop: Option<(String, PropVal)> = None;
    loop {
        let ev = rd
            .read_event()
            .map_err(|e| AppError::new("E_UPSTREAM", format!("bad XML from server: {e}")))?;
        match ev {
            XEvent::Start(ref e) | XEvent::Empty(ref e) => {
                let is_empty = matches!(ev, XEvent::Empty(_));
                let name = local(e.name().as_ref());
                let depth_in_prop = path.iter().rposition(|p| p == "prop");
                match name.as_str() {
                    "response" if !is_empty => cur = Some(DavResponse::default()),
                    "propstat" if !is_empty => {
                        ps_props.clear();
                        ps_status = None;
                    }
                    _ => {}
                }
                if let Some(pi) = depth_in_prop {
                    if path.len() == pi + 1 {
                        cur_prop = Some((name.clone(), PropVal::default()));
                    } else if let Some((_, pv)) = cur_prop.as_mut() {
                        if path.len() == pi + 2 {
                            pv.children.push(name.clone());
                        }
                        if name == "href" {
                            pv.hrefs.push(String::new());
                        }
                        if name == "comp" {
                            for a in e.attributes().flatten() {
                                if local(a.key.as_ref()) == "name" {
                                    pv.comps.push(String::from_utf8_lossy(&a.value).to_string());
                                }
                            }
                        }
                    }
                }
                if is_empty {
                    if let Some(pi) = depth_in_prop
                        && path.len() == pi + 1
                        && let Some((n, pv)) = cur_prop.take()
                    {
                        ps_props.insert(n, pv);
                    }
                } else {
                    path.push(name);
                }
            }
            XEvent::Text(t) => {
                let s = t
                    .unescape()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&t).to_string());
                push_text(&path, &mut cur, &mut cur_prop, &mut ps_status, &s);
            }
            XEvent::CData(t) => {
                let s = String::from_utf8_lossy(&t.into_inner()).to_string();
                push_text(&path, &mut cur, &mut cur_prop, &mut ps_status, &s);
            }
            XEvent::End(e) => {
                let name = local(e.name().as_ref());
                path.pop();
                let in_prop = path.iter().rposition(|p| p == "prop");
                if let Some(pi) = in_prop
                    && path.len() == pi + 1
                    && let Some((n, mut pv)) = cur_prop.take()
                {
                    pv.hrefs = pv
                        .hrefs
                        .iter()
                        .map(|h| h.trim().to_string())
                        .filter(|h| !h.is_empty())
                        .collect();
                    ps_props.insert(n, pv);
                }
                match name.as_str() {
                    "propstat" => {
                        if let Some(r) = cur.as_mut()
                            && ps_status.map(|s| (200..300).contains(&s)).unwrap_or(true)
                        {
                            r.props.extend(ps_props.drain());
                        }
                    }
                    "response" => {
                        if let Some(mut r) = cur.take() {
                            r.href = r.href.trim().to_string();
                            out.push(r);
                        }
                    }
                    _ => {}
                }
            }
            XEvent::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn push_text(
    path: &[String],
    cur: &mut Option<DavResponse>,
    cur_prop: &mut Option<(String, PropVal)>,
    ps_status: &mut Option<u16>,
    s: &str,
) {
    let last = path.last().map(|s| s.as_str()).unwrap_or("");
    if let Some((_, pv)) = cur_prop.as_mut() {
        if last == "href"
            && let Some(h) = pv.hrefs.last_mut()
        {
            h.push_str(s);
        }
        pv.text.push_str(s);
        return;
    }
    let in_propstat = path.iter().any(|p| p == "propstat");
    match last {
        "href" if !in_propstat => {
            if let Some(r) = cur.as_mut() {
                r.href.push_str(s)
            }
        }
        "status" => {
            let code = s.split_whitespace().nth(1).and_then(|c| c.parse().ok());
            if in_propstat {
                *ps_status = code;
            } else if let Some(r) = cur.as_mut() {
                r.status = code;
            }
        }
        _ => {}
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn decode_href(h: &str) -> String {
    percent_encoding::percent_decode_str(h)
        .decode_utf8_lossy()
        .to_string()
}

pub fn last_segment(href: &str) -> String {
    decode_href(href.trim_end_matches('/').rsplit('/').next().unwrap_or(""))
}

/// Discover principal + home set (calendar-home-set or addressbook-home-set).
pub async fn discover_home(dav: &Dav, base: &Url, home_prop: &str) -> Result<Url> {
    let (u, rs) = dav
        .propfind(base, "0", "<d:current-user-principal/>")
        .await?;
    let p = rs
        .iter()
        .find_map(|r| {
            r.props
                .get("current-user-principal")
                .and_then(|p| p.hrefs.first().cloned())
        })
        .ok_or_else(|| {
            AppError::new("E_UPSTREAM", "server did not return current-user-principal")
        })?;
    let principal = u
        .join(&p)
        .map_err(|e| AppError::new("E_UPSTREAM", e.to_string()))?;
    let prop = if home_prop == "calendar-home-set" {
        "<c:calendar-home-set/>"
    } else {
        "<card:addressbook-home-set/>"
    };
    let (u2, rs) = dav.propfind(&principal, "0", prop).await?;
    let h = rs
        .iter()
        .find_map(|r| {
            r.props
                .get(home_prop)
                .and_then(|p| p.hrefs.first().cloned())
        })
        .ok_or_else(|| AppError::new("E_UPSTREAM", format!("server did not return {home_prop}")))?;
    let mut home = u2
        .join(&h)
        .map_err(|e| AppError::new("E_UPSTREAM", e.to_string()))?;
    if !home.path().ends_with('/') {
        home.set_path(&format!("{}/", home.path()));
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_icloud_style_multistatus() {
        let xml = r#"<?xml version="1.0"?><multistatus xmlns="DAV:"><response><href>/123/calendars/home/</href><propstat><prop><displayname>工作 &amp; 生活</displayname><resourcetype><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype><supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name="VEVENT"/></supported-calendar-component-set><getctag xmlns="http://calendarserver.org/ns/">FT=-@RU=a</getctag></prop><status>HTTP/1.1 200 OK</status></propstat><propstat><prop><calendar-color xmlns="http://apple.com/ns/ical/"/></prop><status>HTTP/1.1 404 Not Found</status></propstat></response><response><href>/123/calendars/gone.ics</href><status>HTTP/1.1 404 Not Found</status></response></multistatus>"#;
        let rs = parse_multistatus(xml).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].text("displayname"), Some("工作 & 生活"));
        assert!(
            rs[0].props["resourcetype"]
                .children
                .contains(&"calendar".to_string())
        );
        assert_eq!(
            rs[0].props["supported-calendar-component-set"].comps,
            vec!["VEVENT"]
        );
        assert!(!rs[0].props.contains_key("calendar-color"));
        assert_eq!(rs[1].status, Some(404));
    }
    #[test]
    fn principal_href() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/1234/principal/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let rs = parse_multistatus(xml).unwrap();
        assert_eq!(
            rs[0].props["current-user-principal"].hrefs,
            vec!["/1234/principal/"]
        );
    }
}
