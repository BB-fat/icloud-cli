//! icloud — agent-first CLI for iCloud Calendar, Contacts and Mail via app-specific passwords.
#![allow(clippy::result_large_err, clippy::too_many_arguments)]
mod cal;
mod config;
mod contacts;
mod creds;
mod ctx;
mod dav;
mod error;
mod event;
mod ical;
mod mail;
mod meta;
mod output;
mod timeexpr;

use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use ctx::Ctx;
use error::{AppError, Result};
use output::{Format, Out};
use serde_json::{Value, json};
use std::io::Read;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "icloud",
    version,
    about = "Agent-first CLI for iCloud Calendar (read/write), Contacts and Mail (read-only). JSON output by default.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    g: Global,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Debug)]
pub struct Global {
    /// Profile (account) to use
    #[arg(long, global = true, env = "ICLOUD_PROFILE")]
    profile: Option<String>,
    /// Output format
    #[arg(
        long,
        global = true,
        env = "ICLOUD_OUTPUT",
        value_enum,
        default_value = "json"
    )]
    output: OutFmt,
    /// Comma-separated fields to keep in `data` (dot paths allowed)
    #[arg(long, global = true, value_delimiter = ',')]
    fields: Option<Vec<String>>,
    /// Page size
    #[arg(long, global = true, default_value_t = 50)]
    limit: usize,
    /// Cursor from `page.next_cursor`
    #[arg(long, global = true)]
    cursor: Option<String>,
    /// IANA time zone for input and output (default: system)
    #[arg(long, global = true, env = "ICLOUD_TZ")]
    tz: Option<String>,
    /// Refuse every write (also ICLOUD_READ_ONLY=1 or profile read_only)
    #[arg(long, global = true)]
    read_only: bool,
    /// Show what a write would do without doing it
    #[arg(long, global = true)]
    dry_run: bool,
    /// Credential backend override: keychain, env, password_command, file
    #[arg(long, global = true, env = "ICLOUD_CREDENTIAL_BACKEND")]
    credential_backend: Option<String>,
    /// Network timeout, e.g. 30s
    #[arg(long, global = true, env = "ICLOUD_TIMEOUT", default_value = "30s")]
    timeout: String,
    /// Print only `data` (no envelope)
    #[arg(long, global = true)]
    quiet: bool,
    /// Accepted for forward compatibility (no cache in v0.1)
    #[arg(long, global = true, hide = true)]
    no_cache: bool,
    /// Only use the local cache (not available in v0.1)
    #[arg(long, global = true, hide = true)]
    offline: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum OutFmt {
    Json,
    Ndjson,
    Table,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Log in, check and remove credentials
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Manage profiles (non-secret settings)
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Calendar (CalDAV): read and write
    #[command(subcommand)]
    Cal(cal::CalCmd),
    /// Contacts (CardDAV): read-only in v0.1
    #[command(subcommand)]
    Contacts(contacts::ContactsCmd),
    /// Mail (IMAP): read-only in v0.1
    #[command(subcommand)]
    Mail(mail::MailCmd),
    /// Print the embedded SKILL.md (matches this binary's version)
    Skill,
    /// Describe a command's inputs and output schema id, e.g. `icloud schema cal.events.create`
    Schema { command: Option<String> },
    /// Help; `icloud help --json` prints the full command tree as JSON
    Help {
        #[arg(long)]
        json: bool,
        command: Vec<String>,
    },
    /// Version info
    Version,
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Store credentials and verify them against CalDAV (never prompts)
    Login {
        #[arg(long)]
        apple_id: String,
        /// Read the app-specific password from stdin
        #[arg(long)]
        password_stdin: bool,
        /// Read the app-specific password from this environment variable
        #[arg(long)]
        password_env: Option<String>,
        /// auto probes global then China (云上贵州)
        #[arg(long, default_value = "auto", value_parser = ["auto", "global", "cn"])]
        region: String,
        /// Path for --credential-backend file
        #[arg(long)]
        credential_file: Option<String>,
        /// Shell command printing the password (for --credential-backend password_command)
        #[arg(long)]
        password_command: Option<String>,
        /// Skip the online verification
        #[arg(long)]
        no_verify: bool,
    },
    /// Show account, backend, region and verify credentials
    Status {
        #[arg(long)]
        no_verify: bool,
    },
    /// Remove stored credentials for the profile
    Logout,
}

#[derive(Subcommand, Debug)]
enum ProfileCmd {
    List,
    Show,
    /// Make a profile the default
    Use {
        name: String,
    },
    /// Set a key, e.g. `profile set read_only true`, `profile set credential.backend env`
    Set {
        key: String,
        value: String,
    },
}

