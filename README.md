<img src="assets/icon.svg" width="64" height="64" alt="">

# lxw-cli

An unofficial, single-executable command-line client for the
[Lexware Office API](https://developers.lexware.io/docs/), built for AI agents: every
documented endpoint, offline command search, JSON in and out, typed errors with stable
exit codes, credentials in the OS keychain, and rate limiting that holds across parallel
processes.

> **Not affiliated with Lexware.** Lexware and Lexware Office are trademarks of
> Haufe-Lexware GmbH & Co. KG. This project is not affiliated with, endorsed by or
> supported by Haufe-Lexware.

```sh
printf %s "$KEY" | lxw auth login --with-token    # key from https://app.lexware.de/addons/public-api
lxw cli search "rechnung als pdf"                 # -> lxw invoices download
lxw schema invoices download                      # method, path, params, docs
lxw invoices download <uuid> --out ./docs/        # -> {"file":".../RE1001.pdf","bytes":...}
```

## Install

```sh
cargo build --release                 # target/release/lxw: ~1.6 MB on macOS (arm64)
scripts/build-linux.sh [arm64|amd64]  # dist/lxw-linux-<arch>: ~1.9 MB static musl binary, via Docker
```

The binary has no runtime files and no OpenSSL or system-certificate dependency; it
runs unchanged in minimal containers such as `busybox`.

## Authentication

Any Lexware Office user can use the API with a **personal API key**, created at
<https://app.lexware.de/addons/public-api>. No developer or partner account is needed.

- **Plan:** the Public API is included from **Lexware Office XL** at no extra cost.
  Free 30-day trial accounts have the XL features (apart from a few, e.g. ELSTER) and
  are what Lexware recommends for development.
- **Keys:** several per user, each optionally limited to certain permissions, valid
  24 months. Sent as `Authorization: Bearer <key>`.
- **No username/password login:** the API has no password grant, and this CLI never
  drives the web app's undocumented internals.
- **Partner API (OAuth2):** for products connecting *other* businesses' accounts. It
  needs a client id/secret from Lexware (partner agreement), uses the authorization-code
  flow with PKCE, and adds endpoints such as sending vouchers by email, cash boxes and
  financial accounts. Supported via `lxw auth login --oauth ...`; these commands are
  tagged `partner-api-only`.

## Lexware's terms

Using the API is governed by Lexware's
[Public API license](https://agb.lexware.de/lexware-office/public-api-lizenz--und-nutzungsbedingungen),
the [Lexware Office terms](https://agb.lexware.de/lexware-office) and the
[Fair Usage Policy](https://agb.lexware.de/lexware-office/fair-usage-policy). The CLI
enforces what it can:

- **Documented endpoints only** (undocumented interfaces are forbidden): `lxw request`
  refuses paths that are not in the catalog.
- **At most 20 outgoing vouchers per minute** (Fair Usage Policy): creating or emailing
  invoices, quotations, credit notes, order confirmations, delivery notes and dunnings
  is paced to one every 3 seconds across processes.
- **Rate limit** of about 2 requests per second, kept client-side.

Your part: use your own account and key only (asking for other people's keys is
forbidden; other businesses' accounts need the Partner API), don't poll for changes
(use webhooks via `lxw event-subscriptions create`), and don't load-test the API.

## API facts the CLI handles for you

| Topic                     | Lexware behavior                                                               | CLI                                                                                                         |
| ------------------------- | ------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------- |
| Rate limit                | ~2 req/s per API client, all endpoints together; 429 = not executed            | Client-side limiter (default 1.8/s) shared by all `lxw` processes via a lock file; 429 retried with backoff |
| Errors                    | 3 shapes: regular (`details[]`), legacy (`IssueList`), gateway (`{message}`)   | One JSON error on stderr with `type`, `details[].field`, `hint`, `retryable`, exit code                     |
| 504                       | Request may still have been processed                                          | Writes are never auto-retried; hint says to verify first                                                    |
| Updates                   | `PUT` needs the current `version` (409 otherwise)                              | `--merge` fetches, applies a JSON merge patch, sends with the current version                               |
| Search strings            | `&`, `<`, `>` must be HTML-escaped *and* URL-encoded                           | Done automatically for `contacts --name/--email`, `--voucher-number`                                        |
| Paging                    | 0-based, max 100-250 per page, hard 10,000 window                              | `--all`, `--max-items`, `--ndjson`; warns at 10,000                                                         |
| Finalize / email / delete | Irreversible or outward-facing                                                 | Require `--yes` (exit 10 otherwise, nothing sent)                                                           |
| Documents                 | `/file` subresources, PDF or XRechnung XML by `Accept`; drafts have none (409) | `download --accept application/xml --out DIR`                                                               |

Coverage: 90 operations (55 Public API, 35 Partner-API-only). A test fails if any
endpoint mentioned in Lexware's docs is missing from `catalog/operations.toml`.

## For agents

Every help page starts with a discovery banner (the layout follows Cloudflare's `cf` CLI):

1. `lxw cli search "<task>"`: up to 5 JSON matches; English or German.
2. `lxw schema <command words>`: the request behind a command: method, path,
   parameters, required body fields, a documented example body (`--response` adds an
   example response). Replace the leading `lxw` of any command with `lxw schema`.
3. `lxw schema <resource>`: its commands and every JSON field with type and meaning.
4. `lxw cli guide`: conventions in one page (auth, bodies, paging, safety, errors, terms).

Discovery works offline; the catalog and the field reference are compiled into the binary.

```sh
lxw contacts list --name "Johnson & Partner" --all --fields id,company.name
lxw invoices create --body @invoice.json                  # draft
lxw invoices create --body @invoice.json --finalize --yes # finalized
lxw contacts update <uuid> --merge --set note="VIP"
lxw voucherlist list --voucher-type invoice --voucher-status overdue --all
lxw files upload --file receipt.pdf
lxw request GET /v1/contacts --query page=2               # raw call, documented endpoints only
lxw invoices create --body @invoice.json --dry-run        # preview, sends nothing
```

**Exit codes:** 0 ok, 1 internal/io, 2 usage, 3 auth, 4 not found, 5 validation,
6 conflict, 7 rate limited, 8 server/network, 9 payment required (plan), 10 needs `--yes`.

## Configuration

Nothing is required besides a key. Optional settings:

| Env var / flag                              | Purpose                                                                                |
| ------------------------------------------- | -------------------------------------------------------------------------------------- |
| `LXW_API_KEY`, `LXW_API_KEY_FILE`           | API key (takes precedence over stored profiles)                                        |
| `lxw auth login --with-token` (stdin)       | Store a key in the OS credential store (see below)                                     |
| `--store file`, `LXW_CREDENTIAL_STORE=file` | Keep secrets in `~/.config/lxw/config.json` instead (plaintext, mode 0600)             |
| `--profile`, `LXW_PROFILE`                  | Named profiles for several organizations                                               |
| `--rate-limit`, `LXW_RATE_LIMIT`            | Requests/second (default 1.8; 0 disables)                                              |
| `--max-retries`, `--timeout`, `--base-url`  | Also `LXW_MAX_RETRIES`, `LXW_TIMEOUT`, `LXW_BASE_URL`                                  |
| `LXW_CONFIG_DIR`, `LXW_CACHE_DIR`           | Override config / rate-limit state locations                                           |
| `SSL_CERT_FILE`                             | PEM bundle to trust instead of the built-in Mozilla roots (e.g. a corporate TLS proxy) |

## Credential storage

Stored secrets (API key, OAuth client secret and tokens) live in the OS credential store;
`~/.config/lxw/config.json` only holds non-secret settings.

- **macOS:** the login Keychain, one item per profile (service `lxw-cli`). The item
  trusts the `lxw` binary that created it, so other programs, including
  `security find-generic-password`, need your approval to read it. After the binary
  changes (an update or rebuild), macOS asks once; click "Always Allow". Without a
  terminal (agents) the CLI does not wait for that dialog: it exits with code 3 and asks
  you to run `lxw auth status` once in a terminal.
- **Linux:** the Secret Service (GNOME Keyring, KWallet, ...) via `secret-tool` from
  libsecret; secrets are passed over stdin, never as process arguments.
- **No credential store** (containers, CI, other platforms): use `LXW_API_KEY` /
  `LXW_API_KEY_FILE`, or opt into the plaintext file store with `--store file`.
- Stored credentials are bound to the API host they were verified against: they are
  never sent to a different `--base-url` / `LXW_BASE_URL`.

## Size and speed

- a blocking HTTP client (`ureq`) instead of an async stack, with rustls + ring and
  Mozilla's root certificates built in;
- a release profile tuned for size (`opt-level = "z"`, fat LTO, one codegen unit,
  `panic = "abort"`, stripped);
- `build.rs` turning the catalog and the docs extract into minified JSON, keeping only
  what the CLI reads, deflate-compressed (~300 KB of data down to ~45 KB);
- lazy work at startup: commands of a resource are built only when it is used, and only
  the requested docs section is parsed.

A command takes about 1–3 ms beyond process start; API calls are bound by network
latency (~250 ms per request to Lexware).

## Development

- `cargo test`: unit tests plus end-to-end tests that run the binary against a mock API.
  Tests never touch the real credential store; the ignored test `os_store_round_trip`
  exercises it on demand.
- `catalog/operations.toml`: the operation catalog. Commands, search and `schema` are
  generated from it.
- `catalog/docs.json`: field tables, required fields and examples extracted from
  [Lexware's API documentation](https://developers.lexware.io/docs/) (© Haufe-Lexware),
  regenerated with `scripts/extract_docs.py`. The build works without it (`LXW_NO_DOCS=1`
  leaves it out); `lxw schema` then links to the online docs instead.
- Dependencies are limited to widely used, actively maintained crates: clap, ureq (rustls,
  ring, webpki-roots), serde, serde_json, miniz_oxide, form_urlencoded, percent-encoding,
  sha2, base64, getrandom, strsim, security-framework (macOS only); toml at build time only.

## License

MIT for the code in this repository, see [LICENSE](LICENSE). The extract of Lexware's
documentation in `catalog/docs.json` remains the property of Haufe-Lexware.
