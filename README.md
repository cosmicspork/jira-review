# jira-review

Jira from the command line and from agents, where **no write is applied without review**. Reads answer directly. A write (`update`, `transition`, `link`, `create`, `comment`) is first *proposed* as a change file that holds the issue's current fields, the proposed fields, and the issue's `updated` stamp. `show` renders it as a diff, and `apply` sends it only if the issue hasn't moved since.

It has two modes:

- **a CLI** for people and scripts. A change file is plain JSON, so it can be reviewed in git like any other diff;
- **`jira-review mcp`**, an MCP server on stdio for any agent client. Reads answer directly. Writes return a proposal id the agent waits on while a person runs `jira-review review`.

A third mode, `jira-review serve`, is coming in a follow-up change. It implements the [Tracon](https://github.com/cosmicspork/tracon) plugin contract, in which the node's approval card is the review step.

Other Jira tools apply writes immediately. Atlassian's ACLI is Cloud only, closed source, and cannot list an issue's available transitions. [jira-cli](https://github.com/ankitpokhrel/jira-cli) is interactive. Atlassian's hosted Rovo MCP server and [mcp-atlassian](https://github.com/sooperset/mcp-atlassian) apply whatever the agent asks. jira-review puts a reviewable, stale-checked change between the request and Jira.

It talks to Jira's REST API v2 with wiki markup (no ADF), which both Jira Cloud and Jira Data Center serve.

## Install

```sh
cargo install --git https://github.com/cosmicspork/jira-review
```

It is built with rustls and pure-Rust dependencies: no OpenSSL, no libdbus, no host libraries beyond the C runtime. Prebuilt release archives will come with the Tracon plugin change.

## Configuration

Credentials use the same fields everywhere:

| Field | |
| --- | --- |
| `JIRA_URL` | The site, e.g. `https://example.atlassian.net` (a trailing `/` is trimmed) |
| `JIRA_EMAIL` + `JIRA_TOKEN` | Jira Cloud: account email and [API token](https://id.atlassian.com/manage-profile/security/api-tokens), sent as HTTP Basic |
| `JIRA_PAT` | Jira Data Center: a personal access token, sent as `Authorization: Bearer` |

Give either the email and token or the PAT; both or neither is an error that names the fields.

Standalone, the credential comes from the first of these that sets `JIRA_URL`. The source that wins supplies every field; fields are never mixed between sources:

1. the process environment;
2. a `.env` file in the current directory, or the file named by `--env-file`;
3. the OS keyring: service **`jira-review`**, user **`<profile>`** (`default` unless `--profile` or `JIRA_REVIEW_PROFILE` says otherwise). The secret is the same fields as an env-style block (`JIRA_URL=…` / `JIRA_EMAIL=…` / `JIRA_TOKEN=…`, or `JIRA_PAT=…`).

`jira-review login [--profile P]` prompts for the fields (the token without echo) and stores them in the keyring. `jira-review logout [--profile P]` removes them. The keyring is macOS Keychain, or on Linux the Secret Service (GNOME Keyring, KWallet), reached over D-Bus in pure Rust.

## CLI

```sh
# Reads
jira-review search 'project = WRK AND statusCategory != Done' --limit 10 --format table
jira-review issue WRK-12 --markdown          # description and comments as Markdown
jira-review transitions WRK-12 --format table

# Writes: each only proposes, writing ./<id>.json and printing the diff
jira-review update WRK-12 --summary 'Port the feed' --add-label etl --remove-label stale
jira-review update WRK-12 --description-file notes.md --markdown
jira-review transition WRK-12 'In Review'
jira-review transition WRK-12 Resolve --field 'resolution={"name":"Fixed"}'
jira-review link WRK-1 Blocks WRK-2
jira-review create --project WRK --type Task --summary 'Write the importer' --parent WRK-10
jira-review create --file issues.json        # {"issues": [...]} or a list; parents may be "#N"
jira-review comment WRK-12 --body-file reply.md --markdown

# Review and apply
jira-review show 0193…-update-WRK-12.json
jira-review apply 0193…-update-WRK-12.json
```

`--out F` writes the change file elsewhere (`--out -` writes it to stdout and the diff to stderr). `--markdown` converts Markdown input to wiki markup before proposing; the change file stores the wiki markup that would be sent. `show --markdown` renders wiki text as Markdown; `--no-color` (or `NO_COLOR`) turns colour off.

`apply` exits **0** when the change was applied and **3** when an issue moved since the change was proposed (nothing is sent; propose again). It exits **1** for anything else, including a Jira refusal. An applied or failed change has its `outcome` written back into the file, and a file that already has an outcome is not applied again.

### The change file

```json
{
  "jira_review": 1,
  "id": "019a…-update-WRK-12",
  "kind": "update",
  "jira": "https://example.atlassian.net",
  "created": "2026-11-17T10:00:00Z",
  "args": { "key": "WRK-12", "summary": "Port the feed", "add_labels": ["etl"] },
  "versions": { "WRK-12": "2026-11-16T09:12:33.000+0000" },
  "current": { "summary": "Port it", "description": "…", "priority": "Medium", "labels": ["stale"], "parent": null },
  "proposed": { "summary": "Port the feed", "description": "…", "priority": "Medium", "labels": ["stale", "etl"], "parent": null }
}
```

- `args` are the write's arguments. `apply` validates them again and builds the request from them.
- `versions` lists every existing issue the change depends on, with its `updated` stamp at propose time. `update`, `transition` and `comment` depend on their issue, and `link` depends on both of its issues. `create` depends on nothing, so it cannot go stale. A parent named by key is checked to exist at propose time only, and a `#N` parent is an earlier item of the same change.
- **The stale rule:** `apply` reads each issue's `updated` and refuses if any differs. Jira has no conditional write, so a change that lands between that check and the write is not caught; the window is one request long.
- Per kind: `update` shows the five fields it can write (summary, description, priority, labels, parent). Label removals are matched to the issue's own spelling, and any the issue doesn't have are listed as `not_on_issue`. `transition` shows the status before and after, with the transition it takes. A name matching no transition or several is refused with the list, and so is a transition whose required fields are missing. `link` shows `from`'s links before and after, in the link type's wording. `create` shows the planned issues (up to 10). `comment` shows the last comment's author and time and the new body.

## `jira-review mcp`

An MCP server (protocol `2025-06-18`) on stdin/stdout:

```json
{ "mcpServers": { "jira": { "command": "jira-review", "args": ["mcp"] } } }
```

It serves `search`, `issue` and `transitions`, which answer at once. `update`, `transition`, `link`, `create` and `comment` store a proposal and return `{ "proposal_id", "status": "pending", "diff" }`. The extra tool `proposal_status { id, wait_secs ≤ 45 }` waits for a decision and reports `pending`, `approved` (with Jira's result), `rejected` (with the reviewer's reason), `stale` (the issue moved; propose again) or `failed` (Jira refused).

A person decides in a terminal:

```sh
jira-review review                 # each pending proposal with its diff: [a]pprove / [r]eject / [s]kip / [q]uit
jira-review review --list
jira-review review --approve <id>
jira-review review --reject <id> --reason 'not this sprint'
```

Approving applies the change at once with the reviewer's own credential, running the stale check at that moment. Proposals live in `$JIRA_REVIEW_STATE`, else `$XDG_STATE_HOME/jira-review`, else `~/.local/state/jira-review` (`~/Library/Application Support/jira-review` on macOS), as `pending/<id>.json` and then `decided/<id>.json`. Claiming one is an atomic rename, so two reviewers can't both apply it.

## The `tracon/preview` seam

`change::propose` only reads: its proposal is what a Tracon `tracon/preview` would answer, and its `versions` are the version token. `change::apply(…, expected)` checks a token handed back with an approved call. The plugin mode that uses them comes in a follow-up change.

## The `jira_wiki` format

This crate **owns the `jira_wiki` format**, conversion between Jira wiki markup and Markdown in both directions. A later change will declare it in the Tracon plugin manifest (and serve `tracon/format/convert`). Both directions are library functions and CLI subcommands:

```sh
jira-review wiki to-markdown description.txt
jira-review wiki from-markdown notes.md
```

`jira_review::wiki::to_markdown` ports Tracon's SPA renderer (headings, `*bold*` / `_em_` / `-strike-` spans only at word boundaries, `{{code}}`, `{code}` / `{noformat}` blocks, `bq.` and `{quote}`, `----`, nested `*` / `#` / `-` lists, `||` / `|` tables, `[text|url]` links, `[~mention]`, bare URLs, `{color}`), writing Markdown where the renderer wrote HTML:

- it never writes HTML, and escapes text that Markdown would read as markup;
- only http(s), mailto and rooted (`/…`) links survive; any other scheme keeps only its text;
- `+inserted+` keeps its text, since Markdown has no insert mark;
- `{color}` keeps its text;
- a wiki line break inside a paragraph becomes a Markdown hard break.

`from_markdown` covers headings, emphasis, strikethrough, code spans and blocks, quotes, rules, nested lists, links, images, GFM tables and hard breaks, and escapes characters that wiki markup would read as markup. Converting Markdown to wiki and back is stable for the documents in its tests.

## What is verified, and what isn't

**Verified** by `cargo test`, against a stateful fake Jira (an axum app on loopback):

- each read, including the Data Center search fallback;
- each write's propose → show → apply, with the exact requests sent;
- a stale apply refused after the fake's `updated` moves, with nothing sent;
- multi-issue create with `#N` parents and a refusal partway through;
- both auth schemes' headers;
- credential precedence (environment, then `.env`, then keyring);
- `mcp` over stdio with the proposal flow across processes (approve, reject, stale);
- the CLI's change files: propose, show, apply once, and a stale apply exiting 3.

**Not verified:**

- a live Jira Cloud site. `tests/live.rs` is a read-only smoke test, run with `JIRA_LIVE=1 cargo test --test live -- --ignored`;
- **Jira Data Center (bearer PAT): unverified** on a real instance;
- the keyring backends (`login`, `logout`, keyring reads). Only the secret format is tested.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

MIT licensed; see [LICENSE](LICENSE).
