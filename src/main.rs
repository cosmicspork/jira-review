use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use jira_review::auth::{self, Credential};
use jira_review::change::{self, ApplyError, Kind, Outcome, Proposal, Render};
use jira_review::jira::Jira;
use jira_review::store::{self, Store};
use jira_review::{mcp, review, stdio, wiki};

/// Jira from the command line and from agents, where no write is applied
/// without review.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The keyring profile to read the credential from.
    #[arg(long, global = true, env = "JIRA_REVIEW_PROFILE", default_value = auth::DEFAULT_PROFILE)]
    profile: String,
    /// A .env file to read the credential from, in place of ./.env.
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Table,
}

#[derive(Args)]
struct Propose {
    /// Where to write the change file: a path, or - for stdout. ./<id>.json
    /// unless given.
    #[arg(long)]
    out: Option<String>,
    /// Text inputs are Markdown, converted to Jira wiki markup.
    #[arg(long)]
    markdown: bool,
}

#[derive(Args, Clone)]
struct IssueFields {
    #[arg(long)]
    summary: Option<String>,
    #[arg(long, conflicts_with = "description_file")]
    description: Option<String>,
    /// Read the description from a file (- for stdin).
    #[arg(long)]
    description_file: Option<PathBuf>,
    #[arg(long)]
    priority: Option<String>,
    /// Who to assign it: an account id, username, email, display name, or
    /// "me". Found among the users Jira lets it be assigned to.
    #[arg(long, conflicts_with = "unassign")]
    assignee: Option<String>,
    /// Leave it unassigned.
    #[arg(long)]
    unassign: bool,
    /// The due date, as YYYY-MM-DD.
    #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "clear_due")]
    due: Option<String>,
    /// Clear the due date.
    #[arg(long)]
    clear_due: bool,
    /// Comma-separated labels.
    #[arg(long, value_delimiter = ',')]
    labels: Option<Vec<String>>,
    #[arg(long)]
    parent: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Search issues with JQL.
    Search {
        jql: String,
        #[arg(long, default_value_t = 20)]
        limit: u64,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Read one issue.
    Issue {
        key: String,
        /// Show the description and comments as Markdown.
        #[arg(long)]
        markdown: bool,
    },
    /// The transitions an issue offers now, with the fields they require.
    Transitions {
        key: String,
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
    /// Propose an edit to an issue's fields.
    Update {
        key: String,
        #[command(flatten)]
        fields: IssueFields,
        #[arg(long = "add-label", conflicts_with = "labels")]
        add_labels: Vec<String>,
        #[arg(long = "remove-label", conflicts_with = "labels")]
        remove_labels: Vec<String>,
        #[command(flatten)]
        propose: Propose,
    },
    /// Propose a transition, by id or name.
    Transition {
        key: String,
        transition: String,
        /// A field the transition takes, as id=JSON (or id=text).
        #[arg(long = "field")]
        fields: Vec<String>,
        #[command(flatten)]
        propose: Propose,
    },
    /// Propose a link: FROM <type> TO, e.g. WRK-1 Blocks WRK-2.
    Link {
        from: String,
        #[arg(value_name = "TYPE")]
        kind: String,
        to: String,
        #[command(flatten)]
        propose: Propose,
    },
    /// Propose a new issue, or several from a JSON file.
    Create {
        #[arg(long, required_unless_present = "file")]
        project: Option<String>,
        #[arg(long = "type", required_unless_present = "file")]
        kind: Option<String>,
        #[command(flatten)]
        fields: IssueFields,
        /// The create arguments as JSON: an object, or a list of issues.
        #[arg(long, conflicts_with_all = ["project", "kind"])]
        file: Option<PathBuf>,
        #[command(flatten)]
        propose: Propose,
    },
    /// Propose a comment.
    Comment {
        key: String,
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        body: Option<String>,
        /// Read the body from a file (- for stdin).
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[command(flatten)]
        propose: Propose,
    },
    /// Render a change file (or a stored proposal) as a diff.
    Show {
        change: String,
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        no_color: bool,
    },
    /// Apply a change file, refused if an issue moved since it was proposed.
    Apply { change: String },
    /// Approve or reject the proposals `mcp` is waiting on.
    Review {
        #[arg(long, conflicts_with_all = ["approve", "reject"])]
        list: bool,
        #[arg(long, conflicts_with = "reject")]
        approve: Option<String>,
        #[arg(long)]
        reject: Option<String>,
        #[arg(long, requires = "reject")]
        reason: Option<String>,
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        no_color: bool,
    },
    /// Serve MCP on stdin and stdout for any agent client.
    Mcp,
    /// Serve the Tracon plugin contract on a Unix socket.
    Serve {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        data: PathBuf,
    },
    /// Convert between Jira wiki markup and Markdown.
    Wiki {
        #[command(subcommand)]
        direction: WikiDirection,
    },
    /// Store a credential in the OS keyring under the profile.
    Login,
    /// Remove the profile's credential from the keyring.
    Logout,
}

#[derive(Subcommand)]
enum WikiDirection {
    /// Jira wiki markup to Markdown.
    ToMarkdown { file: Option<PathBuf> },
    /// Markdown to Jira wiki markup.
    FromMarkdown { file: Option<PathBuf> },
}

/// A failure, and the exit code it ends with.
struct Fail(u8, String);

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail(1, e)
    }
}

