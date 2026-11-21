//! `jira-review review --web`: the same review as the terminal's, on a page
//! served to this machine only. It lists what `mcp` is waiting on with each
//! change's diff, and approves or rejects through [`crate::review`], so the
//! claim and the stale check are the terminal's.
//!
//! Who may use it: the listener is bound to 127.0.0.1, and each launch
//! makes a token that is printed once, in the address to open. That address
//! trades the token for a cookie (`HttpOnly`, `SameSite=Strict`), and every
//! other request needs the cookie. A request naming any other Host is
//! refused, so a website rebinding its name to 127.0.0.1 gets nothing. A
//! decision also needs its Origin to be this page's and a form field only
//! this page carries: cookies are not kept apart by port, so a page served
//! from another local port could otherwise send one.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::change::{self, Proposal, Render, Status, Tone};
use crate::jira::Jira;
use crate::review;
use crate::store::{Place, Store};

/// The most decisions the page keeps showing.
const NOTICES: usize = 20;

/// A review page and what it reviews with.
pub struct Page {
    store: Store,
    jira: Jira,
    render: Render,
    port: u16,
    token: String,
    session: String,
    csrf: String,
    /// Decided here, newest last: the id, and the error if it was refused.
    notices: Mutex<Vec<(String, Option<String>)>>,
}

/// 244 random bits, from two v4 UUIDs.
fn secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Compare secrets without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

impl Page {
    /// A page for a listener on 127.0.0.1:`port`, with fresh secrets.
    pub fn new(store: Store, jira: Jira, markdown: bool, port: u16) -> Self {
        Self {
            store,
            jira,
            render: Render {
                color: false,
                markdown,
            },
            port,
            token: secret(),
            session: secret(),
            csrf: secret(),
            notices: Mutex::new(Vec::new()),
        }
    }

    /// The address to open: the only place the token is given out.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?token={}", self.port, self.token)
    }

    /// Named for the port, so two pages open at once keep their own.
    fn cookie(&self) -> String {
        format!("jira_review_{}", self.port)
    }

    fn host_allowed(&self, host: &str) -> bool {
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    fn signed_in(&self, headers: &HeaderMap) -> bool {
        let name = self.cookie();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|c| c.trim().split_once('='))
            .any(|(k, v)| k == name && same(v, &self.session))
    }

    fn notice(&self, id: &str, error: Option<String>) {
        let mut notices = self.notices.lock().expect("notices lock");
        notices.push((id.to_string(), error));
        let over = notices.len().saturating_sub(NOTICES);
        notices.drain(..over);
    }
}

/// Bind 127.0.0.1:`port` (0 picks a free one) and serve the page until
/// ctrl-c, after printing the address to open.
pub async fn run(store: Store, jira: Jira, markdown: bool, port: u16) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let page = Arc::new(Page::new(store, jira, markdown, port));
    println!("review page: {}", page.url());
    println!("only this machine can reach it; ctrl-c stops it");
    axum::serve(listener, router(page))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}

pub fn router(page: Arc<Page>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/approve/{id}", post(approve))
        .route("/reject/{id}", post(reject))
        .fallback(|| async { refused(StatusCode::NOT_FOUND, "nothing here") })
        .layer(middleware::from_fn_with_state(page.clone(), guard))
        .with_state(page)
}

