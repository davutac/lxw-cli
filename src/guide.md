# lxw - agent guide

Covers every endpoint of the Lexware Office API (Public API with an API key;
Partner-API-only operations are tagged `partner-api-only` and need OAuth).

## Auth
- `export LXW_API_KEY=...` (or `LXW_API_KEY_FILE=/path`), or store it:
  `printf %s "$KEY" | lxw auth login --with-token`. Stored keys go into the OS
  credential store (macOS Keychain, Linux Secret Service), never into plain files,
  and are only ever sent to the API host they were verified against.
- macOS asks once to approve Keychain access after the lxw binary changes. Without
  a terminal the CLI fails fast (exit 3) instead of waiting: run `lxw auth status`
  in a terminal and click "Always Allow".
- Keys: https://app.lexware.de/addons/public-api (Lexware Office XL; valid
  24 months; can be limited to certain permissions).
- Check: `lxw auth status` (calls /v1/profile).
- Multiple accounts: `--profile NAME` or `LXW_PROFILE`.

## Find the right command
1. `lxw cli search "<task>"`: up to 5 JSON matches; English or German,
   e.g. `lxw cli search "rechnung als pdf"`. Pick the best match.
2. `<command> --help`: flags of that command.
3. `lxw schema <command words>`: the API request behind it (method, path,
   params, required body fields, documented example body). Replace the leading
   `lxw` of any command with `lxw schema`; add `--response` for an
   example response.
4. `lxw schema <resource>`: its commands plus every JSON field with type and
   meaning (`--object "line items"` to narrow, `--brief` for names only).
- `lxw schema --list` lists all commands. Discovery works offline.

## Calling the API
- Shape: `lxw <resource> <action> [IDs] [--flags]`, e.g.
  `lxw invoices get <uuid>`, `lxw contacts list --name Muster --all`.
- IDs are UUIDs. Find them via `voucherlist list --voucher-number RE-1001` or
  `contacts list --name ...`.
- Bodies: `--body '{"...":...}'`, `--body @file.json`, `--body -` (stdin), plus
  `--set path=value` to build or override fields
  (`--set 'lineItems[0].quantity=2'`, values parsed as JSON when valid).
- Updates (PUT) need the current `version`. Use `--merge` to send only changes:
  `lxw contacts update <id> --merge --set note="VIP"`.
- Lists: `--page/--size`, `--all` (all pages as one array), `--max-items N`,
  `--ndjson` (stream). Lexware stops paging at 10,000 results.
- Downloads: `lxw invoices download <id> --out ./docs/` prints
  `{"file","bytes","contentType"}`; `--accept application/xml` for XRechnung.
- Uploads: `lxw files upload --file receipt.pdf` (voucher, max 5 MB).
- Output shaping: `--fields id,voucherNumber,address.name`.
- Preview without sending: `--dry-run` (shows URL, headers, body).
- Unsupported call? `lxw request GET /v1/... [--query k=v] [--body ...]`.

## Safety
Irreversible or outward-facing actions need `--yes`: deletes, `--finalize`
(finalized vouchers cannot be edited or deleted), and `send-email`. Without
it the CLI exits 10 and sends nothing. Ask the user before adding `--yes`.

## Formats Lexware expects
- Timestamps: `2026-01-31T00:00:00.000+01:00` (exactly 3 ms digits, offset with colon).
- List filters with dates: `2026-01-31`.
- Country codes: ISO 3166 alpha-2 (`DE`); tax rates: numbers like 19, 7, 0.
- Text fields support **bold**, __italic__ and `- ` lists.

## Output and errors
- Success: JSON on stdout (pretty in a terminal, compact when piped).
- Failure: `{"error":{"type","message","status","details","hint","retryable",...}}`
  on stderr. `details[].field` names the offending body field on validation errors.
- Exit codes: 0 ok, 1 internal/io, 2 usage, 3 auth, 4 not found,
  5 validation/bad request, 6 conflict (stale version), 7 rate limited,
  8 server/network/timeout, 9 payment required (plan), 10 needs --yes.

## Lexware's terms (enforced or required)
- Only documented endpoints: undocumented interfaces are forbidden by Lexware's terms,
  so `lxw request` refuses paths that are not in the catalog.
- At most 20 outgoing vouchers (invoices, quotations, credit notes, ...) per minute
  (Fair Usage Policy): creating/sending them is paced to one every 3 s.
- Do not poll for changes (e.g. `voucherlist list` in a loop); subscribe to webhooks
  with `event-subscriptions create`. No load or performance tests against the API.
- Use only the account's own API key; never ask for or use other people's keys.
  Connecting other businesses' accounts requires the Partner API (OAuth).
- Never print or log API keys; they stay in the OS credential store.

## Rate limits and retries
- Lexware allows about 2 requests/second per API key. The CLI spaces requests
  (default 1.8/s) using a lock file shared by all lxw processes on this
  machine, so parallel invocations queue instead of failing.
- 429s are retried with backoff (honoring Retry-After); network errors and 5xx
  are retried only when safe (GET, or the request never left the machine).
- Tune with `--rate-limit`, `--max-retries`, `--timeout`; see waits with `-v`.
- A 504 on a write means it MAY have been applied: check before retrying.
