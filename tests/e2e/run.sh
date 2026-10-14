#!/usr/bin/env bash
# End-to-end tests against Radicale (CalDAV/CardDAV) and optionally GreenMail (IMAP).
# Usage: tests/e2e/run.sh [path/to/icloud]   (needs: jq, curl, python3; Radicale running on :5232)
# Set E2E_IMAP=imap://127.0.0.1:3143 and E2E_SMTP_PORT=3025 to run the mail tests.
set -euo pipefail
I=${1:-target/debug/icloud}
export ICLOUD_CONFIG_DIR=$(mktemp -d) ICLOUD_APPLE_ID=test@icloud.com ICLOUD_APP_PASSWORD=app-pass-1234
export ICLOUD_CALDAV_URL=http://127.0.0.1:5232/ ICLOUD_CARDDAV_URL=http://127.0.0.1:5232/ ICLOUD_TZ=Asia/Shanghai
pass=0
ok() { pass=$((pass+1)); echo "ok $pass - $1"; }
die() { echo "not ok - $1"; exit 1; }
q() { "$I" --quiet "$@"; }
expect_code() { # expect_code <exit> <E_CODE> cmd...
  local want=$1 code=$2; shift 2
  set +e; out=$("$I" "$@"); rc=$?; set -e
  [ "$rc" = "$want" ] || die "exit $rc != $want for: $* -> $out"
  [ "$(echo "$out" | jq -r .error.code)" = "$code" ] || die "code != $code for: $* -> $out"
}

# --- self-description
{ "$I" skill | grep -q "^name: icloud"; } || die "skill prints SKILL.md"; ok "skill prints SKILL.md"
{ [ "$(q help --json | jq '[.commands[] | select(.command=="cal.events.delete")][0].destructive')" = true ]; } || die "help --json marks destructive"; ok "help --json marks destructive"
{ [ "$(q schema cal.events.create | jq -r .output_schema)" = "icloud.cal.events.create/v1" ]; } || die "schema"; ok "schema"
{ expect_code 2 E_USAGE cal events nope; } || die "usage error is structured"; ok "usage error is structured"
{ expect_code 2 E_TIME_PARSE cal events list --from "next friday"; } || die "natural language rejected"; ok "natural language rejected"

# --- credentials
{ ICLOUD_APP_PASSWORD= expect_code 3 E_AUTH_MISSING --credential-backend env cal calendars list; } || die "missing env credential"; ok "missing env credential"
{ ICLOUD_APP_PASSWORD=wrong expect_code 3 E_AUTH_INVALID cal calendars list; } || die "wrong password -> E_AUTH_INVALID"; ok "wrong password -> E_AUTH_INVALID"
printf 'app-pass-1234\n' > "$ICLOUD_CONFIG_DIR/secret"; chmod 644 "$ICLOUD_CONFIG_DIR/secret"
q profile set credential.backend file >/dev/null; q profile set credential.file "$ICLOUD_CONFIG_DIR/secret" >/dev/null
{ expect_code 2 E_CONFIG cal calendars list; } || die "file backend refuses 0644"; ok "file backend refuses 0644"
chmod 600 "$ICLOUD_CONFIG_DIR/secret"
{ ICLOUD_APP_PASSWORD= q cal calendars list >/dev/null; } || die "file backend 0600 works"; ok "file backend 0600 works"
q profile set credential.password_command "echo app-pass-1234" >/dev/null; q profile set credential.backend password_command >/dev/null
{ ICLOUD_APP_PASSWORD= q cal calendars list >/dev/null; } || die "password_command backend"; ok "password_command backend"
q profile set credential.backend env >/dev/null
{ echo app-pass-1234 | q --credential-backend env auth login --apple-id test@icloud.com --password-stdin --region global | jq -e '.verified == true' >/dev/null; } || die "auth login verifies"; ok "auth login verifies"
{ [ "$(q auth status | jq -r .password)" = "****1234" ]; } || die "auth status masks password"; ok "auth status masks password"

# --- calendars
CAL=$(q cal calendars create --name "E2E 工作" --color "#FF2968" | jq -r .id)
{ [ "$(q cal calendars list | jq -r --arg c "$CAL" '.[] | select(.id==$c) | .name')" = "E2E 工作" ]; } || die "calendar create/list"; ok "calendar create/list"