/// Every request: refuse a Host that is not this page's, and send the
/// headers that keep the page out of frames, caches and referrers.
async fn guard(State(page): State<Arc<Page>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    let mut res = if host.is_some_and(|h| page.host_allowed(h)) {
        next.run(req).await
    } else {
        refused(
            StatusCode::FORBIDDEN,
            "this page answers only to 127.0.0.1 or localhost on its own port",
        )
    };
    let h = res.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        // Not `no-referrer`: under it a browser sends `Origin: null` on the
        // page's own form posts, and the Origin check would refuse them.
        (header::REFERRER_POLICY, "same-origin"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    res
}

fn refused(status: StatusCode, why: &str) -> Response {
    (status, format!("jira-review: {why}\n")).into_response()
}

fn see_other(location: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response()
}

async fn index(
    State(page): State<Arc<Page>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Some(token) = query.get("token") {
        if !same(token, &page.token) {
            return refused(StatusCode::FORBIDDEN, "that is not this page's token");
        }
        // Trade the token for a cookie, and take it out of the address bar.
        let cookie = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict",
            page.cookie(),
            page.session
        );
        let mut res = see_other("/");
        res.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("a cookie is a header value"),
        );
        return res;
    }
    if !page.signed_in(&headers) {
        return refused(
            StatusCode::UNAUTHORIZED,
            "open the address `jira-review review --web` printed",
        );
    }
    match render_index(&page) {
        Ok(html) => Html(html).into_response(),
        Err(e) => refused(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(Deserialize)]
struct Decision {
    csrf: String,
    #[serde(default)]
    reason: String,
}

/// Whether a decision may go ahead: signed in, sent by this page (its
/// Origin, which browsers send on every POST), and carrying this page's
/// form field.
fn allowed(
    page: &Page,
    headers: &HeaderMap,
    form: Result<Form<Decision>, FormRejection>,
) -> Result<Decision, (StatusCode, &'static str)> {
    if !page.signed_in(headers) {
        return Err((StatusCode::UNAUTHORIZED, "not signed in"));
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok());
    if host.is_none() || origin != host.map(|h| format!("http://{h}")).as_deref() {
        return Err((
            StatusCode::FORBIDDEN,
            "a decision must come from this page (Origin)",
        ));
    }
    let Ok(Form(d)) = form else {
        return Err((StatusCode::BAD_REQUEST, "not a decision"));
    };
    if !same(&d.csrf, &page.csrf) {
        return Err((StatusCode::FORBIDDEN, "a decision must come from this page"));
    }
    Ok(d)
}

async fn approve(
    State(page): State<Arc<Page>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    form: Result<Form<Decision>, FormRejection>,
) -> Response {
    if let Err((status, why)) = allowed(&page, &headers, form) {
        return refused(status, why);
    }
    let done = review::approve(&page.store, &id, &page.jira).await;
    page.notice(&id, done.err());
    see_other("/")
}

async fn reject(
    State(page): State<Arc<Page>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    form: Result<Form<Decision>, FormRejection>,
) -> Response {
    let d = match allowed(&page, &headers, form) {
        Ok(d) => d,
        Err((status, why)) => return refused(status, why),
    };
    let reason = Some(d.reason.trim().to_string()).filter(|r| !r.is_empty());
    let done = review::reject(&page.store, &id, reason);
    page.notice(&id, done.err());
    see_other("/")
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn proposal_html(page: &Page, p: &Proposal) -> String {
    let id = escape(&p.id);
    let csrf = escape(&page.csrf);
    let mut s = format!(
        "<section><h2>{}</h2><p class=\"meta\">id {id}, proposed {}</p><pre>",
        escape(&change::title(p)),
        escape(&p.created)
    );
    for line in change::change_lines(p, page.render) {
        let (class, mark) = match line.tone {
            Tone::Title | Tone::Plain => ("", ""),
            Tone::Removed => ("del", "- "),
            Tone::Added => ("ins", "+ "),
            Tone::Context => ("ctx", "  "),
        };
        // Each line is a block of its own, so no newline between them.
        let _ = write!(
            s,
            "<span class=\"{class}\">{mark}{}</span>",
            escape(&line.text)
        );
    }
    let _ = write!(
        s,
        "</pre><div class=\"actions\">\
         <form method=\"post\" action=\"/approve/{id}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <button class=\"approve\">Approve and apply</button></form>\
         <form method=\"post\" action=\"/reject/{id}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <input name=\"reason\" placeholder=\"reason (optional)\" aria-label=\"reason\">\
         <button>Reject</button></form></div></section>"
    );
    s
}

fn notice_html(page: &Page, id: &str, error: Option<&str>) -> String {
    let (class, text) = match (error, page.store.get(id)) {
        (Some(e), _) => ("refused", format!("{id}: {e}")),
        (None, Ok(Some((Place::Decided, p)))) => {
            let class = match p.outcome.as_ref().map(|o| o.status) {
                Some(Status::Approved) => "approved",
                Some(Status::Rejected) => "decided",
                _ => "refused",
            };
            (class, review::describe(&p))
        }
        (None, Ok(_)) => ("decided", format!("{id} is not decided")),
        (None, Err(e)) => ("refused", e),
    };
    format!(
        "<li class=\"{class}\"><pre>{}</pre></li>",
        escape(text.trim_end())
    )
}

fn render_index(page: &Page) -> Result<String, String> {
    let pending = page.store.pending()?;
    let mut body = String::new();
    let notices = page.notices.lock().expect("notices lock").clone();
    if !notices.is_empty() {
        body.push_str("<h2>Decided here</h2><ul class=\"notices\">");
        for (id, error) in notices.iter().rev() {
            body.push_str(&notice_html(page, id, error.as_deref()));
        }
        body.push_str("</ul>");
    }
    let _ = write!(
        body,
        "<p class=\"count\">{} waiting for review, oldest first. Approving applies the change \
         with your credential, and only if no issue it depends on has moved since. \
         <a href=\"/\">Reload</a></p>",
        pending.len()
    );
    for p in &pending {
        body.push_str(&proposal_html(page, p));
    }
    Ok(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>jira-review</title><style>{STYLE}</style></head>\
         <body><main><h1>jira-review</h1>{body}</main></body></html>"
    ))
}

const STYLE: &str = "\
:root{--bg:#fff;--fg:#1d1d1f;--muted:#666;--line:#ddd;--del:#b42318;--del-bg:#fdecea;\
--ins:#067647;--ins-bg:#e7f6ec;color-scheme:light dark}\
@media (prefers-color-scheme:dark){:root{--bg:#161618;--fg:#e8e8ea;--muted:#9a9aa0;\
--line:#333;--del:#ff8a80;--del-bg:#3a1d1b;--ins:#7ee2a8;--ins-bg:#16301f}}\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}\
main{max-width:60rem;margin:0 auto;padding:1rem}\
h1{font-size:1.3rem}h2{font-size:1.05rem;margin:0 0 .25rem;overflow-wrap:anywhere}\
section{border:1px solid var(--line);border-radius:6px;padding:.75rem 1rem;margin:1rem 0}\
.meta,.count{color:var(--muted);margin:0 0 .5rem}a{color:inherit}\
pre{margin:0;padding:.5rem;overflow-x:auto;font:13px/1.4 ui-monospace,monospace;\
white-space:pre-wrap;overflow-wrap:anywhere}\
pre span{display:block;min-height:1.4em}\
.del{color:var(--del);background:var(--del-bg)}\
.ins{color:var(--ins);background:var(--ins-bg)}.ctx{color:var(--muted)}\
.actions{display:flex;flex-wrap:wrap;gap:.5rem;margin-top:.75rem}\
form{display:flex;gap:.5rem;flex-wrap:wrap}input{font:inherit;padding:.3rem .5rem}\
button{font:inherit;padding:.3rem .8rem;cursor:pointer}\
.approve{background:var(--ins);color:var(--bg);border:0;border-radius:4px}\
.notices{list-style:none;padding:0}.notices li{border-left:4px solid var(--line);margin:.5rem 0}\
.notices .approved{border-color:var(--ins)}.notices .refused{border-color:var(--del)}";
