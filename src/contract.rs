//! The caller context Tracon signs onto every plugin call, and its check.
//!
//! Wire format v1, provisional, shared with sql-review and Tracon's plugin
//! host: `params._meta.tracon` on each `tools/call` is an object holding
//! `v`, `plugin`, `instance`, `tool`, `arguments_sha256`, `caller`,
//! `channel`, `approval_id`, `issued_ms`, `expires_ms`, `nonce`,
//! `credentials` and `signature`. The signature is ed25519 by the node key
//! over the ASCII label `tracon/v1/plugin-call` followed by a big-endian u32
//! length and the canonical JSON of the object without `signature`.
//! `tests/contract_vectors.json` pins every byte of it.

use std::collections::HashMap;
use std::sync::Mutex;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// What the signature is over, ahead of the context.
pub const LABEL: &str = "tracon/v1/plugin-call";
/// The manifest name a context must be addressed to.
pub const PLUGIN: &str = "jira-review";
/// Clock skew allowed either side of a context's window.
pub const SKEW_MS: i64 = 5_000;
/// The longest window a context may claim.
pub const MAX_WINDOW_MS: i64 = 60_000;

/// JSON with object keys sorted by byte order and no whitespace; strings
/// and keys escaped as serde_json escapes them. Ported from Tracon's
/// `proto::admin::write_canonical`, so both sides write the same bytes.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn has_float(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_f64(),
        Value::Array(a) => a.iter().any(has_float),
        Value::Object(o) => o.values().any(has_float),
        _ => false,
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// What a context carries as `instance`: the hash of the plugin token, so
/// the token itself never travels.
pub fn instance_of(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// The hash a context carries of the call's arguments; missing arguments
/// count as `{}`.
pub fn arguments_sha256(arguments: Option<&Value>) -> String {
    let empty = Value::Object(Map::new());
    sha256_hex(canonical(arguments.unwrap_or(&empty)).as_bytes())
}

/// The bytes a context is signed over: the label, then the length-prefixed
/// canonical JSON of the context with any `signature` removed.
pub fn signing_bytes(context: &Value) -> Vec<u8> {
    let mut unsigned = context.clone();
    if let Some(o) = unsigned.as_object_mut() {
        o.remove("signature");
    }
    let json = canonical(&unsigned);
    let mut out = LABEL.as_bytes().to_vec();
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json.as_bytes());
    out
}

/// `context` signed by `key`, as the node signs it. For tests, and for the
/// host's own tests to check against.
pub fn sign(context: &Value, key: &SigningKey) -> Value {
    let signature = key.sign(&signing_bytes(context));
    let mut signed = context.clone();
    signed["signature"] = Value::String(hex::encode(signature.to_bytes()));
    signed
}

/// What `serve` was started with.
#[derive(Clone)]
pub struct ContractEnv {
    /// `hex(sha256(TRACON_PLUGIN_TOKEN))`.
    pub instance: String,
    pub node_key: VerifyingKey,
}

impl ContractEnv {
    pub fn new(plugin_token: &str, node_key_hex: &str) -> Result<Self, String> {
        if plugin_token.is_empty() {
            return Err("TRACON_PLUGIN_TOKEN is empty".into());
        }
        let bytes: [u8; 32] = hex::decode(node_key_hex.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or("TRACON_NODE_KEY is not 32 bytes of hex")?;
        let node_key = VerifyingKey::from_bytes(&bytes)
            .map_err(|e| format!("TRACON_NODE_KEY is not an ed25519 key: {e}"))?;
        Ok(Self {
            instance: instance_of(plugin_token),
            node_key,
        })
    }
}

/// The call a context must be about.
pub struct CallId<'a> {
    pub tool: &'a str,
    pub arguments: Option<&'a Value>,
    /// The tool writes, so it runs only with an approval.
    pub write: bool,
}

/// Nonces seen and the `expires_ms` of the context each came in, pruned once
/// a context that old could no longer pass the clock check.
#[derive(Default)]
pub struct NonceCache(Mutex<HashMap<String, i64>>);

impl NonceCache {
    /// Record `nonce`; false when it was already there.
    fn fresh(&self, nonce: &str, expires_ms: i64, now_ms: i64) -> bool {
        let mut seen = self.0.lock().unwrap();
        seen.retain(|_, expires| *expires + SKEW_MS >= now_ms);
        if seen.contains_key(nonce) {
            return false;
        }
        seen.insert(nonce.to_string(), expires_ms);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerKind {
    Operator,
    Agent,
    /// Reserved for plugin-to-plugin calls; accepted, not yet sent.
    Plugin,
}

/// Who a verified call is from, and what it was lent.
#[derive(Clone)]
pub struct Caller {
    pub kind: CallerKind,
    pub session: Option<String>,
    pub harness: Option<String>,
    pub channel: String,
    pub approval_id: Option<String>,
    /// Credential name (`jira`, `jira-<profile>`) to its env-style fields.
    pub credentials: HashMap<String, HashMap<String, String>>,
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut lent: Vec<&String> = self.credentials.keys().collect();
        lent.sort();
        f.debug_struct("Caller")
            .field("kind", &self.kind)
            .field("session", &self.session)
            .field("harness", &self.harness)
            .field("channel", &self.channel)
            .field("approval_id", &self.approval_id)
            .field("credentials", &lent)
            .finish()
    }
}

/// Why a call was refused, and the check (1–7) that refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub check: u8,
    pub reason: String,
}