fn build_ctx(g: &Global) -> Result<Ctx> {
    let cfg = Config::load()?;
    let profile_name = cfg.profile_name(g.profile.as_deref());
    let profile = cfg.profiles.get(&profile_name).cloned().unwrap_or_default();
    let tz_name = g.tz.clone().or_else(|| profile.tz.clone());
    let tz = timeexpr::resolve_tz(tz_name.as_deref())?;
    let timeout = timeexpr::parse_duration(&g.timeout)
        .ok()
        .filter(|s| *s > 0)
        .ok_or_else(|| AppError::usage("--timeout must look like 30s or 2m"))?;
    if g.offline {
        return Err(AppError::new(
            "E_UNSUPPORTED",
            "--offline needs the local cache, planned for v0.2",
        ));
    }
    Ok(Ctx {
        tz_name: tz.iana_name().unwrap_or("UTC").to_string(),
        cfg,
        profile_name,
        profile,
        tz,
        format: match g.output {
            OutFmt::Json => Format::Json,
            OutFmt::Ndjson => Format::Ndjson,
            OutFmt::Table => Format::Table,
        },
        fields: g.fields.clone(),
        limit: g.limit.max(1),
        cursor: g.cursor.clone(),
        dry_run: g.dry_run,
        read_only_flag: g.read_only,
        credential_backend: g.credential_backend.clone(),
        timeout: Duration::from_secs(timeout as u64),
        quiet: g.quiet,
        started: Instant::now(),
        apple_id_override: None,
        password_override: None,
    })
}

fn read_secret(stdin: bool, env: &Option<String>) -> Result<Option<String>> {
    if stdin {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| AppError::usage(format!("cannot read stdin: {e}")))?;
        let s = s.lines().next().unwrap_or("").trim().to_string();
        if s.is_empty() {
            return Err(AppError::usage("--password-stdin got an empty password"));
        }
        return Ok(Some(s));
    }
    if let Some(v) = env {
        let s = std::env::var(v)
            .map_err(|_| AppError::usage(format!("environment variable {v} is not set")))?;
        return Ok(Some(s.trim().to_string()));
    }
    Ok(None)
}

async fn verify(ctx: &Ctx) -> Result<Value> {
    let c = cal::Cal::connect(ctx).await?;
    let n = c.event_calendars().await?.len();
    Ok(
        json!({ "caldav": { "ok": true, "endpoint": ctx.endpoint("caldav")?.as_str(), "calendar_home": c.home.as_str(), "calendars": n } }),
    )
}

