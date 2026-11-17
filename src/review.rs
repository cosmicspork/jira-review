//! `jira-review review`: the person on the other side of `mcp`. Lists what
//! is waiting, shows each change, and applies what they approve with their
//! own credential, the stale check running then.

use std::io::{BufRead, Write};

use crate::change::{self, Outcome, Proposal, Render, Status};
use crate::jira::Jira;
use crate::store::Store;

/// One line per pending proposal, oldest first.
pub fn list(store: &Store) -> Result<String, String> {
    let pending = store.pending()?;
    if pending.is_empty() {
        return Ok("nothing is waiting for review\n".into());
    }
    Ok(pending
        .iter()
        .map(|p| {
            format!(
                "{}  {} {}  {}\n",
                p.id,
                p.kind.name(),
                p.subject(),
                p.created
            )
        })
        .collect())
}

/// Claim `id` and apply it. Whatever happens is written back as its
/// outcome, so the agent waiting on it hears.
pub async fn approve(store: &Store, id: &str, jira: &Jira) -> Result<Proposal, String> {
    let p = store.claim(id)?;
    let outcome = Outcome::of(change::apply(&p, jira, None).await);
    store.decide(p, outcome)
}

pub fn reject(store: &Store, id: &str, reason: Option<String>) -> Result<Proposal, String> {
    let p = store.claim(id)?;
    store.decide(p, Outcome::new(Status::Rejected, None, reason))
}

/// How a decided proposal turned out, for the reviewer.
pub fn describe(p: &Proposal) -> String {
    let Some(o) = &p.outcome else {
        return format!("{} is not decided\n", p.id);
    };
    let status = format!("{:?}", o.status).to_lowercase();
    let mut s = format!("{}: {status}", p.id);
    if let Some(reason) = &o.reason {
        s.push_str(&format!(": {reason}"));
    }
    s.push('\n');
    if let Some(result) = &o.result {
        s.push_str(&serde_json::to_string_pretty(result).expect("JSON serializes"));
        s.push('\n');
    }
    s
}

fn ask(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Go through what is pending, oldest first, asking about each.
pub async fn interactive(store: &Store, jira: &Jira, render: Render) -> Result<(), String> {
    let pending = store.pending()?;
    if pending.is_empty() {
        println!("nothing is waiting for review");
        return Ok(());
    }
    let total = pending.len();
    for (i, p) in pending.iter().enumerate() {
        println!("\n[{}/{total}]", i + 1);
        print!("{}", change::render(p, render));
        loop {
            let Some(answer) = ask("\n[a]pprove / [r]eject / [s]kip / [q]uit: ") else {
                return Ok(());
            };
            match answer.as_str() {
                "a" | "approve" => {
                    match approve(store, &p.id, jira).await {
                        Ok(done) => print!("{}", describe(&done)),
                        Err(e) => println!("{e}"),
                    }
                    break;
                }
                "r" | "reject" => {
                    let reason = ask("reason (optional): ").filter(|r| !r.is_empty());
                    match reject(store, &p.id, reason) {
                        Ok(done) => print!("{}", describe(&done)),
                        Err(e) => println!("{e}"),
                    }
                    break;
                }
                "s" | "skip" => break,
                "q" | "quit" => return Ok(()),
                _ => println!("a, r, s or q"),
            }
        }
    }
    Ok(())
}
