//! Self-description: `help --json` and `schema`.
use crate::error::{AppError, Result};
use clap::CommandFactory;
use serde_json::{Value, json};

pub const TARGET: &str = env!("ICLOUD_TARGET");

const DESTRUCTIVE: &[&str] = &["cal.calendars.delete", "cal.events.delete"];
const WRITES: &[&str] = &[
    "cal.calendars.create",
    "cal.calendars.delete",
    "cal.events.create",
    "cal.events.update",
    "cal.events.delete",
];

fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<Value>) {
    let subs: Vec<&clap::Command> = cmd.get_subcommands().collect();
    if (subs.is_empty() || path.is_empty()) && !path.is_empty() {
        out.push(describe(cmd, path));
    }
    for s in subs {
        let p = if path.is_empty() {
            s.get_name().to_string()
        } else {
            format!("{path}.{}", s.get_name())
        };
        walk(s, &p, out);
    }
}

fn describe(cmd: &clap::Command, path: &str) -> Value {
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !a.is_global_set() && a.get_id() != "help" && a.get_id() != "version")
        .map(|a| {
            json!({
                "name": a.get_long().map(|l| format!("--{l}")).unwrap_or_else(|| a.get_id().to_string()),
                "positional": a.is_positional(),
                "required": a.is_required_set(),
                "repeatable": matches!(a.get_action(), clap::ArgAction::Append),
                "takes_value": a.get_action().takes_values(),
                "default": a.get_default_values().iter().map(|v| v.to_string_lossy().to_string()).collect::<Vec<_>>().first(),
                "help": a.get_help().map(|h| h.to_string()),
            })
        })
        .collect();
    json!({
        "command": path,
        "invocation": format!("icloud {}", path.replace('.', " ")),
        "about": cmd.get_about().map(|s| s.to_string()),
        "args": args,
        "writes": WRITES.contains(&path),
        "destructive": DESTRUCTIVE.contains(&path),
        "requires_confirmation": if DESTRUCTIVE.contains(&path) { Some("--yes") } else { None },
        "supports_dry_run": WRITES.contains(&path),
        "output_schema": format!("icloud.{path}/v1"),
    })
}

pub fn help_json(filter: &[String]) -> Value {
    let root = crate::Cli::command();
    let mut cmds = Vec::new();
    walk(&root, "", &mut cmds);
    let prefix = filter.join(".");
    if !prefix.is_empty() {
        cmds.retain(|c| c["command"].as_str().unwrap().starts_with(&prefix));
    }
    let globals: Vec<Value> = root
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| json!({ "name": format!("--{}", a.get_long().unwrap_or("")), "env": a.get_env().map(|e| e.to_string_lossy().to_string()), "help": a.get_help().map(|h| h.to_string()) }))
        .collect();
    json!({ "name": "icloud", "version": env!("CARGO_PKG_VERSION"), "global_args": globals, "commands": cmds, "exit_codes": exit_codes() })
}

fn exit_codes() -> Value {
    json!({ "0": "success (incl. dry-run)", "1": "E_INTERNAL", "2": "E_USAGE, E_TIME_PARSE, E_CONFIG", "3": "E_AUTH_INVALID, E_AUTH_MISSING", "4": "E_NOT_FOUND", "5": "E_CONFLICT", "6": "E_READ_ONLY, E_CONFIRM_REQUIRED, E_BATCH_LIMIT", "7": "E_NETWORK, E_TIMEOUT (retryable)", "8": "E_RATE_LIMITED, E_UPSTREAM (retryable)", "9": "E_UNSUPPORTED" })
}

fn output_fields(path: &str) -> Value {
    let event = json!([
        "id",
        "uid",
        "calendar",
        "calendar_id",
        "title",
        "start",
        "end",
        "all_day",
        "last_day",
        "location",
        "notes",
        "url",
        "status",
        "busy",
        "recurring",
        "rrule",
        "recurrence_id",
        "alarms",
        "attendees[].email",
        "attendees[].name",
        "attendees[].status",
        "organizer",
        "categories",
        "etag"
    ]);
    match path {
        "cal.events.list" | "cal.events.search" | "cal.events.get" => {
            json!({ "data": if path.ends_with("get") { json!({ "type": "object", "fields": event }) } else { json!({ "type": "array", "items": event }) }, "page": ["returned", "total", "next_cursor"] })
        }
        "cal.events.create" => {
            json!({ "data": { "action": "create|created|noop", "uid": "string", "event": event, "ics_preview": "string (dry-run only)" } })
        }
        "cal.events.update" => {
            json!({ "data": { "action": "update|updated", "changed": "string[]", "event": event, "before": "object (dry-run)", "after": "object (dry-run)" } })
        }
        "cal.events.delete" => {
            json!({ "data": { "action": "delete|deleted", "id": "string", "title": "string" } })
        }
        "cal.events.sync" => {
            json!({ "data": { "changed": event, "deleted": ["id", "calendar_id"], "next_token": "string", "full": "bool" } })
        }
        "cal.free" => json!({ "data": { "type": "array", "items": ["start", "end", "minutes"] } }),
        "cal.calendars.list" => {
            json!({ "data": { "type": "array", "items": ["id", "name", "color", "components", "ctag", "sync_token"] } })
        }
        "contacts.list" | "contacts.search" | "contacts.get" => {
            json!({ "data": ["id", "uid", "name", "first_name", "last_name", "nickname", "org", "title", "emails[].value", "emails[].type", "phones[].value", "phones[].type", "addresses[]", "birthday", "notes", "urls", "etag"] })
        }
        "mail.list" | "mail.search" => {
            json!({ "data": { "type": "array", "items": ["id", "uid", "folder", "date", "from", "to", "cc", "subject", "flags", "unread", "size"] } })
        }
        "mail.get" => {
            json!({ "data": ["id", "uid", "folder", "date", "from", "to", "cc", "subject", "message_id", "flags", "body_text", "untrusted", "truncated", "attachments[].index", "attachments[].filename", "attachments[].content_type", "attachments[].size"] })
        }
        _ => json!({ "data": "object" }),
    }
}

pub fn schema(cmd: Option<&str>) -> Result<Value> {
    let root = crate::Cli::command();
    let mut cmds = Vec::new();
    walk(&root, "", &mut cmds);
    match cmd {
        None => Ok(
            json!({ "commands": cmds.iter().map(|c| c["command"].clone()).collect::<Vec<_>>(), "envelope": envelope(), "error": error_schema() }),
        ),
        Some(name) => {
            let name = name.trim().replace(' ', ".");
            let c = cmds
                .into_iter()
                .find(|c| c["command"] == json!(name))
                .ok_or_else(|| {
                    AppError::not_found(format!("unknown command `{name}`"))
                        .hint("Run `icloud schema` for the list of commands.")
                })?;
            let mut c = c;
            c["output"] = output_fields(&name);
            c["envelope"] = envelope();
            c["error"] = error_schema();
            Ok(c)
        }
    }
}

fn envelope() -> Value {
    json!({ "schema": "icloud.<command>/v1", "ok": "true", "dry_run": "true when --dry-run", "data": "command-specific", "page": { "returned": "int", "total": "int", "next_cursor": "string|null" }, "meta": { "profile": "string", "elapsed_ms": "int", "version": "string" }, "note": "null fields and empty arrays are omitted" })
}

fn error_schema() -> Value {
    json!({ "schema": "icloud.error/v1", "ok": "false", "error": { "code": "E_*", "message": "string", "hint": "string|null", "retryable": "bool", "exit_code": "int", "details": "object|null" }, "exit_codes": exit_codes() })
}
