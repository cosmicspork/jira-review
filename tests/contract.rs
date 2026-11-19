//! The caller-context wire format v1, pinned by `tests/contract_vectors.json`.
//! sql-review copies the file into its own tests; Tracon's plugin host checks
//! its signer against it. Regenerate it (deterministically) with
//! `cargo test --test contract -- --ignored generate_contract_vectors`.

use ed25519_dalek::SigningKey;
use jira_review::contract::{self, CallId, Caller, ContractEnv, NonceCache};
use jira_review::mcp::is_write;
use serde_json::{Value, json};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/contract_vectors.json");
const NOW_MS: i64 = 1_794_900_000_000;
const TOKEN: &str = "jira-review-test-plugin-token";
/// The canonical JSON of a proposal's `versions`, as `tracon/preview` hands
/// it to the node.
const PREVIEW_TOKEN: &str = r#"{"WRK-1":"2026-11-16T09:12:33.000+0000"}"#;

fn seed() -> [u8; 32] {
    std::array::from_fn(|i| i as u8 + 1)
}

fn nonce(i: u32) -> String {
    format!("{:032x}", 0x5eed_0000_u64 + i as u64)
}

fn valid_call() -> Value {
    json!({
        "tool": "update",
        "arguments": { "key": "WRK-1", "summary": "Ship it", "labels": ["a", "b"],
                       "profile": "work" },
    })
}

/// The unsigned context for `call`, before any tampering.
fn context(call: &Value, nonce: &str) -> Value {
    json!({
        "v": 1,
        "plugin": "jira-review",
        "instance": contract::instance_of(TOKEN),
        "tool": call["tool"],
        "arguments_sha256": contract::arguments_sha256(call.get("arguments")),
        "caller": { "kind": "agent", "session": "01a1e94c-d876-7750-b2c0-2aa620c31655",
                    "harness": "claude" },
        "channel": "personal",
        "approval_id": "apv-0001",
        "issued_ms": NOW_MS - 1_000,
        "expires_ms": NOW_MS + 29_000,
        "nonce": nonce,
        "credentials": { "jira-work": { "JIRA_URL": "https://example.atlassian.net",
                                         "JIRA_EMAIL": "me@example.com",
                                         "JIRA_TOKEN": "not-a-real-token" } },
    })
}

fn build() -> Value {
    let key = SigningKey::from_bytes(&seed());
    let call = valid_call();
    let unsigned = context(&call, &nonce(0));
    let signed = contract::sign(&unsigned, &key);
    let mut with_token = context(&call, &nonce(100));
    with_token["preview_token"] = json!(PREVIEW_TOKEN);
    let with_token_signed = contract::sign(&with_token, &key);
    let mut cases = Vec::new();
    let mut case =
        |name: &str, check: u8, call: &Value, edit: &dyn Fn(&mut Value), resign: bool| {
            let mut ctx = context(&valid_call(), &nonce(cases.len() as u32 + 1));
            edit(&mut ctx);
            let ctx = if resign {
                contract::sign(&ctx, &key)
            } else {
                let mut ctx = contract::sign(&ctx, &key);
                // Flip the signature's last bit: well formed, but wrong.
                let sig = ctx["signature"].as_str().unwrap().to_string();
                let mut bytes = hex::decode(&sig).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                ctx["signature"] = json!(hex::encode(bytes));
                ctx
            };
            cases.push(
                json!({ "name": name, "check": check, "call": call, "context": ctx,
                           "expect": "refused" }),
            );
        };
    let other_args = json!({ "tool": "update", "arguments": { "key": "WRK-1",
                             "summary": "Ship something else", "labels": ["a", "b"],
                             "profile": "work" } });
    case("wrong v", 1, &call, &|c| c["v"] = json!(2), true);
    case(
        "wrong plugin",
        1,
        &call,
        &|c| c["plugin"] = json!("sql-review"),
        true,
    );
    case(
        "wrong instance",
        2,
        &call,
        &|c| c["instance"] = json!(contract::instance_of("another-token")),
        true,
    );
    case(
        "wrong tool",
        3,
        &call,
        &|c| c["tool"] = json!("comment"),
        true,
    );
    case("wrong arguments hash", 3, &other_args, &|_| {}, true);
    case("bad signature", 4, &call, &|_| {}, false);
    case(
        "expired",
        5,
        &call,
        &|c| {
            c["issued_ms"] = json!(NOW_MS - 70_000);
            c["expires_ms"] = json!(NOW_MS - 10_000);
        },
        true,
    );
    case(
        "issued too far ahead",
        5,
        &call,
        &|c| {
            c["issued_ms"] = json!(NOW_MS + 10_000);
            c["expires_ms"] = json!(NOW_MS + 40_000);
        },
        true,
    );
    case(
        "window over 60 s",
        5,
        &call,
        &|c| c["expires_ms"] = json!(NOW_MS + 60_000),
        true,
    );
    case(
        "write without approval_id",
        7,
        &call,
        &|c| c["approval_id"] = Value::Null,
        true,
    );
    cases.push(
        json!({ "name": "replayed nonce", "check": 6, "call": call, "context": signed,
                       "expect": "refused", "after": "valid" }),
    );
    json!({
        "note": "Caller-context wire format v1 (provisional; shared by jira-review, sql-review \
                 and Tracon's plugin host). seed is the raw ed25519 secret key, used as \
                 SigningKey::from_bytes(seed): it is NOT a Tracon Identity seed, which derives \
                 its key through HKDF. signing_bytes = ASCII label || u32 big-endian length || \
                 canonical JSON of the context without signature (keys sorted by byte order, \
                 no whitespace, serde_json escaping, integers only). Each refused case is \
                 checked with a fresh nonce cache at now_ms, except the one marked after: \
                 valid, which follows the valid context in the same cache. \
                 valid_preview_token is a valid context for the same call carrying the \
                 optional preview_token, as on a call dispatched from an approved card whose \
                 preview returned a token; check it with a fresh nonce cache.",
        "label": contract::LABEL,
        "seed": hex::encode(seed()),
        "public_key": hex::encode(key.verifying_key().to_bytes()),
        "plugin_token": TOKEN,
        "instance": contract::instance_of(TOKEN),
        "now_ms": NOW_MS,
        "valid": {
            "call": call,
            "arguments_canonical": contract::canonical(&call["arguments"]),
            "context": signed,
            "signing_bytes_hex": hex::encode(contract::signing_bytes(&unsigned)),
            "signature": signed["signature"],
        },
        "valid_preview_token": {
            "call": call,
            "arguments_canonical": contract::canonical(&call["arguments"]),
            "context": with_token_signed,
            "signing_bytes_hex": hex::encode(contract::signing_bytes(&with_token)),
            "signature": with_token_signed["signature"],
        },
        "refused": cases,
    })
}

