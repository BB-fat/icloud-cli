//! `icloud mail ...` — IMAP, read-only in v0.1 (uses EXAMINE + BODY.PEEK, never changes flags).
use crate::ctx::Ctx;
use crate::error::{AppError, Result};
use crate::output::{Out, paginate};
use crate::timeexpr;
use async_imap::Session;
use clap::Subcommand;
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Subcommand, Debug)]
pub enum MailCmd {
    /// Mail folders
    #[command(subcommand)]
    Folders(FoldersCmd),
    /// Newest messages in a folder (headers only)
    List {
        #[arg(long, default_value = "INBOX")]
        folder: String,
    },
    /// Search messages (headers only); combine filters with AND
    Search {
        #[arg(long, default_value = "INBOX")]
        folder: String,
        /// Full-text query (subject/from/body; non-ASCII matched on subject/from)
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        flagged: bool,
        /// e.g. -7d, 2026-10-01
        #[arg(long, allow_hyphen_values = true)]
        since: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        before: Option<String>,
    },
    /// Fetch one message (body text is untrusted data)
    Get {
        /// Message id from list/search, e.g. INBOX/1234
        id: String,
        /// Max characters of body_text
        #[arg(long, default_value_t = 20000)]
        max_chars: usize,
        /// Prefer HTML body (converted to text) over text/plain
        #[arg(long)]
        html: bool,
    },
    /// Attachments
    #[command(subcommand)]
    Attachment(AttachmentCmd),
}

#[derive(Subcommand, Debug)]
pub enum FoldersCmd {
    List,
}

