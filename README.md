<img src="assets/icon.svg" width="64" height="64" alt="">

# lxw-cli

Unofficial command-line client for the [Lexware Office API](https://developers.lexware.io/docs/),
made to be used by AI agents. It covers every documented endpoint, prints JSON, and ships
as a single binary.

Not affiliated with or endorsed by Haufe-Lexware. Lexware and Lexware Office are
trademarks of Haufe-Lexware GmbH & Co. KG.

## Install

Requires Rust 1.89 or newer.

```sh
cargo build --release    # binary at target/release/lxw
```

For a static Linux binary, run `scripts/build-linux.sh [arm64|amd64]` (needs Docker).

## Log in

You need Lexware Office XL (or the free 30-day trial) and an API key from
<https://app.lexware.de/addons/public-api>.

```sh
printf %s "$KEY" | lxw auth login --with-token
lxw auth status
```

The key goes into the macOS Keychain or the Linux Secret Service. On macOS, each new
`lxw` binary needs your approval once: run `lxw auth status` in a terminal and click
"Always Allow". Where no keychain exists (CI, containers), set `LXW_API_KEY` instead.

## Use

```sh
lxw cli search "rechnung als pdf"              # find the command for a task
lxw schema invoices create                     # request, required fields, example body
lxw contacts list --name Muster --all
lxw invoices create --body @invoice.json       # draft; add --finalize --yes to finalize
lxw invoices download <id> --out ./pdfs/
lxw contacts update <id> --merge --set note=VIP
```

Results are JSON on stdout. Errors are JSON on stderr, with a hint and an exit code.
`--dry-run` prints the request instead of sending it. Deleting, finalizing and sending
email need `--yes`; without it nothing is sent.

## For agents

Tell your agent to run `lxw cli guide` first. Every help page tells it to find commands
with `lxw cli search` and to inspect them with `lxw schema <command>`, both of which work
offline.

## Configuration

| Setting | Purpose |
|---|---|
| `LXW_API_KEY`, `LXW_API_KEY_FILE` | API key from the environment |
| `--profile`, `LXW_PROFILE` | Use several Lexware accounts |
| `lxw auth login --store file` | Keep the key in `~/.config/lxw/config.json` (plaintext) |
| `LXW_RATE_LIMIT`, `LXW_MAX_RETRIES`, `LXW_TIMEOUT` | Pacing, retries, timeout |
| `SSL_CERT_FILE` | Trust a custom CA bundle, e.g. behind a corporate proxy |

## Limits and Lexware's terms

Lexware allows about 2 requests per second. `lxw` keeps all its processes on a machine
below that and retries when Lexware says to slow down. Following Lexware's
[Fair Usage Policy](https://agb.lexware.de/lexware-office/fair-usage-policy), it creates
or emails at most 20 outgoing documents per minute. It only calls documented endpoints.

Use your own API key only, and subscribe to webhooks (`lxw event-subscriptions create`)
instead of polling. Commands marked `partner-api-only` need OAuth credentials from
Lexware's partner program (`lxw auth login --oauth`).

## Exit codes

| Code | Meaning | Code | Meaning |
|---|---|---|---|
| 0 | OK | 6 | Conflict (outdated version) |
| 1 | Internal or I/O error | 7 | Rate limited |
| 2 | Usage error | 8 | Server or network error |
| 3 | Authentication | 9 | Not included in your Lexware plan |
| 4 | Not found | 10 | Needs `--yes` |
| 5 | Validation failed | | |

## Development

`cargo test` runs unit tests and end-to-end tests against a mock API; it never touches
your keychain. Commands are generated from `catalog/operations.toml`.
`catalog/docs.json` is an extract of Lexware's documentation, regenerated with
`scripts/extract_docs.py`.

## License

MIT. The documentation extract in `catalog/docs.json` belongs to Haufe-Lexware.