/// Exit code for a change refused because an issue moved since.
const STALE: u8 = 3;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail(code, e)) => {
            eprintln!("jira-review: {e}");
            ExitCode::from(code)
        }
    }
}

fn read_input(file: Option<&Path>) -> Result<String, String> {
    let mut s = String::new();
    match file {
        None => std::io::stdin().read_to_string(&mut s).map(drop),
        Some(p) if p == Path::new("-") => std::io::stdin().read_to_string(&mut s).map(drop),
        Some(p) => std::fs::read_to_string(p).map(|t| s = t),
    }
    .map_err(|e| {
        format!(
            "{}: {e}",
            file.map_or("stdin".into(), |p| p.display().to_string())
        )
    })?;
    Ok(s)
}

fn print_json(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("JSON serializes")
    );
}

fn color(no_color: bool) -> bool {
    !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows.first().map_or(0, Vec::len))
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let cells: Vec<String> = r
                .iter()
                .zip(&widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect();
            format!("{}\n", cells.join("  ").trim_end())
        })
        .collect()
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(cell).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

impl IssueFields {
    fn into_args(self, args: &mut Map<String, Value>, markdown: bool) -> Result<(), String> {
        let text = |s: String| if markdown { wiki::from_markdown(&s) } else { s };
        if let Some(s) = self.summary {
            args.insert("summary".into(), json!(s));
        }
        let description = match self.description_file {
            Some(f) => Some(read_input(Some(&f))?),
            None => self.description,
        };
        if let Some(d) = description {
            args.insert("description".into(), json!(text(d)));
        }
        if let Some(p) = self.priority {
            args.insert("priority".into(), json!(p));
        }
        if let Some(a) = self.assignee {
            args.insert("assignee".into(), json!(a));
        } else if self.unassign {
            args.insert("assignee".into(), Value::Null);
        }
        if let Some(d) = self.due {
            args.insert("duedate".into(), json!(d));
        } else if self.clear_due {
            args.insert("duedate".into(), Value::Null);
        }
        if let Some(l) = self.labels {
            args.insert("labels".into(), json!(l));
        }
        if let Some(p) = self.parent {
            args.insert("parent".into(), json!(p));
        }
        Ok(())
    }
}

