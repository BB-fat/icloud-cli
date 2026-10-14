# icloud-cli

Agent-first command line for **iCloud Calendar** (read/write), **Contacts** and **Mail** (read-only), using only an
[app-specific password](https://support.apple.com/102654). One static binary, `icloud`, built for personal AI agents
(Claude Code, Codex, Grok Bot, OpenClaw, …) and for humans who like JSON.

- JSON by default, with a versioned envelope (`icloud.<command>/v1`) and structured errors (`code`, `hint`, `retryable`, stable exit codes)
- Never interactive: credentials come from the OS keychain, environment, a `password_command`, or an opt-in 0600 file
- Safe writes: `--dry-run` everywhere, `--yes` for deletes, `--idempotency-key` for retry-safe creates, etag checks, global `--read-only`
- Self-describing: `icloud help --json`, `icloud schema <command>`, and `icloud skill` prints the matching agent skill (SKILL.md)
- Works with China mainland iCloud (云上贵州) endpoints (`--region cn`, auto-detected at login)

> Status: v0.1 in development. Not affiliated with Apple.

## Install

```sh
# macOS / Linux
curl -fsSL https://github.com/BB-fat/icloud-cli/releases/latest/download/icloud-cli-installer.sh | sh
# Windows (PowerShell)
irm https://github.com/BB-fat/icloud-cli/releases/latest/download/icloud-cli-installer.ps1 | iex
```

Binaries: macOS (x64, arm64), Linux (x64, arm64; musl static and gnu), Windows (x64, arm64).

## Log in

Create an app-specific password at account.apple.com → Sign-In and Security → App-Specific Passwords
(China: appleid.apple.com.cn). Then pick what fits the machine:

```sh
# personal computer (OS keychain)
printf %s "$PW" | icloud auth login --apple-id you@icloud.com --password-stdin
# sandbox / CI / container: nothing stored
export ICLOUD_APPLE_ID=you@icloud.com ICLOUD_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx
# secret manager
icloud auth login --apple-id you@icloud.com --credential-backend password_command --password-command "op read op://vault/icloud/password"
# server without keychain (file must be chmod 600)
printf %s "$PW" | icloud auth login --apple-id you@icloud.com --credential-backend file --credential-file ~/.config/icloud-cli/secret --password-stdin

icloud auth status
```

Revoke access any time by deleting the app-specific password on the Apple ID site.

## Examples

```sh
icloud cal events list --from today --to tomorrow --fields title,start,end,location
icloud cal events create --calendar 工作 --title "和老王吃饭" --start 2026-10-16T15:00 --duration 1h \
  --alarm -15m --idempotency-key wang-dinner-1016
icloud cal events update '<id>' --start 2026-10-16T18:00          # keeps the duration
icloud cal events delete '<id>' --yes
icloud cal free --from +1d --to +7d --min 60m --work-hours 09:00-18:00 --weekdays-only
icloud cal events sync --since-token '<token>'                     # changed[] / deleted[] / next_token
icloud contacts search 王 --fields name,phones,emails
icloud mail search --unread --since -1d --fields id,from,subject
icloud mail get INBOX/1234 --max-chars 4000                        # body_text is marked untrusted
icloud --output table cal events list
```

Times are ISO 8601 or relative (`now`, `today`, `+3d`, `-2h`, `+1w/mon`, `eom`). No natural-language parsing, by design.

## Output

```json
{
  "schema": "icloud.cal.events.list/v1",
  "ok": true,
  "data": [{ "id": "home/ABC.ics", "title": "和老王吃饭", "start": "2026-10-16T15:00:00+08:00", "end": "2026-10-16T16:00:00+08:00", "all_day": false }],
  "page": { "returned": 1, "total": 1, "next_cursor": null },
  "meta": { "profile": "default", "elapsed_ms": 412, "version": "0.1.0" }
}
```

| exit | meaning | codes |
|---|---|---|
| 0 | ok (incl. dry-run) | |
| 2 | usage | `E_USAGE`, `E_TIME_PARSE`, `E_CONFIG` |
| 3 | auth | `E_AUTH_INVALID`, `E_AUTH_MISSING` |
| 4 | not found | `E_NOT_FOUND` |
| 5 | conflict | `E_CONFLICT` |
| 6 | policy | `E_READ_ONLY`, `E_CONFIRM_REQUIRED` |
| 7 | network (retryable) | `E_NETWORK`, `E_TIMEOUT` |
| 8 | upstream (retryable) | `E_UPSTREAM`, `E_RATE_LIMITED` |
| 9 | unsupported | `E_UNSUPPORTED` |

## Configuration

`~/.config/icloud-cli/config.toml` (`%APPDATA%\icloud-cli\` on Windows; override with `ICLOUD_CONFIG_DIR`) holds
non-secret settings per profile: `icloud --profile family profile set read_only true`.
Environment overrides: `ICLOUD_PROFILE`, `ICLOUD_APPLE_ID`, `ICLOUD_APP_PASSWORD` / `ICLOUD_<PROFILE>_APP_PASSWORD`,
`ICLOUD_CREDENTIAL_BACKEND`, `ICLOUD_REGION`, `ICLOUD_TZ`, `ICLOUD_READ_ONLY`, `ICLOUD_OUTPUT`,
`ICLOUD_CALDAV_URL`, `ICLOUD_CARDDAV_URL`, `ICLOUD_IMAP_URL`.

## Agent skill

```sh
icloud skill > ~/.claude/skills/icloud/SKILL.md   # or wherever your agent reads skills
```

## Development

```sh
cargo test
pip install radicale && python -m radicale --config tests/e2e/radicale.conf &
tests/e2e/run.sh target/debug/icloud           # add E2E_IMAP=imap://127.0.0.1:3143 with GreenMail for mail tests
```

## License

MIT
