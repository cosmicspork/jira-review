# jira-review

Jira from the command line and from agents, where **no write is applied without review**. Reads answer directly. A write (`update`, `transition`, `link`, `create`, `comment`) is first *proposed* as a change file that holds the issue's current fields, the proposed fields, and the issue's `updated` stamp. `show` renders it as a diff, and `apply` sends it only if the issue hasn't moved since.

One binary, three modes:

- **a CLI** for people and scripts. A change file is plain JSON, so it can be reviewed in git like any other diff;
- **`jira-review mcp`**, an MCP server on stdio for any agent client. Reads answer directly. Writes return a proposal id the agent waits on while a person runs `jira-review review`, in a terminal or on a local page (`--web`);
- **`jira-review serve`**, the [Tracon](https://github.com/cosmicspork/tracon) plugin contract v1. The node's approval card is the review step.

Other Jira tools apply writes immediately. Atlassian's ACLI is Cloud only, closed source, and cannot list an issue's available transitions. [jira-cli](https://github.com/ankitpokhrel/jira-cli) is interactive. Atlassian's hosted Rovo MCP server and [mcp-atlassian](https://github.com/sooperset/mcp-atlassian) apply whatever the agent asks. jira-review puts a reviewable, stale-checked change between the request and Jira.

It talks to Jira's REST API v2 with wiki markup (no ADF), which both Jira Cloud and Jira Data Center serve.

## Install

Download the archive for your platform from the [releases](https://github.com/cosmicspork/jira-review/releases) (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`) and check it against `checksums.txt`. The binary is `bin/jira-review`. It is built with rustls and pure-Rust dependencies: no OpenSSL, no libdbus, no host libraries beyond the C runtime.

Or build it yourself:

```sh
cargo install --git https://github.com/cosmicspork/jira-review
```

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
jira-review update WRK-12 --assignee ada@example.com --due 2026-12-01
jira-review update WRK-12 --unassign --clear-due
jira-review transition WRK-12 'In Review'
jira-review transition WRK-12 Resolve --field 'resolution={"name":"Fixed"}'
jira-review transition WRK-12 --to Done --field 'resolution={"name":"Fixed"}'   # every hop, one change
jira-review transition WRK-12 --hop 'Requirements Gathering' --hop 'In Progress' --hop UAT
jira-review link WRK-1 Blocks WRK-2
jira-review create --project WRK --type Task --summary 'Write the importer' --parent WRK-10 \
  --assignee me --due 2026-12-15
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
  "current": { "summary": "Port it", "description": "…", "priority": "Medium", "assignee": null, "assignee_ref": null, "duedate": null, "labels": ["stale"], "parent": null },
  "proposed": { "summary": "Port the feed", "description": "…", "priority": "Medium", "assignee": null, "assignee_ref": null, "duedate": null, "labels": ["stale", "etl"], "parent": null }
}
```

- `args` are the write's arguments. `apply` validates them again and builds the request from them.
- `versions` lists every existing issue the change depends on, with its `updated` stamp at propose time. `update`, `transition` and `comment` depend on their issue, and `link` depends on both of its issues. `create` depends on nothing, so it cannot go stale. A parent named by key is checked to exist at propose time only, and a `#N` parent is an earlier item of the same change.
- **The stale rule:** `apply` reads each issue's `updated` and refuses if any differs. Jira has no conditional write, so a change that lands between that check and the write is not caught; the window is one request long.
- Per kind: `update` shows the seven fields it can write (summary, description, priority, assignee, due date, labels, parent). Label removals are matched to the issue's own spelling, and any the issue doesn't have are listed as `not_on_issue`. `transition` shows the status before and after, with the transition it takes. A name matching no transition or several is refused with the list, and so is a transition whose required fields are missing. A [route](#transitions-over-several-hops) shows every hop in order. `link` shows `from`'s links before and after, in the link type's wording. `create` shows the planned issues (up to 10).
- **Assignee and due date.** `duedate` is `YYYY-MM-DD`, checked to be a real day, or null to clear it. `assignee` names a user the way a person would: an account id, a username, an email, a display name, or `me` for the credential's own user; null unassigns. Propose finds them among the users the issue (for `create`, the project) can be assigned to, with Jira's `user/assignable/search`: by `query`, then by `accountId`, on Cloud, and by `username` on Data Center. An exact match on id, username, email or display name wins; otherwise a search that finds exactly one user takes them, and one that finds none or several is refused with who it found. The change shows the assignee by display name and keeps the account Jira takes (`{"accountId": …}` on Cloud, `{"name": …}` on Data Center) as `assignee_ref`, which is what `apply` sends. `--assignee`, `--unassign`, `--due` and `--clear-due` are the CLI's spellings. `comment` shows the last comment's author and time and the new body.

### Transitions over several hops

A strict workflow (To Do, Requirements Gathering, In Progress, UAT, Rollout, Done, each reachable only from the one before) takes five transitions to close an issue. `transition` takes one of three arguments:

- `transition`: one transition the issue offers now, by id, transition name or the status it leads to;
- `to_status`: the status to end in, through as many transitions as it takes;
- `path`: the hops to take in order, each named as `transition` is, or `{ "transition": …, "fields": {…} }` for a hop that takes fields (`--hop` on the CLI, repeated).

Either of the last two is **one change, approved once**, that runs each hop in turn. `fields` given beside `to_status` or `path` go with the last hop (`--field` on the CLI), where a resolution usually is.

**Finding the route.** Jira's REST API lists only the transitions an issue offers from where it is now, and nothing a normal user can read reveals what a later status offers without moving the issue there. So:

- A `to_status` that a transition offered now leads to takes that transition, and needs nothing else. Two offered transitions to it are refused with both.
- Further away, the route is read from the workflow. Jira Cloud shows a workflow, read only, through `POST /rest/api/3/workflows/preview` to a caller with the *View (read-only) workflow* or *Administer projects* project permission. The route is the shortest one, through directed transitions only, that starts with a transition the issue offers now. Global transitions (Canceled or On-hold from any status) are used only when no directed route exists, because an equally short detour through On-hold is never the route anyone means. No route, or more than one equally short, is refused with the routes listed, so the caller can pick one with `path`.
- Data Center has no REST read of a workflow's transitions (`GET /rest/api/2/workflow` lists names and step counts, for administrators), and a Cloud credential without the permission is refused it too. There, a `to_status` past the next status is refused with what the issue offers now, and the caller names the hops with `path`. Statuses work as hop names, so `path: ["Requirements Gathering", "In Progress", "UAT", "Rollout", "Done"]` is enough.
- A `path` is checked hop by hop against the workflow when it can be read. When it can't, its first hop is checked against what the issue offers, and each later hop is resolved only when the issue reaches it. The card says which.

Only the first hop's required fields can be known in advance (Jira lists a transition's fields only once the issue offers it). The card shows them for the first hop and says that later hops' are checked when the issue gets there.

**Applying it.** `apply` checks the issue's `updated` stamp once, as for any change. Then, before each hop, it reads the issue's status and what it offers, and stops without sending that hop if the issue is not where the hop starts, the transition is no longer offered or now leads elsewhere, or its required fields are missing. A hop Jira refuses stops it too. Nothing is rolled back: the hops already sent stay applied and the issue is left where it stopped. The call returns once every hop has been applied or one has stopped it:

```json
{ "key": "WRK-12", "final_status": "In Progress",
  "hops": [ { "from": "To Do", "transition": { "id": "11", "name": "Gather" }, "to": "Requirements Gathering", "ok": true },
            { "from": "Requirements Gathering", "transition": { "id": "21", "name": "Start" }, "to": "In Progress", "ok": true },
            { "from": "In Progress", "transition": { "id": "31", "name": "Test" }, "to": "UAT", "ok": false,
              "error": "WRK-12 no longer offers transition 31 (Test) from In Progress; …" } ],
  "stopped_at": 2,
  "not_tried": [ { "from": "UAT", "transition": { "id": "41", "name": "Roll out" }, "to": "Rollout" } ] }
```

`final_status` is read back from Jira after the last hop, so it is the issue's real status. `stopped_at` and `not_tried` appear only when the route stopped part way. A route whose first hop stops moves nothing and is an error, as a refused single transition is.

## `jira-review mcp`

An MCP server (protocol `2025-06-18`) on stdin/stdout:

```json
{ "mcpServers": { "jira": { "command": "jira-review", "args": ["mcp"] } } }
```

It serves `search`, `issue` and `transitions`, which answer at once. `update`, `transition`, `link`, `create` and `comment` store a proposal and return `{ "proposal_id", "status": "pending", "diff" }`.

`update`, `create` and `comment` take an optional `format`: `jira_wiki` (the default) or `markdown`. Markdown text (a `description`, on each of `issues` too, or a comment's `body`) is converted to wiki markup before proposing, so the proposal holds and shows what will be sent, as the CLI's `--markdown` does. `serve` takes the same argument.

`search` pages when it is given `page: true` or a `cursor`, and `limit` may then go to 100 (100 unless you say). A page answers in the shape Tracon's `GET /api/jira/search` returns, so the node can pass it through unchanged:

```json
{ "site": "https://example.atlassian.net", "account_email": "me@example.com",
  "issues": [ { "id": "10001", "key": "WRK-12", "summary": "Port the feed", "status": "To Do",
                "status_category": "new", "priority": "Medium", "issue_type": "Task",
                "labels": ["etl"], "parent": { "key": "WRK-10", "summary": "Feeds", "issue_type": "Epic" } } ],
  "next_cursor": "…" }
```

`site` is the origin of `JIRA_URL`, and `account_email` is null for a Data Center PAT. `next_cursor` is Cloud's `nextPageToken`, or on Data Center, which only pages by offset, the next offset as a decimal string; it is null after the last page. Pass it back as `cursor` for the next page. A `search` without `page` or `cursor` answers its rows as before. The extra tool `proposal_status { id, wait_secs ≤ 45 }` waits for a decision and reports `pending`, `approved` (with Jira's result), `rejected` (with the reviewer's reason), `stale` (the issue moved; propose again) or `failed` (Jira refused).

A person decides in a terminal:

```sh
jira-review review                 # each pending proposal with its diff: [a]pprove / [r]eject / [s]kip / [q]uit
jira-review review --list
jira-review review --approve <id>
jira-review review --reject <id> --reason 'not this sprint'
```

Or on a page in a browser:

```sh
jira-review review --web           # prints http://127.0.0.1:<port>/?token=…; --port picks the port
```

The page lists what is waiting, oldest first, with each change's diff, an **Approve and apply** button, and **Reject** with an optional reason. It decides through the same code as the terminal: the same claim, the same stale check, the same outcome the agent's `proposal_status` reads. Reload it to see proposals that arrived since. It runs until ctrl-c, and `--markdown` shows wiki text as Markdown, as in the terminal.

Only you should be able to approve, so the page is closed to everyone else on the machine and to every website:

- it listens on 127.0.0.1 only;
- each launch makes a random token that is printed once, in the address to open. Opening it trades the token for a cookie (`HttpOnly`, `SameSite=Strict`, named for the port) and drops the token from the address bar. Every other request needs that cookie;
- a request whose `Host` is not `127.0.0.1:<port>` or `localhost:<port>` is refused, so a website that rebinds its own name to 127.0.0.1 (DNS rebinding) gets nothing;
- an approval or rejection is a POST that must carry an `Origin` equal to the page's own and a form field only the page holds. Cookies are not kept apart by port, so a page served from another local port could otherwise send one with yours;
- the page sends `frame-ancestors 'none'` and `X-Frame-Options: DENY`, so it cannot be framed, and runs no script.

Anyone who can read the printed address, or your browser's cookies, can approve. That is the same person who could run `jira-review review` as you.

Approving applies the change at once with the reviewer's own credential, running the stale check at that moment. Proposals live in `$JIRA_REVIEW_STATE`, else `$XDG_STATE_HOME/jira-review`, else `~/.local/state/jira-review` (`~/Library/Application Support/jira-review` on macOS), as `pending/<id>.json` and then `decided/<id>.json`. Claiming one is an atomic rename, so two reviewers can't both apply it.

## Tracon plugin (`jira-review serve`)

`jira-review serve --socket <path> --data <dir>` implements the Tracon plugin contract v1: MCP over HTTP on the Unix socket the node names (POST answers with JSON, GET gets 405). The manifest is [`tracon-plugin.toml`](tracon-plugin.toml):

- credential `jira`, pattern `jira-*`, with `select = "profile"`: every tool takes an optional `profile` argument, and the node lends a call only the binding it names, `jira-<profile>`, or `jira` when the call leaves it out (or passes `null` or `""`). A call never carries another profile's credential, and one whose `profile` names a binding the channel does not bind is refused by the node before it is sent. A write's approved call is the agent's call itself, with the arguments its card showed, so it is lent the profile the card named, not a proposal looked up later. A call with no profile on a channel that binds no `jira` is lent nothing and answers `credential jira was not lent to this call`. Credentials are lent per call in the caller context, never read from the start environment. Nodes before Tracon 0.58 refuse the `select` key, so this manifest needs 0.58 or later;
- tools `search`, `issue`, `transitions` (read) and `update`, `transition`, `link`, `create`, `comment` (write; `transition` is `consequential`);
- no `egress`: the host is whatever `JIRA_URL` names, a Jira Cloud site or a Data Center host, so no fixed entry would be true. The node shows egress on cards and does not enforce it;
- `formats = ["jira_wiki"]`: it owns the [`jira_wiki` format](#the-jira_wiki-format). In `tools/list` (from `mcp` too), the string properties that hold wiki markup carry `"x-tracon-format": "jira_wiki"`: `update.description`, `create.description`, `description` inside `create`'s `issues` items, and `comment.body`. The node's approval card shows them as prose, rendered through `tracon/format/convert`, and lists `create`'s `issues` as items.

At start it requires `TRACON_PLUGIN_TOKEN` and `TRACON_NODE_KEY` (and `TRACON_CONTRACT=1` if set), and exits non-zero without them. `initialize`, `ping`, `tools/list` and `tracon/format/convert` need no caller context. Every `tools/call` and `tracon/preview` must carry a verified context. A write runs only when its context carries an `approval_id`, because the node's approval card is the review. It then proposes and applies in one step and records the change under `<data>/proposals/decided/`.

**`tracon/preview`** is sent when an approval card for one of its tools opens, with the call's `name`, `arguments` and a caller context checked as for a `tools/call` except that it needs no approval (checks 1–6). A write is proposed, and nothing is sent to Jira but reads:

```json
{ "previewable": true,
  "summary": "update WRK-12 on https://example.atlassian.net",
  "body": "```diff\nagainst WRK-12 as updated 2026-11-16T09:12:33.000+0000\n\nsummary:\n  - Port it\n  + Port the feed\n```\n",
  "token": "{\"WRK-12\":\"2026-11-16T09:12:33.000+0000\"}" }
```

- `body` is Markdown: the diff `show` prints, without its header, in a code fence longer than any run of backticks inside it;
- `token` is the proposal's `versions` as canonical JSON. For a route of several hops it is `{"route": […], "versions": {…}}`, the route as each hop's statuses and transition. The node keeps it with the approval and hands it back unread;
- a read answers `previewable: false` with a `null` token, since there is nothing to show before it runs;
- a write that would be refused (an unknown transition, a missing issue, a credential not lent) answers JSON-RPC error `-32003` with the reason, and the card shows it.

The approved `tools/call` carries the token back as the context's `preview_token`. The write is applied against it rather than against a fresh read: if an issue it names has moved since the card was shown, nothing is sent, the change is recorded as stale, and the call is refused with JSON-RPC error `-32002` whose `data` is `{"stale": true}`, so the node can ask the agent to call again for a fresh card. A route is found again when the approved call arrives, and if it is not the route the token names (the workflow changed while the issue did not), nothing is sent and the call is refused the same way. An approved call without a token is checked against the issue as it is when the call arrives. Any stale refusal answers the same way.

**`tracon/format/convert`** takes `{ "format": "jira_wiki", "to": "markdown" | "jira_wiki", "text": "…" }` and answers `{ "text": "…" }`, from wiki markup to Markdown or from Markdown to wiki markup. It is a pure function. Any other `format` or `to`, or a missing `text`, is a `-32602` error.

`jira-review mcp` answers both methods with `-32601` (method not found).

### Caller context: wire format v1 (provisional, shared with sql-review and Tracon U3)

On each `tools/call`, `params._meta.tracon` is:

```json
{
  "v": 1,
  "plugin": "jira-review",
  "instance": "<lowercase hex sha256 of TRACON_PLUGIN_TOKEN>",
  "tool": "<tool name, unprefixed>",
  "arguments_sha256": "<lowercase hex sha256 of the canonical JSON of params.arguments>",
  "caller": { "kind": "operator" | "agent" | "plugin", "session": "<id or null>", "harness": "<id or null>" },
  "channel": "<channel>",
  "approval_id": "<id>" | null,
  "preview_token": "<token from tracon/preview>",
  "issued_ms": 0,
  "expires_ms": 0,
  "nonce": "<32 lowercase hex chars>",
  "credentials": { "<credential name>": { "<FIELD>": "<value>" } },
  "signature": "<lowercase hex ed25519 signature>"
}
```

- **Signed bytes:** the ASCII label `tracon/v1/plugin-call`, then a big-endian u32 length and the canonical JSON of the object without `signature`. The signature is ed25519 by the node key, whose public key `TRACON_NODE_KEY` carries as lowercase hex.
- **Canonical JSON** (Tracon's `proto::admin::write_canonical`): object keys sorted by byte order, no whitespace, strings and keys escaped as `serde_json` escapes them, `null` written out. The context holds integers only, and a float in it is refused. A missing `arguments` hashes as `{}`.
- **Checks, in order**, any failure refusing the call with JSON-RPC error `-32001` (`data.check` names it) before anything is read or sent:
  1. `v` is 1 and `plugin` is `jira-review`;
  2. `instance` matches;
  3. `tool` and `arguments_sha256` match the call;
  4. the signature verifies;
  5. `issued_ms − 5 s ≤ now ≤ expires_ms + 5 s`, and the window is at most 60 s;
  6. the nonce hasn't been seen;
  7. a write carries an `approval_id`.
- `preview_token` is optional; absent and `null` mean the same. It is present only on a call dispatched from an approved card whose preview returned a token, and is signed like the rest, so no check changes for it.
- `caller.kind = "plugin"` is reserved: it is accepted, but nothing sends it yet.

[`tests/contract_vectors.json`](tests/contract_vectors.json) pins every byte: a fixed key, token and clock, a valid call with its signing bytes and signature, the same call's context carrying a `preview_token` (`valid_preview_token`), and one tampered case per check. Its `seed` is the **raw ed25519 secret key** (`SigningKey::from_bytes`), *not* a Tracon `Identity` seed, which derives its key through HKDF. sql-review should copy the file into its tests, and Tracon's plugin host should check its signer against it. `cargo test --test contract -- --ignored generate_contract_vectors` regenerates it deterministically.

## The `jira_wiki` format

This crate **owns the `jira_wiki` format**, conversion between Jira wiki markup and Markdown in both directions. The manifest declares it and `serve` answers `tracon/format/convert` for it. Both directions are library functions and CLI subcommands:

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
- paged `search` on both: the page's shape, following `next_cursor` to the last page, the limit of 100, and a bad cursor refused;
- Markdown input on `update`, `create` and `comment`, proposed and sent as wiki markup, and end to end through `serve`;
- each write's propose → show → apply, with the exact requests sent;
- assignee lookup (by email, name, account id and `me`; an ambiguous or unknown name refused) on both Cloud and Data Center, and due dates set and cleared;
- a stale apply refused after the fake's `updated` moves, with nothing sent;
- transitions over several hops on a strict workflow: a route found in the workflow and applied hop by hop, the On-hold detour passed over, no route and an ambiguous route refused with the routes, a `path` checked against the workflow, a `path` without it (Cloud without the permission, and Data Center), a hop retired mid-route and a required field found on the way each stopping the route there, a stale route sending nothing, and a route whose card no longer matches the workflow refused as stale through `serve`;
- multi-issue create with `#N` parents and a refusal partway through;
- both auth schemes' headers;
- credential precedence (environment, then `.env`, then keyring);
- `mcp` over stdio with the proposal flow across processes (approve, reject, stale);
- the `review --web` page: the token traded for a cookie, a wrong token, cookie, `Host` or `Origin` refused, a decision without the page's form field refused, all with nothing sent; approve, reject with a reason, a stale approval and a second approval of the same proposal;
- `serve` over a Unix socket with signed contexts, each refusal and an approved write;
- a call's `profile` read as the manifest's `select` reads it: absent, `null` or `""` uses `jira`, and every tool takes `profile`;
- `tracon/preview`: a write's diff and token with nothing sent, a read's `previewable: false`, a write that would be refused, and a preview whose context does not verify; the approved call applying against the token, and refused as stale (`data.stale`) with nothing sent once the issue moved;
- `tracon/format/convert` both ways, and its `-32602` refusals;
- the `x-tracon-format` markers in `tools/list`, from `serve` and `mcp`;
- the manifest against `tools/list`;
- the contract vectors.

**Not verified:**

- a live Jira Cloud site. `tests/live.rs` is a read-only smoke test, run with `JIRA_LIVE=1 cargo test --test live -- --ignored`;
- **Jira Data Center (bearer PAT): unverified** on a real instance;
- the workflow read for routes (`POST /rest/api/3/workflows/preview`) against a real Cloud site: the fake answers in the shape and with the permission refusal (401) the Cloud REST API's OpenAPI description gives, and which permission schemes grant *View (read-only) workflow* by default is not checked;
- assignee lookup against a real Jira: the fake matches users the way the REST docs describe `user/assignable/search`, and a real site's matching may differ;
- the keyring backends (`login`, `logout`, keyring reads). Only the secret format is tested;
- the release workflow, until the first tag, including the aarch64 Linux cross build;
- the caller-context format, which stays provisional until Tracon's plugin host (U3) signs against the vectors;
- `tracon/preview`, `preview_token` and `tracon/format/convert` against a real node, until Tracon's U6 and U8 send them.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Releases

Releases are cut with [release-please](https://github.com/googleapis/release-please). Each merge to `main` updates a release PR that bumps the version in `Cargo.toml`, `Cargo.lock` and `tracon-plugin.toml` together and writes `CHANGELOG.md` from the conventional-commit PR titles (`feat` bumps the minor version, `fix` the patch, and before 1.0 a breaking change bumps the minor). Merging the release PR tags `v<version>`, creates the GitHub release, and the release workflow attaches `jira-review-<version>-<target>.tar.gz` for each target with `checksums.txt` and build provenance attestations.

MIT licensed; see [LICENSE](LICENSE).