/// Propose a write and write its change file: nothing is sent to Jira.
async fn propose(cli: &Cli, kind: Kind, args: Value, options: &Propose) -> Result<(), Fail> {
    let jira = Jira::new(auth::resolve(cli.env_file.as_deref(), &cli.profile)?);
    let p = change::propose(kind, &args, &jira).await?;
    let out = options
        .out
        .clone()
        .unwrap_or_else(|| format!("{}.json", p.id));
    if out == "-" {
        print!("{}", p.to_json());
        eprint!("{}", change::render(&p, Render::default()));
        return Ok(());
    }
    std::fs::write(&out, p.to_json()).map_err(|e| format!("{out}: {e}"))?;
    print!(
        "{}",
        change::render(
            &p,
            Render {
                color: color(false),
                markdown: false,
            }
        )
    );
    println!("\nwrote {out}; nothing is sent until: jira-review apply {out}");
    Ok(())
}

/// A change by file path, or else by id in the proposal store.
fn load(change: &str) -> Result<(Option<PathBuf>, Proposal), String> {
    let path = PathBuf::from(change);
    if path.is_file() {
        let s = std::fs::read_to_string(&path).map_err(|e| format!("{change}: {e}"))?;
        return Ok((Some(path), Proposal::from_json(&s)?));
    }
    let store = Store::open(store::default_root()?)?;
    match store.get(change)? {
        Some((_, p)) => Ok((None, p)),
        None => Err(format!(
            "{change} is neither a change file nor a proposal id"
        )),
    }
}