# --- create / idempotency
mk() { q cal events create --calendar "E2E 工作" --title "和老王吃饭" --start 2026-10-16T15:00 --duration 1h --alarm -15m --location 望京 --idempotency-key wang "$@"; }
{ [ "$(mk --dry-run | jq -r .action)" = create ]; } || die "create dry-run"; ok "create dry-run"
{ [ "$(q cal events list --from 2026-10-16 --to 2026-10-17 --calendar "$CAL" | jq length)" = 0 ]; } || die "dry-run wrote nothing"; ok "dry-run wrote nothing"
ID=$(mk | jq -r .event.id)
{ [ "$(mk | jq -r .action)" = noop ]; } || die "idempotent retry is noop"; ok "idempotent retry is noop"
{ expect_code 5 E_CONFLICT cal events create --calendar "$CAL" --title other --start 2026-10-16T15:00 --idempotency-key wang; } || die "idempotency conflict"; ok "idempotency conflict"
EV=$(q cal events get "$ID")
{ [ "$(echo "$EV" | jq -r .start)" = "2026-10-16T15:00:00+08:00" ] && [ "$(echo "$EV" | jq -r '.alarms[0]')" = "-PT15M" ]; } || die "get returns times in tz + alarms"; ok "get returns times in tz + alarms"

# --- update
U=$(q cal events update "$ID" --start 2026-10-16T18:00 --notes "带酒")
{ [ "$(echo "$U" | jq -r .event.end)" = "2026-10-16T19:00:00+08:00" ]; } || die "update keeps duration"; ok "update keeps duration"
{ [ "$(echo "$U" | jq -c .changed)" = '["start","end","notes"]' ]; } || die "update reports changed fields"; ok "update reports changed fields"
{ expect_code 5 E_CONFLICT cal events update "$ID" --title x --if-match '"stale"'; } || die "etag mismatch"; ok "etag mismatch"
{ expect_code 6 E_READ_ONLY --read-only cal events update "$ID" --title x; } || die "read-only flag"; ok "read-only flag"
{ ICLOUD_READ_ONLY=1 expect_code 6 E_READ_ONLY cal events create --title x --start now; } || die "read-only env"; ok "read-only env"

# --- recurring + all-day + window queries
q cal events create --calendar "$CAL" --title Standup --start 2026-10-12T10:00 --duration 15m --rrule "FREQ=DAILY;COUNT=10" >/dev/null
q cal events create --calendar "$CAL" --title 出差 --all-day --start 2026-10-17 --end 2026-10-19 >/dev/null
L=$(q cal events list --calendar "$CAL" --from 2026-10-14 --to 2026-10-18)
{ [ "$(echo "$L" | jq '[.[] | select(.title=="Standup")] | length')" = 4 ]; } || die "recurring expanded in window"; ok "recurring expanded in window"
{ [ "$(echo "$L" | jq -r '.[] | select(.title=="出差") | .last_day')" = 2026-10-18 ]; } || die "all-day last_day"; ok "all-day last_day"
{ [ "$(q --limit 2 cal events list --calendar "$CAL" --from 2026-10-14 --to 2026-10-18 | jq length)" = 2 ]; } || die "limit"; ok "limit"
NEXT=$("$I" --limit 2 cal events list --calendar "$CAL" --from 2026-10-14 --to 2026-10-18 | jq -r .page.next_cursor)
{ [ "$(q --limit 2 --cursor "$NEXT" cal events list --calendar "$CAL" --from 2026-10-14 --to 2026-10-18 | jq length)" = 2 ]; } || die "cursor pagination"; ok "cursor pagination"
{ [ "$(q cal events search 老王 --calendar "$CAL" | jq length)" = 1 ]; } || die "search"; ok "search"
F=$(q cal free --calendar "$CAL" --from 2026-10-16 --to 2026-10-17 --work-hours 09:00-20:00 --min 60m)
{ [ "$(echo "$F" | jq -c '[.[] | .start[11:16] + "-" + .end[11:16]]')" = '["09:00-10:00","10:15-18:00","19:00-20:00"]' ]; } || die "free slots"; ok "free slots"

# --- sync
T=$(q cal events sync --calendar "$CAL" | jq -r .next_token)
q cal events create --calendar "$CAL" --title Later --start 2026-10-21T09:00 >/dev/null
S=$(q cal events sync --calendar "$CAL" --since-token "$T")
{ [ "$(echo "$S" | jq -c '[.changed[].title]')" = '["Later"]' ]; } || die "sync changed"; ok "sync changed"