#[test]
#[ignore = "rewrites tests/contract_vectors.json"]
fn generate_contract_vectors() {
    let mut s = serde_json::to_string_pretty(&build()).unwrap();
    s.push('\n');
    std::fs::write(PATH, s).unwrap();
}

fn vectors() -> Value {
    serde_json::from_str(&std::fs::read_to_string(PATH).unwrap()).unwrap()
}

fn env(v: &Value) -> ContractEnv {
    ContractEnv::new(
        v["plugin_token"].as_str().unwrap(),
        v["public_key"].as_str().unwrap(),
    )
    .unwrap()
}

fn verify(v: &Value, call: &Value, ctx: &Value, nonces: &NonceCache) -> Result<Caller, u8> {
    let tool = call["tool"].as_str().unwrap();
    let id = CallId {
        tool,
        arguments: call.get("arguments"),
        write: is_write(tool),
    };
    contract::verify(ctx, &id, &env(v), v["now_ms"].as_i64().unwrap(), nonces).map_err(|r| r.check)
}

#[test]
fn the_committed_vectors_are_what_this_build_generates() {
    assert_eq!(
        vectors(),
        build(),
        "regenerate with --ignored generate_contract_vectors"
    );
}

#[test]
fn our_signer_reproduces_the_signing_bytes_and_signature() {
    let v = vectors();
    let seed: [u8; 32] = hex::decode(v["seed"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let key = SigningKey::from_bytes(&seed);
    assert_eq!(
        hex::encode(key.verifying_key().to_bytes()),
        v["public_key"].as_str().unwrap()
    );
    assert_eq!(
        contract::instance_of(v["plugin_token"].as_str().unwrap()),
        v["instance"].as_str().unwrap()
    );
    let valid = &v["valid"];
    assert_eq!(
        contract::canonical(&valid["call"]["arguments"]),
        valid["arguments_canonical"].as_str().unwrap()
    );
    assert_eq!(
        hex::encode(contract::signing_bytes(&valid["context"])),
        valid["signing_bytes_hex"].as_str().unwrap()
    );
    let resigned = contract::sign(&valid["context"], &key);
    assert_eq!(resigned["signature"], valid["signature"]);
    assert_eq!(resigned, valid["context"]);

    let with_token = &v["valid_preview_token"];
    assert_eq!(
        contract::canonical(&with_token["call"]["arguments"]),
        with_token["arguments_canonical"].as_str().unwrap()
    );
    assert_eq!(
        hex::encode(contract::signing_bytes(&with_token["context"])),
        with_token["signing_bytes_hex"].as_str().unwrap()
    );
    let resigned = contract::sign(&with_token["context"], &key);
    assert_eq!(resigned["signature"], with_token["signature"]);
}

#[test]
fn the_valid_context_passes_and_each_tampered_one_fails_at_its_check() {
    let v = vectors();
    let nonces = NonceCache::default();
    let caller = verify(&v, &v["valid"]["call"], &v["valid"]["context"], &nonces).unwrap();
    assert_eq!(caller.preview_token, None);
    let with_token = &v["valid_preview_token"];
    let caller = verify(
        &v,
        &with_token["call"],
        &with_token["context"],
        &NonceCache::default(),
    )
    .unwrap();
    assert_eq!(caller.preview_token.as_deref(), Some(PREVIEW_TOKEN));
    let cases = v["refused"].as_array().unwrap();
    assert_eq!(cases.len(), 11);
    for case in cases {
        let fresh = NonceCache::default();
        let cache = if case["after"] == "valid" {
            &nonces
        } else {
            &fresh
        };
        let got = verify(&v, &case["call"], &case["context"], cache).map(drop);
        assert_eq!(
            got,
            Err(case["check"].as_u64().unwrap() as u8),
            "{}",
            case["name"]
        );
    }
}