async fn run(cli: Cli) -> Result<(), Fail> {
    let credential = || auth::resolve(cli.env_file.as_deref(), &cli.profile);
    let jira = || credential().map(Jira::new);
    match &cli.command {
        Command::Search { jql, limit, format } => {
            let v = jira()?.search(jql, *limit).await?;
            match format {
                Format::Json => print_json(&v),
                Format::Table => {
                    let mut rows = vec![
                        ["key", "type", "status", "priority", "assignee", "summary"]
                            .map(String::from)
                            .to_vec(),
                    ];
                    for i in v["issues"].as_array().into_iter().flatten() {
                        rows.push(
                            ["key", "type", "status", "priority", "assignee", "summary"]
                                .iter()
                                .map(|f| cell(&i[*f]))
                                .collect(),
                        );
                    }
                    print!("{}", table(&rows));
                }
            }
        }
        Command::Issue { key, markdown } => {
            let key = json!(key);
            let key = jira_review::args::issue_key(Some(&key), "key")?;
            let mut v = jira()?.issue(key).await?;
            if *markdown {
                if let Some(d) = v["description"].as_str() {
                    v["description"] = json!(wiki::to_markdown(d));
                }
                for c in v["comments"].as_array_mut().into_iter().flatten() {
                    if let Some(b) = c["body"].as_str() {
                        c["body"] = json!(wiki::to_markdown(b));
                    }
                }
            }
            print_json(&v);
        }
        Command::Transitions { key, format } => {
            let key = json!(key);
            let key = jira_review::args::issue_key(Some(&key), "key")?;
            let v = jira()?.transitions(key).await?;
            match format {
                Format::Json => print_json(&v),
                Format::Table => {
                    let mut rows =
                        vec![["id", "name", "to", "requires"].map(String::from).to_vec()];
                    for t in v["transitions"].as_array().into_iter().flatten() {
                        let requires: Vec<String> = t["required_fields"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|f| cell(&f["id"]))
                            .collect();
                        rows.push(vec![
                            cell(&t["id"]),
                            cell(&t["name"]),
                            cell(&t["to"]),
                            if requires.is_empty() {
                                "-".into()
                            } else {
                                requires.join(",")
                            },
                        ]);
                    }
                    print!("{}", table(&rows));
                }
            }
        }
        Command::Update {
            key,
            fields,
            add_labels,
            remove_labels,
            propose: options,
        } => {
            let mut args = Map::new();
            args.insert("key".into(), json!(key));
            fields.clone().into_args(&mut args, options.markdown)?;
            if !add_labels.is_empty() {
                args.insert("add_labels".into(), json!(add_labels));
            }
            if !remove_labels.is_empty() {
                args.insert("remove_labels".into(), json!(remove_labels));
            }
            propose(&cli, Kind::Update, Value::Object(args), options).await?;
        }
        Command::Transition {
            key,
            transition,
            fields,
            propose: options,
        } => {
            let mut args = json!({ "key": key, "transition": transition });
            if !fields.is_empty() {
                let mut given = Map::new();
                for f in fields {
                    let (id, value) = f
                        .split_once('=')
                        .ok_or_else(|| format!("--field {f}: expected id=value"))?;
                    let value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
                    given.insert(id.trim().to_string(), value);
                }
                args["fields"] = Value::Object(given);
            }
            propose(&cli, Kind::Transition, args, options).await?;
        }
        Command::Link {
            from,
            kind,
            to,
            propose: options,
        } => {
            let args = json!({ "from": from, "to": to, "type": kind });
            propose(&cli, Kind::Link, args, options).await?;
        }
        Command::Create {
            project,
            kind,
            fields,
            file,
            propose: options,
        } => {
            let args = match file {
                Some(f) => {
                    let s = read_input(Some(f))?;
                    let v: Value =
                        serde_json::from_str(&s).map_err(|e| format!("{}: {e}", f.display()))?;
                    let mut v = match v {
                        Value::Array(items) => json!({ "issues": items }),
                        other => other,
                    };
                    if options.markdown {
                        let convert = |i: &mut Value| {
                            if let Some(d) = i["description"].as_str() {
                                i["description"] = json!(wiki::from_markdown(d));
                            }
                        };
                        match v["issues"].as_array_mut() {
                            Some(items) => items.iter_mut().for_each(convert),
                            None => convert(&mut v),
                        }
                    }
                    v
                }
                None => {
                    let mut args = Map::new();
                    args.insert("project".into(), json!(project));
                    args.insert("type".into(), json!(kind));
                    fields.clone().into_args(&mut args, options.markdown)?;
                    Value::Object(args)
                }
            };
            propose(&cli, Kind::Create, args, options).await?;
        }
        Command::Comment {
            key,
            body,
            body_file,
            propose: options,
        } => {
            let body = match body_file {
                Some(f) => read_input(Some(f))?,
                None => body.clone().unwrap_or_default(),
            };
            let body = if options.markdown {
                wiki::from_markdown(&body)
            } else {
                body
            };
            propose(
                &cli,
                Kind::Comment,
                json!({ "key": key, "body": body }),
                options,
            )
            .await?;
        }
        Command::Show {
            change,
            markdown,
            no_color,
        } => {
            let (_, p) = load(change)?;
            let render = Render {
                color: color(*no_color),
                markdown: *markdown,
            };
            print!("{}", change::render(&p, render));
            if p.outcome.is_some() {
                print!("\n{}", review::describe(&p));
            }
        }
        Command::Apply { change } => {
            let (path, p) = load(change)?;
            let jira = jira()?;
            let Some(path) = path else {
                // A stored proposal is decided through the store, so an
                // agent waiting on it hears.
                let store = Store::open(store::default_root()?)?;
                let done = review::approve(&store, change, &jira).await?;
                print!("{}", review::describe(&done));
                return outcome_code(&done);
            };
            if let Some(o) = &p.outcome {
                return Err(Fail(
                    1,
                    format!(
                        "{} was already decided ({:?}, {})",
                        path.display(),
                        o.status,
                        o.decided
                    ),
                ));
            }
            let applied = change::apply(&p, &jira, None).await;
            if let Err(ApplyError::Stale { .. } | ApplyError::Invalid(_)) = &applied {
                // Nothing was sent: leave the file as it was.
                let e = applied.unwrap_err();
                let code = if matches!(e, ApplyError::Stale { .. }) {
                    STALE
                } else {
                    1
                };
                return Err(Fail(code, e.to_string()));
            }
            let mut p = p;
            p.outcome = Some(Outcome::of(applied));
            std::fs::write(&path, p.to_json()).map_err(|e| format!("{}: {e}", path.display()))?;
            print!("{}", review::describe(&p));
            return outcome_code(&p);
        }
        Command::Review {
            list,
            approve,
            reject,
            reason,
            markdown,
            no_color,
        } => {
            let store = Store::open(store::default_root()?)?;
            if *list {
                print!("{}", review::list(&store)?);
            } else if let Some(id) = approve {
                let done = review::approve(&store, id, &jira()?).await?;
                print!("{}", review::describe(&done));
                return outcome_code(&done);
            } else if let Some(id) = reject {
                let done = review::reject(&store, id, reason.clone())?;
                print!("{}", review::describe(&done));
            } else {
                let render = Render {
                    color: color(*no_color),
                    markdown: *markdown,
                };
                review::interactive(&store, &jira()?, render).await?;
            }
        }
        Command::Mcp => {
            let host = mcp::Standalone {
                credential: credential(),
                store: Store::open(store::default_root()?)?,
            };
            stdio::run(Arc::new(host))
                .await
                .map_err(|e| format!("stdio: {e}"))?;
        }
        Command::Serve { socket, data } => {
            #[cfg(unix)]
            jira_review::serve::run(socket, data).await?;
            #[cfg(not(unix))]
            return Err(Fail(
                1,
                format!("serve needs Unix sockets ({socket:?}, {data:?})"),
            ));
        }
        Command::Wiki { direction } => match direction {
            WikiDirection::ToMarkdown { file } => {
                println!("{}", wiki::to_markdown(&read_input(file.as_deref())?));
            }
            WikiDirection::FromMarkdown { file } => {
                println!("{}", wiki::from_markdown(&read_input(file.as_deref())?));
            }
        },
        Command::Login => login(&cli.profile)?,
        Command::Logout => {
            if auth::keyring_delete(&cli.profile)? {
                println!("removed {}/{}", auth::KEYRING_SERVICE, cli.profile);
            } else {
                println!("no credential stored for {}", cli.profile);
            }
        }
    }
    Ok(())
}