async fn auth(ctx: &mut Ctx, cmd: &AuthCmd) -> Result<Out> {
    match cmd {
        AuthCmd::Login {
            apple_id,
            password_stdin,
            password_env,
            region,
            credential_file,
            password_command,
            no_verify,
        } => {
            let mut prof = ctx.profile.clone();
            prof.apple_id = Some(apple_id.clone());
            if let Some(f) = credential_file {
                prof.credential.file = Some(f.clone());
            }
            if let Some(p) = password_command {
                prof.credential.password_command = Some(p.clone());
            }
            let secret = read_secret(*password_stdin, password_env)?;
            let backend = match ctx.credential_backend.clone() {
                Some(b) => b,
                None if password_command.is_some() => "password_command".into(),
                None if credential_file.is_some() => "file".into(),
                None => {
                    // auto: prefer keychain if it works on this machine
                    let probe =
                        keyring::Entry::new("icloud-cli", "__probe__").and_then(|e| {
                            match e.get_password() {
                                Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
                                Err(x) => Err(x),
                            }
                        });
                    if probe.is_ok() {
                        "keychain".into()
                    } else {
                        return Err(AppError::new("E_AUTH_MISSING", "no OS keychain available on this machine")
                            .hint("Pick a backend explicitly: --credential-backend env (export ICLOUD_APP_PASSWORD), --credential-backend password_command --password-command '<cmd>', or --credential-backend file --credential-file ~/.config/icloud-cli/secret"));
                    }
                }
            };
            if !creds::BACKENDS.contains(&backend.as_str()) {
                return Err(AppError::usage(format!(
                    "unknown credential backend `{backend}`"
                )));
            }
            let needs_secret = matches!(backend.as_str(), "keychain" | "file");
            let pw = match (&secret, backend.as_str()) {
                (Some(s), _) => s.clone(),
                (None, _) if needs_secret => {
                    return Err(AppError::usage(format!(
                        "backend `{backend}` needs the password via --password-stdin or --password-env VAR"
                    )));
                }
                (None, _) => {
                    prof.credential.backend = Some(backend.clone());
                    creds::read_password(Some(&backend), &prof, &ctx.profile_name, apple_id)
                        .await?
                        .0
                }
            };
            ctx.apple_id_override = Some(apple_id.clone());
            ctx.profile = prof.clone();
            // verify with the secret in memory before persisting anything
            ctx.password_override = Some(pw.clone());
            let mut detected = region.clone();
            let mut verification = Value::Null;
            if !no_verify {
                let order: Vec<&str> = match region.as_str() {
                    "auto" => vec!["global", "cn"],
                    r => vec![r],
                };
                let mut last_err = None;
                for r in order {
                    ctx.profile.region = Some(r.into());
                    match verify(ctx).await {
                        Ok(v) => {
                            // China-mainland accounts can authenticate on the global endpoint but live on
                            // pXX-*.icloud.com.cn shards; their CardDAV/IMAP must then use the .cn endpoints.
                            let on_cn = v["caldav"]["calendar_home"]
                                .as_str()
                                .and_then(|h| reqwest::Url::parse(h).ok())
                                .and_then(|u| u.host_str().map(|h| h.ends_with(".com.cn")))
                                .unwrap_or(false);
                            detected = if on_cn && region == "auto" {
                                "cn".into()
                            } else {
                                r.into()
                            };
                            verification = v;
                            last_err = None;
                            break;
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
                if let Some(e) = last_err {
                    return Err(e);
                }
            } else if detected == "auto" {
                detected = "global".into();
            }
            let note = creds::store_password(&backend, &prof, apple_id, &pw)?;
            prof.region = Some(detected.clone());
            prof.credential.backend = Some(backend.clone());
            let mut cfg = ctx.cfg.clone();
            cfg.profiles.insert(ctx.profile_name.clone(), prof);
            if cfg.current.is_none() {
                cfg.current = Some(ctx.profile_name.clone());
            }
            cfg.save()?;
            Ok(Out::new(
                "auth.login",
                json!({ "profile": ctx.profile_name, "apple_id": apple_id, "region": detected, "credential_backend": backend, "credential": note, "password": creds::mask(&pw), "verified": !no_verify, "services": verification, "config": config::config_path() }),
            ))
        }
        AuthCmd::Status { no_verify } => {
            let id = ctx.apple_id()?;
            let backend = ctx.backend();
            let pw = creds::read_password(backend.as_deref(), &ctx.profile, &ctx.profile_name, &id)
                .await?;
            let mut data = json!({ "profile": ctx.profile_name, "apple_id": id, "region": ctx.region(), "credential_backend": pw.1, "password": creds::mask(&pw.0), "read_only": ctx.read_only(), "endpoints": { "caldav": ctx.endpoint("caldav")?.as_str(), "carddav": ctx.endpoint("carddav")?.as_str(), "imap": ctx.endpoint("imap")?.as_str() }, "config": config::config_path() });
            if !no_verify {
                data["services"] = verify(ctx).await?;
            }
            Ok(Out::new("auth.status", data))
        }
        AuthCmd::Logout => {
            let id = ctx.apple_id()?;
            let backend = ctx.backend().unwrap_or_else(|| "keychain".into());
            ctx.guard_write("auth logout").ok();
            let note = creds::delete_password(&backend, &ctx.profile, &id)?;
            let mut cfg = ctx.cfg.clone();
            if let Some(p) = cfg.profiles.get_mut(&ctx.profile_name) {
                p.credential.backend = None;
            }
            cfg.save()?;
            Ok(Out::new(
                "auth.logout",
                json!({ "profile": ctx.profile_name, "apple_id": id, "credential": note }),
            ))
        }
    }
}

fn profile(ctx: &Ctx, cmd: &ProfileCmd) -> Result<Out> {
    let mut cfg = ctx.cfg.clone();
    match cmd {
        ProfileCmd::List => {
            let cur = cfg.profile_name(None);
            let v: Vec<Value> = cfg.profiles.iter().map(|(n, p)| json!({ "name": n, "apple_id": p.apple_id, "region": p.region, "current": *n == cur })).collect();
            Ok(Out::new("profile.list", json!(v)))
        }
        ProfileCmd::Show => Ok(Out::new(
            "profile.show",
            json!({ "name": ctx.profile_name, "settings": serde_json::to_value(&ctx.profile).unwrap(), "config": config::config_path() }),
        )),
        ProfileCmd::Use { name } => {
            if !cfg.profiles.contains_key(name) {
                return Err(AppError::not_found(format!("no profile `{name}`"))
                    .hint("Create it with `icloud --profile <name> auth login ...`."));
            }
            cfg.current = Some(name.clone());
            cfg.save()?;
            Ok(Out::new("profile.use", json!({ "current": name })))
        }
        ProfileCmd::Set { key, value } => {
            let p = cfg.profiles.entry(ctx.profile_name.clone()).or_default();
            config::set_key(p, key, value)?;
            let settings = serde_json::to_value(&*p).unwrap();
            cfg.save()?;
            Ok(Out::new(
                "profile.set",
                json!({ "profile": ctx.profile_name, "key": key, "settings": settings }),
            ))
        }
    }
}

async fn dispatch(cli: &Cli) -> Result<Out> {
    match &cli.cmd {
        Cmd::Skill => unreachable!(),
        Cmd::Version => Ok(Out::new(
            "version",
            json!({ "name": "icloud-cli", "version": env!("CARGO_PKG_VERSION"), "target": meta::TARGET, "schema_version": 1 }),
        )),
        Cmd::Help { command, .. } => Ok(Out::new("help", meta::help_json(command))),
        Cmd::Schema { command } => Ok(Out::new("schema", meta::schema(command.as_deref())?)),
        _ => {
            let mut ctx = build_ctx(&cli.g)?;
            match &cli.cmd {
                Cmd::Auth(a) => auth(&mut ctx, a).await,
                Cmd::Profile(p) => profile(&ctx, p),
                Cmd::Cal(c) => cal::run(&ctx, c).await,
                Cmd::Contacts(c) => contacts::run(&ctx, c).await,
                Cmd::Mail(m) => mail::run(&ctx, m).await,
                _ => unreachable!(),
            }
        }
    }
}

fn emit(out: Out, g: &Global, started: Instant, profile: &str) {
    let fmt = match g.output {
        OutFmt::Json => Format::Json,
        OutFmt::Ndjson => Format::Ndjson,
        OutFmt::Table => Format::Table,
    };
    let mut data = output::strip_nulls(out.data);
    if let Some(f) = &g.fields {
        data = match data {
            // write/sync commands: --fields filters the nested event(s), keeping action/ids/tokens
            Value::Object(mut m)
                if ["event", "changed"].iter().any(|k| m.contains_key(*k))
                    && !f
                        .iter()
                        .any(|x| x.starts_with("event") || x.starts_with("changed")) =>
            {
                for k in ["event", "changed"] {
                    if let Some(v) = m.get(k) {
                        let nv = output::select_fields(v, f);
                        m.insert(k.into(), nv);
                    }
                }
                Value::Object(m)
            }
            other => output::select_fields(&other, f),
        };
    }
    match fmt {
        Format::Table => {
            print!("{}", output::table(&data));
        }
        Format::Ndjson => match &data {
            Value::Array(a) => {
                for x in a {
                    println!("{x}");
                }
            }
            other => println!("{other}"),
        },
        Format::Json => {
            if g.quiet {
                println!("{}", serde_json::to_string_pretty(&data).unwrap());
                return;
            }
            let mut env = json!({ "schema": out.schema, "ok": true });
            if out.dry_run {
                env["dry_run"] = json!(true);
            }
            env["data"] = data;
            if let Some(p) = out.page {
                env["page"] = p;
            }
            env["meta"] = json!({ "profile": profile, "elapsed_ms": started.elapsed().as_millis() as u64, "version": env!("CARGO_PKG_VERSION") });
            for (k, v) in out.extra {
                env[k] = v;
            }
            println!("{}", serde_json::to_string_pretty(&env).unwrap());
        }
    }
}

fn main() {
    let started = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let human = argv
        .windows(2)
        .any(|w| w[0] == "--output" && w[1] == "table")
        || argv.iter().any(|a| a == "--output=table")
        || std::env::var("ICLOUD_OUTPUT")
            .map(|v| v == "table")
            .unwrap_or(false);
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                let _ = e.print();
                std::process::exit(
                    if e.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
                        2
                    } else {
                        0
                    },
                );
            }
            let err = AppError::usage(
                e.render()
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("invalid usage")
                    .trim_start_matches("error: ")
                    .to_string(),
            )
            .hint("Run `icloud help --json` or `icloud <command> --help` for valid arguments.")
            .details(json!({ "clap": e.render().to_string() }));
            fail(err, human);
        }
    };
    if let Cmd::Skill = cli.cmd {
        print!("{}", include_str!("../skill/SKILL.md"));
        return;
    }
    if let Cmd::Help {
        json: false,
        command,
    } = &cli.cmd
    {
        let mut cur = <Cli as clap::CommandFactory>::command();
        cur.build();
        for part in command {
            match cur.find_subcommand(part) {
                Some(s) => cur = s.clone(),
                None => break,
            }
        }
        let _ = cur.print_long_help();
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let profile = Config::load()
        .map(|c| c.profile_name(cli.g.profile.as_deref()))
        .unwrap_or_else(|_| "default".into());
    match rt.block_on(dispatch(&cli)) {
        Ok(out) => emit(out, &cli.g, started, &profile),
        Err(e) => fail(e, human),
    }
}

fn fail(e: AppError, human: bool) -> ! {
    if human {
        eprintln!("error: {e}");
    } else {
        println!("{}", serde_json::to_string_pretty(&e.to_json()).unwrap());
    }
    std::process::exit(e.exit_code());
}
