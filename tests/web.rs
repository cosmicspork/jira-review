mod support;

use std::sync::Arc;

use jira_review::change::{self, Kind, Status};
use jira_review::store::{Place, Store};
use jira_review::web::{Page, router};
use serde_json::{Value, json};
use support::Fake;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Request headers, as a test sets them.
type Headers<'a> = Vec<(&'a str, String)>;

/// A review page on a free port over a fresh store, with the fake behind it.
struct Served {
    port: u16,
    token: String,
    store: Store,
    _dir: tempfile::TempDir,
}

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Served {
    async fn start(fake: &Fake) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let page = Arc::new(Page::new(store.clone(), fake.client(), false, port));
        let url = page.url();
        let token = url.split_once("token=").unwrap().1.to_string();
        tokio::spawn(async move { axum::serve(listener, router(page)).await.unwrap() });
        Self {
            port,
            token,
            store,
            _dir: dir,
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.host())
    }

    /// One raw HTTP/1.1 exchange, so the test sets Host and Origin itself.
    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> Answer {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if method == "POST" {
            req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).await.unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Answer {
            status,
            headers,
            body: body.to_string(),
        }
    }

    async fn get(&self, path: &str, headers: &[(&str, String)]) -> Answer {
        self.send("GET", path, headers, "").await
    }

    /// Trade the token for the page's cookie.
    async fn sign_in(&self) -> String {
        let a = self
            .get(&format!("/?token={}", self.token), &[("Host", self.host())])
            .await;
        assert_eq!(a.status, 303);
        assert_eq!(a.header("location"), Some("/"));
        let cookie = a.header("set-cookie").unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        cookie.split(';').next().unwrap().to_string()
    }

    /// The signed-in page, and the form field its decisions carry.
    async fn index(&self, cookie: &str) -> (String, String) {
        let a = self
            .get("/", &[("Host", self.host()), ("Cookie", cookie.into())])
            .await;
        assert_eq!(a.status, 200, "{}", a.body);
        let csrf = a
            .body
            .split_once("name=\"csrf\" value=\"")
            .map(|(_, rest)| rest.split('"').next().unwrap().to_string())
            .unwrap_or_default();
        (a.body, csrf)
    }

    async fn decide(
        &self,
        action: &str,
        id: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> Answer {
        self.send("POST", &format!("/{action}/{id}"), headers, body)
            .await
    }

    fn status(&self, id: &str) -> Option<Status> {
        match self.store.get(id).unwrap() {
            Some((Place::Decided, p)) => p.outcome.map(|o| o.status),
            _ => None,
        }
    }
}