# --- delete
{ expect_code 6 E_CONFIRM_REQUIRED cal events delete "$ID"; } || die "delete needs --yes"; ok "delete needs --yes"
{ [ "$(q cal events delete "$ID" --dry-run | jq -r .action)" = delete ]; } || die "delete dry-run"; ok "delete dry-run"
q cal events delete "$ID" --yes >/dev/null
{ expect_code 4 E_NOT_FOUND cal events get "$ID"; } || die "deleted"; ok "deleted"
{ [ "$(q cal events sync --calendar "$CAL" --since-token "$T" | jq '.deleted | length')" = 1 ]; } || die "sync deleted"; ok "sync deleted"
{ expect_code 6 E_CONFIRM_REQUIRED cal calendars delete "$CAL"; } || die "calendar delete needs --yes"; ok "calendar delete needs --yes"
{ q cal calendars delete "$CAL" --yes >/dev/null; } || die "calendar delete"; ok "calendar delete"

# --- contacts (read-only)
U=test@icloud.com:app-pass-1234; B=http://127.0.0.1:5232/test@icloud.com/e2e-book/
curl -sf -u $U -X MKCOL $B -H 'Content-Type: application/xml' --data '<?xml version="1.0"?><d:mkcol xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:set><d:prop><d:resourcetype><d:collection/><card:addressbook/></d:resourcetype><d:displayname>E2E Book</d:displayname></d:prop></d:set></d:mkcol>' >/dev/null || true
printf 'BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c1\r\nN:王;小明;;;\r\nFN:王小明\r\nTEL;TYPE=CELL:+86 138 0013 8000\r\nEMAIL;TYPE=WORK:xm@example.com\r\nBDAY:1990-01-02\r\nEND:VCARD\r\n' | curl -sf -u $U -X PUT ${B}c1.vcf -H 'Content-Type: text/vcard' --data-binary @- >/dev/null
{ [ "$(q contacts list --book "E2E Book" | jq -r '.[0].name')" = 王小明 ]; } || die "contacts list"; ok "contacts list"
{ [ "$(q contacts search 13800138 | jq -r '.[0].birthday')" = 1990-01-02 ]; } || die "contacts search by phone digits"; ok "contacts search by phone digits"
{ [ "$(q contacts get e2e-book/c1.vcf | jq -r '.emails[0].value')" = xm@example.com ]; } || die "contacts get"; ok "contacts get"

# --- mail (read-only), optional
if [ -n "${E2E_IMAP:-}" ]; then
  export ICLOUD_IMAP_URL=$E2E_IMAP
  python3 - <<PY
import smtplib
from email.message import EmailMessage
for i, (subj, body) in enumerate([("航班确认 CA1234", "您的航班已确认"), ("Weekly report", "hello world")]):
    m = EmailMessage(); m["From"] = "Airline <noreply@air.example>"; m["To"] = "test@icloud.com"; m["Subject"] = subj
    m.set_content(body)
    if i == 0: m.add_attachment(b"PDFDATA", maintype="application", subtype="pdf", filename="ticket.pdf")
    with smtplib.SMTP("127.0.0.1", ${E2E_SMTP_PORT:-3025}) as s: s.send_message(m)
PY
  sleep 1
  { [ "$(q mail folders list | jq '[.[] | select(.name=="INBOX")] | length')" = 1 ]; } || die "mail folders"; ok "mail folders"
  { [ "$(q mail list | jq length)" = 2 ]; } || die "mail list"; ok "mail list"
  { [ "$(q mail search --query Weekly | jq length)" = 1 ]; } || die "mail search ascii (server side)"; ok "mail search ascii (server side)"
  MID=$(q mail search --subject 航班 | jq -r '.[0].id')
  { [ "$MID" != null ]; } || die "mail search non-ascii (local)"; ok "mail search non-ascii (local)"
  G=$(q mail get "$MID")
  { [ "$(echo "$G" | jq -r .untrusted)" = true ] && echo "$G" | jq -r .body_text | grep -q 航班已确认; } || die "mail get body (untrusted)"; ok "mail get body (untrusted)"
  { [ "$(q mail list | jq -r '.[] | select(.id=="'"$MID"'") | .unread')" = true ]; } || die "reading did not mark as seen"; ok "reading did not mark as seen"
  { D=$(mktemp -d); q mail attachment get "$MID" --index 0 --out-dir "$D" >/dev/null && [ "$(cat "$D/ticket.pdf")" = PDFDATA ]; } || die "attachment get"; ok "attachment get"
fi
echo "all $pass e2e checks passed"