fn refuse<T>(check: u8, reason: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal {
        check,
        reason: reason.into(),
    })
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Check a call's context, in the order the contract fixes:
///
/// 1. `v` is 1 and `plugin` is this plugin (and the context is well formed)
/// 2. `instance` is the hash of this process's plugin token
/// 3. `tool` and `arguments_sha256` match the call being served
/// 4. the signature verifies under the node key
/// 5. now is within the window, give or take [`SKEW_MS`], and the window is
///    at most [`MAX_WINDOW_MS`]
/// 6. the nonce has not been seen
/// 7. a write carries an `approval_id`
///
/// Nothing is read or sent for a refused call.
pub fn verify(
    meta: &Value,
    call: &CallId,
    env: &ContractEnv,
    now_ms: i64,
    nonces: &NonceCache,
) -> Result<Caller, Refusal> {
    // 1
    let Some(ctx) = meta.as_object() else {
        return refuse(1, "the call carries no caller context");
    };
    if ctx.get("v").and_then(Value::as_i64) != Some(1) {
        return refuse(1, "caller context version is not 1");
    }
    if ctx.get("plugin").and_then(Value::as_str) != Some(PLUGIN) {
        return refuse(1, format!("caller context is not for {PLUGIN}"));
    }
    if has_float(meta) {
        return refuse(1, "caller context holds a non-integer number");
    }
    let str_of = |k: &str| ctx.get(k).and_then(Value::as_str);
    let opt_str = |v: Option<&Value>, k: &str| -> Result<Option<String>, Refusal> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => refuse(1, format!("caller context {k} is not a string or null")),
        }
    };
    let (Some(instance), Some(tool), Some(args_hash), Some(channel), Some(nonce), Some(sig)) = (
        str_of("instance"),
        str_of("tool"),
        str_of("arguments_sha256"),
        str_of("channel"),
        str_of("nonce"),
        str_of("signature"),
    ) else {
        return refuse(1, "caller context is missing a field");
    };
    let (Some(issued_ms), Some(expires_ms)) = (
        ctx.get("issued_ms").and_then(Value::as_i64),
        ctx.get("expires_ms").and_then(Value::as_i64),
    ) else {
        return refuse(1, "caller context has no integer issued_ms and expires_ms");
    };
    let approval_id = opt_str(ctx.get("approval_id"), "approval_id")?;
    let caller = ctx.get("caller").and_then(Value::as_object);
    let Some(caller) = caller else {
        return refuse(1, "caller context has no caller");
    };
    let kind = match caller.get("kind").and_then(Value::as_str) {
        Some("operator") => CallerKind::Operator,
        Some("agent") => CallerKind::Agent,
        Some("plugin") => CallerKind::Plugin,
        _ => return refuse(1, "caller kind is not operator, agent or plugin"),
    };
    let session = opt_str(caller.get("session"), "caller.session")?;
    let harness = opt_str(caller.get("harness"), "caller.harness")?;
    let mut credentials = HashMap::new();
    match ctx.get("credentials") {
        None | Some(Value::Null) => {}
        Some(Value::Object(lent)) => {
            for (name, fields) in lent {
                let Some(fields) = fields.as_object() else {
                    return refuse(1, format!("credential {name} is not an object"));
                };
                let mut out = HashMap::new();
                for (k, v) in fields {
                    let Some(v) = v.as_str() else {
                        return refuse(1, format!("credential {name} field {k} is not a string"));
                    };
                    out.insert(k.clone(), v.to_string());
                }
                credentials.insert(name.clone(), out);
            }
        }
        Some(_) => return refuse(1, "caller context credentials is not an object"),
    }

    // 2
    if instance != env.instance {
        return refuse(2, "caller context is for another instance of this plugin");
    }

    // 3
    if tool != call.tool {
        return refuse(
            3,
            format!("caller context is for tool {tool}, not {}", call.tool),
        );
    }
    if args_hash != arguments_sha256(call.arguments) {
        return refuse(3, "caller context is for other arguments");
    }

    // 4
    let signature = hex::decode(sig)
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
        .map(|b| Signature::from_bytes(&b));
    let Some(signature) = signature else {
        return refuse(4, "caller context signature is not 64 bytes of hex");
    };
    if env
        .node_key
        .verify(&signing_bytes(meta), &signature)
        .is_err()
    {
        return refuse(
            4,
            "caller context signature does not verify under the node key",
        );
    }

    // 5
    if expires_ms < issued_ms || expires_ms - issued_ms > MAX_WINDOW_MS {
        return refuse(5, "caller context window is longer than 60 s");
    }
    if now_ms < issued_ms - SKEW_MS {
        return refuse(5, "caller context is issued in the future");
    }
    if now_ms > expires_ms + SKEW_MS {
        return refuse(5, "caller context has expired");
    }

    // 6
    if !is_hex(nonce, 32) {
        return refuse(6, "caller context nonce is not 32 lowercase hex characters");
    }
    if !nonces.fresh(nonce, expires_ms, now_ms) {
        return refuse(6, "caller context nonce was already used");
    }

    // 7
    if call.write && approval_id.is_none() {
        return refuse(
            7,
            format!("{} writes, and this call carries no approval", call.tool),
        );
    }

    Ok(Caller {
        kind,
        session,
        harness,
        channel: channel.to_string(),
        approval_id,
        credentials,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn env() -> ContractEnv {
        ContractEnv::new("token", &hex::encode(key().verifying_key().to_bytes())).unwrap()
    }

    fn context(tool: &str, args: &Value, now: i64, nonce: &str, approval: Option<&str>) -> Value {
        sign(
            &json!({
                "v": 1, "plugin": PLUGIN, "instance": instance_of("token"), "tool": tool,
                "arguments_sha256": arguments_sha256(Some(args)),
                "caller": { "kind": "agent", "session": "s", "harness": null },
                "channel": "personal", "approval_id": approval,
                "issued_ms": now, "expires_ms": now + 30_000, "nonce": nonce,
                "credentials": { "jira": { "JIRA_URL": "https://j", "JIRA_PAT": "p" } },
            }),
            &key(),
        )
    }

    const NONCE: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        assert_eq!(
            canonical(&json!({ "b": [1, { "d": null, "c": "x\"y" }], "a": true })),
            r#"{"a":true,"b":[1,{"c":"x\"y","d":null}]}"#
        );
        assert_eq!(arguments_sha256(None), arguments_sha256(Some(&json!({}))));
    }

    #[test]
    fn a_good_context_gives_the_caller_and_its_credentials() {
        let args = json!({ "key": "WRK-1" });
        let ctx = context("issue", &args, 1_000_000, NONCE, None);
        let call = CallId {
            tool: "issue",
            arguments: Some(&args),
            write: false,
        };
        let caller = verify(&ctx, &call, &env(), 1_000_000, &NonceCache::default()).unwrap();
        assert_eq!(caller.kind, CallerKind::Agent);
        assert_eq!(caller.session.as_deref(), Some("s"));
        assert_eq!(caller.harness, None);
        assert_eq!(caller.credentials["jira"]["JIRA_PAT"], "p");
        assert!(!format!("{caller:?}").contains("JIRA_PAT"));
    }

    #[test]
    fn a_bad_signature_an_expired_context_and_a_replay_are_refused() {
        let args = json!({});
        let call = CallId {
            tool: "search",
            arguments: Some(&args),
            write: false,
        };
        let nonces = NonceCache::default();
        let mut ctx = context("search", &args, 1_000_000, NONCE, None);
        ctx["channel"] = json!("other");
        assert_eq!(
            verify(&ctx, &call, &env(), 1_000_000, &nonces)
                .unwrap_err()
                .check,
            4
        );
        let ctx = context("search", &args, 1_000_000, NONCE, None);
        assert_eq!(
            verify(&ctx, &call, &env(), 1_036_000, &nonces)
                .unwrap_err()
                .check,
            5
        );
        verify(&ctx, &call, &env(), 1_000_000, &nonces).unwrap();
        assert_eq!(
            verify(&ctx, &call, &env(), 1_000_001, &nonces)
                .unwrap_err()
                .check,
            6
        );
        assert_eq!(
            verify(&Value::Null, &call, &env(), 0, &nonces)
                .unwrap_err()
                .check,
            1
        );
    }

    #[test]
    fn a_write_without_an_approval_is_refused_last() {
        let args = json!({ "key": "WRK-1", "body": "x" });
        let call = CallId {
            tool: "comment",
            arguments: Some(&args),
            write: true,
        };
        let ctx = context("comment", &args, 5, NONCE, None);
        assert_eq!(
            verify(&ctx, &call, &env(), 5, &NonceCache::default())
                .unwrap_err()
                .check,
            7
        );
        let other = "ffeeddccbbaa99887766554433221100";
        let ctx = context("comment", &args, 5, other, Some("ap-1"));
        let caller = verify(&ctx, &call, &env(), 5, &NonceCache::default()).unwrap();
        assert_eq!(caller.approval_id.as_deref(), Some("ap-1"));
    }

    #[test]
    fn a_missing_or_malformed_node_key_is_refused_at_start() {
        assert!(ContractEnv::new("t", "").is_err());
        assert!(ContractEnv::new("t", "zz").is_err());
        assert!(ContractEnv::new("", &hex::encode([1u8; 32])).is_err());
    }

    #[test]
    fn nonces_are_forgotten_once_their_context_could_not_pass() {
        let nonces = NonceCache::default();
        assert!(nonces.fresh("a", 100, 0));
        assert!(!nonces.fresh("a", 100, 0));
        assert!(nonces.fresh("b", 10_000, 5_101));
        assert!(
            nonces.fresh("a", 10_000, 5_101),
            "a's context expired at 100"
        );
    }
}