async fn put(fake: &Fake, store: &Store, kind: Kind, args: Value) -> String {
    let p = change::propose(kind, &args, &fake.client()).await.unwrap();
    store.put(&p).unwrap();
    fake.clear();
    p.id
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_opens_only_with_its_token_and_only_on_its_own_host() {
    let fake = Fake::start().await;
    let page = Served::start(&fake).await;
    let id = put(
        &fake,
        &page.store,
        Kind::Comment,
        json!({ "key": "WRK-1", "body": "Shipped <b>today</b>." }),
    )
    .await;

    let host = || ("Host", page.host());
    assert_eq!(page.get("/", &[host()]).await.status, 401);
    let wrong = page.get("/?token=nope", &[host()]).await;
    assert_eq!(wrong.status, 403);
    assert!(wrong.header("set-cookie").is_none());
    // A guessed cookie is no cookie.
    let guessed = format!("jira_review_{}={}", page.port, page.token);
    assert_eq!(
        page.get("/", &[host(), ("Cookie", guessed)]).await.status,
        401
    );

    let cookie = page.sign_in().await;
    let a = page.get("/", &[host(), ("Cookie", cookie.clone())]).await;
    assert_eq!(a.status, 200);
    assert!(a.body.contains(&id));
    assert!(a.body.contains("comment WRK-1 on"));
    // Proposed text is escaped, never markup.
    assert!(
        a.body
            .contains("<span class=\"ins\">+ Shipped &lt;b&gt;today&lt;/b&gt;.</span>"),
        "{}",
        a.body
    );
    let csp = a.header("content-security-policy").unwrap();
    assert!(csp.contains("frame-ancestors 'none'") && csp.contains("default-src 'none'"));
    assert_eq!(a.header("x-frame-options"), Some("DENY"));
    assert_eq!(a.header("cache-control"), Some("no-store"));

    // localhost is this machine too; any other name is a rebinding site.
    let local = page
        .get(
            "/",
            &[
                ("Host", format!("localhost:{}", page.port)),
                ("Cookie", cookie.clone()),
            ],
        )
        .await;
    assert_eq!(local.status, 200);
    for bad in [
        format!("evil.example:{}", page.port),
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{}", page.port + 1),
    ] {
        for path in ["/".to_string(), format!("/?token={}", page.token)] {
            let a = page
                .get(&path, &[("Host", bad.clone()), ("Cookie", cookie.clone())])
                .await;
            assert_eq!(a.status, 403, "{bad} {path}");
            assert!(a.header("set-cookie").is_none());
            assert!(!a.body.contains(&id));
        }
    }
    let no_host = page.get("/", &[("Cookie", cookie)]).await;
    assert!(matches!(no_host.status, 400 | 403), "{}", no_host.status);
    assert!(!no_host.body.contains(&id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decision_needs_this_pages_cookie_origin_and_form() {
    let fake = Fake::start().await;
    let page = Served::start(&fake).await;
    let id = put(
        &fake,
        &page.store,
        Kind::Comment,
        json!({ "key": "WRK-1", "body": "Shipped." }),
    )
    .await;
    let cookie = page.sign_in().await;
    let (_, csrf) = page.index(&cookie).await;
    assert!(!csrf.is_empty());
    let form = format!("csrf={csrf}");

    let refused: Vec<(&str, Headers, String, u16)> = vec![
        (
            "no cookie",
            vec![("Host", page.host()), ("Origin", page.origin())],
            form.clone(),
            401,
        ),
        (
            "no origin",
            vec![("Host", page.host()), ("Cookie", cookie.clone())],
            form.clone(),
            403,
        ),
        (
            "another site",
            vec![
                ("Host", page.host()),
                ("Cookie", cookie.clone()),
                ("Origin", "http://evil.example".into()),
            ],
            form.clone(),
            403,
        ),
        (
            "another local port",
            vec![
                ("Host", page.host()),
                ("Cookie", cookie.clone()),
                ("Origin", format!("http://127.0.0.1:{}", page.port + 1)),
            ],
            form.clone(),
            403,
        ),
        (
            "opaque origin",
            vec![
                ("Host", page.host()),
                ("Cookie", cookie.clone()),
                ("Origin", "null".into()),
            ],
            form.clone(),
            403,
        ),
        (
            "wrong form field",
            vec![
                ("Host", page.host()),
                ("Cookie", cookie.clone()),
                ("Origin", page.origin()),
            ],
            "csrf=guess".into(),
            403,
        ),
        (
            "no form field",
            vec![
                ("Host", page.host()),
                ("Cookie", cookie.clone()),
                ("Origin", page.origin()),
            ],
            String::new(),
            400,
        ),
        (
            "rebound host",
            vec![
                ("Host", format!("evil.example:{}", page.port)),
                ("Cookie", cookie.clone()),
                ("Origin", format!("http://evil.example:{}", page.port)),
            ],
            form.clone(),
            403,
        ),
    ];
    for (name, headers, body, status) in refused {
        for action in ["approve", "reject"] {
            let a = page.decide(action, &id, &headers, &body).await;
            assert_eq!(a.status, status, "{name} {action}: {}", a.body);
        }
    }
    assert!(fake.writes().is_empty());
    assert_eq!(page.status(&id), None, "still pending");
    // A GET decides nothing.
    let get = page
        .get(
            &format!("/approve/{id}"),
            &[("Host", page.host()), ("Cookie", cookie.clone())],
        )
        .await;
    assert_eq!(get.status, 405);

    let ok = [
        ("Host", page.host()),
        ("Cookie", cookie.clone()),
        ("Origin", page.origin()),
    ];
    let a = page.decide("approve", &id, &ok, &form).await;
    assert_eq!(a.status, 303, "{}", a.body);
    assert_eq!(page.status(&id), Some(Status::Approved));
    let writes = fake.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body, json!({ "body": "Shipped." }));
    let (html, _) = page.index(&cookie).await;
    assert!(html.contains("<li class=\"approved\">"), "{html}");
    assert!(html.contains("0 waiting for review"));

    // Approving it again applies nothing and says why.
    let a = page.decide("approve", &id, &ok, &form).await;
    assert_eq!(a.status, 303);
    assert_eq!(fake.writes().len(), 1);
    let (html, _) = page.index(&cookie).await;
    assert!(html.contains("already decided"), "{html}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_rejects_and_refuses_what_moved() {
    let fake = Fake::start().await;
    let page = Served::start(&fake).await;
    let rejected = put(
        &fake,
        &page.store,
        Kind::Update,
        json!({ "key": "WRK-2", "summary": "Tidy up" }),
    )
    .await;
    let moved = put(
        &fake,
        &page.store,
        Kind::Update,
        json!({ "key": "WRK-1", "summary": "Port the feed to S3" }),
    )
    .await;
    let cookie = page.sign_in().await;
    let (html, csrf) = page.index(&cookie).await;
    assert!(html.contains("2 waiting for review"));
    assert!(
        html.contains("<span class=\"del\">- Port the feed</span>"),
        "{html}"
    );
    let ok = [
        ("Host", page.host()),
        ("Cookie", cookie.clone()),
        ("Origin", page.origin()),
    ];

    let a = page
        .decide(
            "reject",
            &rejected,
            &ok,
            &format!("csrf={csrf}&reason=not+now"),
        )
        .await;
    assert_eq!(a.status, 303);
    let (_, p) = page.store.get(&rejected).unwrap().unwrap();
    let o = p.outcome.unwrap();
    assert_eq!(o.status, Status::Rejected);
    assert_eq!(o.reason.as_deref(), Some("not now"));

    // The terminal's stale check: the issue moved, so nothing is sent.
    fake.bump("WRK-1");
    let a = page
        .decide("approve", &moved, &ok, &format!("csrf={csrf}"))
        .await;
    assert_eq!(a.status, 303);
    assert_eq!(page.status(&moved), Some(Status::Stale));
    assert!(fake.writes().is_empty());
    let (html, _) = page.index(&cookie).await;
    assert!(html.contains("<li class=\"refused\">"), "{html}");
    assert!(html.contains("WRK-1 moved"), "{html}");
    assert!(html.contains("not now"));
}