/// Exit with the code a decided proposal's status calls for.
fn outcome_code(p: &Proposal) -> Result<(), Fail> {
    match p.outcome.as_ref().map(|o| o.status) {
        Some(change::Status::Stale) => Err(Fail(STALE, format!("{} was not applied", p.id))),
        Some(change::Status::Failed) => Err(Fail(1, format!("{} failed", p.id))),
        _ => Ok(()),
    }
}

fn line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    let mut s = String::new();
    std::io::stdin()
        .read_line(&mut s)
        .map_err(|e| e.to_string())?;
    Ok(s.trim().to_string())
}

fn login(profile: &str) -> Result<(), String> {
    let url = line("Jira URL (e.g. https://example.atlassian.net): ")?;
    let email = line("Email, for Jira Cloud (blank for a Data Center personal access token): ")?;
    let (prompt, field) = if email.is_empty() {
        ("Personal access token: ", "JIRA_PAT")
    } else {
        ("API token: ", "JIRA_TOKEN")
    };
    let secret = rpassword::prompt_password(prompt).map_err(|e| e.to_string())?;
    let credential = Credential::from_fields(
        |k| match k {
            "JIRA_URL" => Some(url.clone()),
            "JIRA_EMAIL" if !email.is_empty() => Some(email.clone()),
            k if k == field => Some(secret.clone()),
            _ => None,
        },
        "what was entered",
    )?;
    auth::keyring_store(profile, &credential)?;
    println!(
        "stored in the keyring as {}/{profile}",
        auth::KEYRING_SERVICE
    );
    Ok(())
}