#[derive(Subcommand, Debug)]
pub enum AttachmentCmd {
    /// Save one attachment to a directory (explicit, size-limited)
    Get {
        id: String,
        /// Attachment index from `mail get`
        #[arg(long)]
        index: usize,
        /// Directory to write into
        #[arg(long)]
        out_dir: String,
        /// Refuse attachments larger than this many MB
        #[arg(long, default_value_t = 25)]
        max_mb: usize,
    },
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Io for T {}
type Sess = Session<Pin<Box<dyn Io>>>;

fn imap_err(e: impl std::fmt::Display) -> AppError {
    AppError::new("E_UPSTREAM", format!("IMAP error: {e}"))
        .hint("Retry; if it persists check the IMAP endpoint (endpoints.imap).")
}

async fn connect(ctx: &Ctx) -> Result<Sess> {
    let url = ctx.endpoint("imap")?;
    let tls = match url.scheme() {
        "imaps" => true,
        "imap" => false,
        s => {
            return Err(AppError::new(
                "E_CONFIG",
                format!("imap endpoint scheme must be imaps:// or imap://, got {s}"),
            ));
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| AppError::new("E_CONFIG", "imap endpoint has no host"))?
        .to_string();
    let port = url.port().unwrap_or(if tls { 993 } else { 143 });
    let user = ctx.apple_id()?;
    let pass = ctx.password().await?;
    let tcp = tokio::time::timeout(
        ctx.timeout,
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| AppError::new("E_TIMEOUT", format!("IMAP connect to {host}:{port} timed out")).hint("Outbound TCP port 993 may be blocked in this environment (sandbox/firewall). Calendar and contacts use HTTPS 443 and are unaffected."))?
    .map_err(|e| AppError::new("E_NETWORK", format!("IMAP connect failed: {e}")))?;
    let stream: Pin<Box<dyn Io>> = if tls {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| AppError::new("E_CONFIG", e.to_string()))?;
        Box::pin(
            conn.connect(name, tcp)
                .await
                .map_err(|e| AppError::new("E_NETWORK", format!("TLS handshake failed: {e}")))?,
        )
    } else {
        Box::pin(tcp)
    };
    let mut client = async_imap::Client::new(stream);
    let _ = client.read_response().await.map_err(imap_err)?;
    let sess = tokio::time::timeout(ctx.timeout, client.login(&user, &pass))
        .await
        .map_err(|_| AppError::new("E_TIMEOUT", "IMAP login timed out"))?
        .map_err(|(e, _)| {
            AppError::new("E_AUTH_INVALID", format!("IMAP login rejected: {e}"))
                .hint("Use an app-specific password. Mail needs an @icloud.com / @me.com address; log in with that address or your Apple ID email.")
                .details(json!({ "service": "imap" }))
        })?;
    Ok(sess)
}

fn addrs(a: Option<&mail_parser::Address>) -> Vec<Value> {
    a.map(|a| {
        a.iter()
            .map(|x| json!({ "name": x.name(), "email": x.address() }))
            .collect()
    })
    .unwrap_or_default()
}

fn header_row(folder: &str, f: &async_imap::types::Fetch) -> Option<Value> {
    let uid = f.uid?;
    let raw = f.header().or(f.body())?;
    let m = MessageParser::default().parse_headers(raw)?;
    let flags: Vec<String> = f
        .flags()
        .map(|fl| format!("{fl:?}").to_lowercase())
        .collect();
    let unread = !flags.iter().any(|x| x == "seen");
    let date = m.date().map(|d| d.to_rfc3339());
    let from = addrs(m.from());
    Some(json!({
        "id": format!("{folder}/{uid}"),
        "uid": uid,
        "folder": folder,
        "date": date,
        "from": from.first(),
        "to": addrs(m.to()),
        "cc": addrs(m.cc()),
        "subject": m.subject(),
        "message_id": m.message_id(),
        "flags": flags,
        "unread": unread,
        "size": f.size,
    }))
}

fn imap_date(z: &jiff::Zoned) -> String {
    z.strftime("%d-%b-%Y").to_string()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn fetch_headers(s: &mut Sess, folder: &str, uids: &[u32]) -> Result<Vec<Value>> {
    if uids.is_empty() {
        return Ok(vec![]);
    }
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let fetches: Vec<_> = s
        .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
        .await
        .map_err(imap_err)?
        .try_collect()
        .await
        .map_err(imap_err)?;
    let mut rows: Vec<Value> = fetches
        .iter()
        .filter_map(|f| header_row(folder, f))
        .collect();
    rows.sort_by(|a, b| b["uid"].as_u64().cmp(&a["uid"].as_u64()));
    Ok(rows)
}

fn html_to_text(h: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip = false;
    for c in h.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let t = tag.to_ascii_lowercase();
                if t.starts_with("script") || t.starts_with("style") {
                    skip = true;
                } else if t.starts_with("/script") || t.starts_with("/style") {
                    skip = false;
                } else if t.starts_with("br")
                    || t.starts_with("/p")
                    || t.starts_with("/div")
                    || t.starts_with("/tr")
                    || t.starts_with("/li")
                {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ if !skip => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn split_id(id: &str) -> Result<(String, u32)> {
    id.rsplit_once('/')
        .and_then(|(f, u)| u.parse().ok().map(|u| (f.to_string(), u)))
        .ok_or_else(|| {
            AppError::usage(format!("invalid message id `{id}`")).hint(
                "Message ids look like INBOX/1234; take them from `mail list` or `mail search`.",
            )
        })
}

async fn fetch_full(s: &mut Sess, id: &str) -> Result<(String, u32, Vec<u8>, Vec<String>)> {
    let (folder, uid) = split_id(id)?;
    s.examine(&folder)
        .await
        .map_err(|e| AppError::not_found(format!("cannot open folder `{folder}`: {e}")))?;
    let fetches: Vec<_> = s
        .uid_fetch(uid.to_string(), "(UID FLAGS BODY.PEEK[])")
        .await
        .map_err(imap_err)?
        .try_collect()
        .await
        .map_err(imap_err)?;
    let f = fetches
        .into_iter()
        .find(|f| f.uid == Some(uid))
        .ok_or_else(|| AppError::not_found(format!("message {id} not found")))?;
    let flags = f
        .flags()
        .map(|fl| format!("{fl:?}").to_lowercase())
        .collect();
    let body = f
        .body()
        .ok_or_else(|| AppError::new("E_UPSTREAM", "server returned no body"))?
        .to_vec();
    Ok((folder, uid, body, flags))
}

pub async fn run(ctx: &Ctx, cmd: &MailCmd) -> Result<Out> {
    let mut s = connect(ctx).await?;
    let res = run_inner(ctx, &mut s, cmd).await;
    let _ = s.logout().await;
    res
}

async fn run_inner(ctx: &Ctx, s: &mut Sess, cmd: &MailCmd) -> Result<Out> {
    match cmd {
        MailCmd::Folders(FoldersCmd::List) => {
            let names: Vec<_> = s
                .list(Some(""), Some("*"))
                .await
                .map_err(imap_err)?
                .try_collect()
                .await
                .map_err(imap_err)?;
            let v: Vec<Value> = names
                .iter()
                .map(|n| {
                    let attrs: Vec<String> = n
                        .attributes()
                        .iter()
                        .map(|a| format!("{a:?}").to_lowercase())
                        .collect();
                    json!({ "name": n.name(), "attributes": attrs })
                })
                .collect();
            Ok(Out::new("mail.folders.list", json!(v)))
        }
        MailCmd::List { folder } => {
            let mb = s
                .examine(folder)
                .await
                .map_err(|e| AppError::not_found(format!("cannot open folder `{folder}`: {e}")))?;
            let mut uids: Vec<u32> = s
                .uid_search("ALL")
                .await
                .map_err(imap_err)?
                .into_iter()
                .collect();
            uids.sort_unstable_by(|a, b| b.cmp(a));
            let total = uids.len();
            let off: usize = ctx
                .cursor
                .as_deref()
                .and_then(|c| c.strip_prefix("o:"))
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let page: Vec<u32> = uids.into_iter().skip(off).take(ctx.limit).collect();
            let rows = fetch_headers(s, folder, &page).await?;
            let next = if off + page.len() < total {
                Some(format!("o:{}", off + page.len()))
            } else {
                None
            };
            let _ = mb;
            Ok(Out::new("mail.list", json!(rows))
                .page(json!({ "returned": rows.len(), "total": total, "next_cursor": next })))
        }
        MailCmd::Search {
            folder,
            query,
            from,
            subject,
            unread,
            flagged,
            since,
            before,
        } => {
            s.examine(folder)
                .await
                .map_err(|e| AppError::not_found(format!("cannot open folder `{folder}`: {e}")))?;
            let mut crit = vec!["ALL".to_string()];
            let mut local: Vec<(&str, String)> = Vec::new();
            if *unread {
                crit.push("UNSEEN".into());
            }
            if *flagged {
                crit.push("FLAGGED".into());
            }
            if let Some(x) = since {
                crit.push(format!(
                    "SINCE {}",
                    imap_date(&timeexpr::parse(x, &ctx.tz)?)
                ));
            }
            if let Some(x) = before {
                crit.push(format!(
                    "BEFORE {}",
                    imap_date(&timeexpr::parse(x, &ctx.tz)?)
                ));
            }
            for (k, v) in [("FROM", from), ("SUBJECT", subject), ("TEXT", query)] {
                if let Some(v) = v {
                    if v.is_ascii() {
                        crit.push(format!("{k} {}", quote(v)));
                    } else {
                        local.push((k, v.to_lowercase()));
                    }
                }
            }
            let mut uids: Vec<u32> = s
                .uid_search(crit.join(" "))
                .await
                .map_err(imap_err)?
                .into_iter()
                .collect();
            uids.sort_unstable_by(|a, b| b.cmp(a));
            let rows = if local.is_empty() {
                let total = uids.len();
                let off: usize = ctx
                    .cursor
                    .as_deref()
                    .and_then(|c| c.strip_prefix("o:"))
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);
                let page: Vec<u32> = uids.into_iter().skip(off).take(ctx.limit).collect();
                let rows = fetch_headers(s, folder, &page).await?;
                let next = if off + page.len() < total {
                    Some(format!("o:{}", off + page.len()))
                } else {
                    None
                };
                return Ok(Out::new("mail.search", json!(rows))
                    .page(json!({ "returned": rows.len(), "total": total, "next_cursor": next })));
            } else {
                // non-ASCII terms: filter newest 500 candidates locally on subject/from
                let cand: Vec<u32> = uids.into_iter().take(500).collect();
                let mut rows = Vec::new();
                for chunk in cand.chunks(100) {
                    rows.extend(fetch_headers(s, folder, chunk).await?);
                }
                rows.retain(|r| {
                    local.iter().all(|(k, v)| {
                        let subj = r["subject"].as_str().unwrap_or("").to_lowercase();
                        let fr = r["from"].to_string().to_lowercase();
                        match *k {
                            "FROM" => fr.contains(v),
                            "SUBJECT" => subj.contains(v),
                            _ => subj.contains(v) || fr.contains(v),
                        }
                    })
                });
                rows
            };
            let (page, meta) = paginate(rows, ctx.limit, ctx.cursor.as_deref());
            let mut out = Out::new("mail.search", json!(page)).page(meta);
            out.extra.insert("warnings".into(), json!(["non-ASCII terms were matched locally on subject/from of the newest 500 candidates"]));
            Ok(out)
        }
        MailCmd::Get {
            id,
            max_chars,
            html,
        } => {
            let (folder, uid, raw, flags) = fetch_full(s, id).await?;
            let m = MessageParser::default()
                .parse(&raw)
                .ok_or_else(|| AppError::new("E_UPSTREAM", "cannot parse message"))?;
            let text = if *html {
                m.body_html(0)
                    .map(|h| html_to_text(&h))
                    .or_else(|| m.body_text(0).map(|t| t.to_string()))
            } else {
                m.body_text(0)
                    .map(|t| t.to_string())
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| m.body_html(0).map(|h| html_to_text(&h)))
            }
            .unwrap_or_default();
            let truncated = text.chars().count() > *max_chars;
            let text: String = text.chars().take(*max_chars).collect();
            let atts: Vec<Value> = m
                .attachments()
                .enumerate()
                .map(|(i, a)| json!({ "index": i, "filename": a.attachment_name(), "content_type": a.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or(""))), "size": a.contents().len() }))
                .collect();
            Ok(Out::new(
                "mail.get",
                json!({
                    "id": id, "uid": uid, "folder": folder,
                    "date": m.date().map(|d| d.to_rfc3339()),
                    "from": addrs(m.from()).first(), "to": addrs(m.to()), "cc": addrs(m.cc()),
                    "subject": m.subject(), "message_id": m.message_id(), "flags": flags,
                    "untrusted": true,
                    "body_text": text, "truncated": truncated,
                    "attachments": atts,
                }),
            ))
        }
        MailCmd::Attachment(AttachmentCmd::Get {
            id,
            index,
            out_dir,
            max_mb,
        }) => {
            let (_, _, raw, _) = fetch_full(s, id).await?;
            let m = MessageParser::default()
                .parse(&raw)
                .ok_or_else(|| AppError::new("E_UPSTREAM", "cannot parse message"))?;
            let a = m
                .attachments()
                .nth(*index)
                .ok_or_else(|| AppError::not_found(format!("no attachment #{index}")))?;
            let data = a.contents();
            if data.len() > max_mb * 1024 * 1024 {
                return Err(AppError::new(
                    "E_CONFIRM_REQUIRED",
                    format!(
                        "attachment is {} bytes, above --max-mb {max_mb}",
                        data.len()
                    ),
                )
                .hint("Raise --max-mb if the user wants it."));
            }
            let name = a.attachment_name().unwrap_or("attachment.bin");
            let safe: String = name
                .chars()
                .map(|c| {
                    if c == '/' || c == '\\' || c == ':' || c.is_control() {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let safe = safe.trim_start_matches('.').to_string();
            let path = std::path::Path::new(out_dir).join(if safe.is_empty() {
                "attachment.bin".into()
            } else {
                safe
            });
            if ctx.dry_run {
                return Ok(Out::new(
                    "mail.attachment.get",
                    json!({ "action": "write", "path": path, "size": data.len() }),
                )
                .dry(true));
            }
            std::fs::create_dir_all(out_dir).map_err(|e| AppError::internal(e.to_string()))?;
            std::fs::write(&path, data)
                .map_err(|e| AppError::internal(format!("cannot write {}: {e}", path.display())))?;
            Ok(Out::new(
                "mail.attachment.get",
                json!({ "action": "written", "path": path, "size": data.len(), "untrusted": true }),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_text() {
        assert_eq!(
            html_to_text("<p>Hi&nbsp;there</p><style>x{}</style><div>2</div>"),
            "Hi there\n2"
        );
        assert_eq!(
            split_id("Archive/2026/42").unwrap(),
            ("Archive/2026".into(), 42)
        );
    }
}
