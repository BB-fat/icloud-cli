# Changelog

## v0.1.0 — 2026-10-15

First release of `icloud`, a command line for iCloud built for AI agents. It signs in with an app-specific password only.

### Calendar (read/write) — verified on a real iCloud account
- `cal calendars list|create|delete`, `cal events list|get|create|update|delete|sync|search`, `cal free`
- Recurring events are expanded within the requested window, including EXDATEs and RECURRENCE-ID overrides. All-day events report a `last_day`.
- `--idempotency-key` makes creates safe to retry, updates check the etag, and `sync` returns incremental changes (RFC 6578)

### Contacts (read-only) — verified on a real iCloud account
- `contacts books list`, `contacts list|get|search` (search also matches phone digits)

### Mail (read-only) — **not yet verified against real iCloud**
- `mail folders list`, `mail list|search|get`, `mail attachment get`. It opens folders read-only, so reading doesn't mark messages as seen, and message bodies are marked `untrusted`.
- Tested against GreenMail only. Outbound port 993 was blocked in the release test environment, so the IMAP host (default `imap.mail.me.com`) is still unconfirmed, especially for China-mainland accounts.

### Agent-first
- JSON by default, with a versioned envelope (`icloud.<cmd>/v1`), structured errors (`code`, `hint`, `retryable`) and stable exit codes
- `--fields`, `--limit`/`--cursor`, `--quiet`, `--output table|ndjson`
- `--dry-run` for every write, `--yes` required for deletes, and read-only mode (`--read-only`, `ICLOUD_READ_ONLY`, or the profile)
- Self-describing: `icloud help --json`, `icloud schema <cmd>`, and `icloud skill` prints the embedded SKILL.md
- Accepts only ISO 8601 and relative times; natural language is rejected on purpose

### Credentials & regions
- Credentials can come from the OS keychain, an env var, a `password_command`, or a file (opt-in, must be 0600). It never prompts.
- China-mainland (云上贵州) accounts are auto-detected and use the `.com.cn` endpoints

### Distribution
- Prebuilt binaries for macOS (x64, arm64), Linux (x64, arm64; musl and gnu) and Windows (x64, arm64), each with a SHA-256 file, plus `icloud-cli-installer.sh` / `.ps1`
