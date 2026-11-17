//! Who jira-review is to Jira: a site and either an email with an API token
//! (Jira Cloud, HTTP Basic) or a personal access token (Jira Data Center,
//! Bearer). The same env-style fields serve every source: the process
//! environment, a `.env` file, the OS keyring, and the credential Tracon
//! lends a plugin call.

use std::collections::HashMap;
use std::path::Path;

use base64::Engine;

/// The keyring service every profile is stored under; the profile is the
/// keyring user.
pub const KEYRING_SERVICE: &str = "jira-review";

/// The profile used when none is named.
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// Jira Cloud: account email and API token.
    Basic { email: String, token: String },
    /// Jira Data Center: personal access token.
    Bearer { pat: String },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// `JIRA_URL` without a trailing slash.
    pub url: String,
    pub auth: Auth,
}

// Never print a token, even by accident in a debug log.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self.auth {
            Auth::Basic { .. } => "basic",
            Auth::Bearer { .. } => "bearer",
        };
        write!(f, "Credential({} via {scheme})", self.url)
    }
}

impl Credential {
    /// A credential from env-style fields. `source` names where they came
    /// from, for the error.
    pub fn from_fields(get: impl Fn(&str) -> Option<String>, source: &str) -> Result<Self, String> {
        let get = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let url = get("JIRA_URL")
            .ok_or_else(|| format!("{source} has no JIRA_URL"))?
            .trim_end_matches('/')
            .to_string();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(format!(
                "{source} has a JIRA_URL that is not http(s): {url}"
            ));
        }
        let auth = match (get("JIRA_EMAIL"), get("JIRA_TOKEN"), get("JIRA_PAT")) {
            (Some(email), Some(token), None) => Auth::Basic { email, token },
            (None, None, Some(pat)) => Auth::Bearer { pat },
            (Some(_), Some(_), Some(_)) => {
                return Err(format!(
                    "{source} has both JIRA_EMAIL with JIRA_TOKEN (Jira Cloud) and JIRA_PAT \
                     (Data Center); keep one"
                ));
            }
            _ => {
                return Err(format!(
                    "{source} needs JIRA_EMAIL and JIRA_TOKEN (Jira Cloud) or JIRA_PAT \
                     (Jira Data Center)"
                ));
            }
        };
        Ok(Self { url, auth })
    }

    pub fn from_map(fields: &HashMap<String, String>, source: &str) -> Result<Self, String> {
        Self::from_fields(|k| fields.get(k).cloned(), source)
    }

    /// The `Authorization` header every request carries.
    pub fn header(&self) -> String {
        match &self.auth {
            Auth::Basic { email, token } => {
                let raw = format!("{email}:{token}");
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(raw)
                )
            }
            Auth::Bearer { pat } => format!("Bearer {pat}"),
        }
    }

    /// The same fields as an env-style block, as the keyring holds them.
    pub fn to_block(&self) -> String {
        let mut block = format!("JIRA_URL={}\n", self.url);
        match &self.auth {
            Auth::Basic { email, token } => {
                block.push_str(&format!("JIRA_EMAIL={email}\nJIRA_TOKEN={token}\n"));
            }
            Auth::Bearer { pat } => block.push_str(&format!("JIRA_PAT={pat}\n")),
        }
        block
    }
}

/// `KEY=value` lines, as a keyring secret holds them. Blank lines and `#`
/// comments are skipped; a value may be quoted.
pub fn parse_block(block: &str) -> HashMap<String, String> {
    block
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let k = k.trim().trim_start_matches("export ").trim();
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// The standalone credential: the first source that names `JIRA_URL` wins,
/// whole, so fields are never mixed between sources. In order: the process
/// environment, the `.env` file, then the keyring entry for `profile`.
pub fn resolve(env_file: Option<&Path>, profile: &str) -> Result<Credential, String> {
    resolve_from(
        |k| std::env::var(k).ok(),
        env_file,
        Path::new(".env"),
        || keyring_read(profile),
        profile,
    )
}

/// [`resolve`] with its sources passed in, so each can be tested alone.
pub fn resolve_from(
    env: impl Fn(&str) -> Option<String>,
    env_file: Option<&Path>,
    default_env_file: &Path,
    keyring: impl FnOnce() -> Result<Option<String>, String>,
    profile: &str,
) -> Result<Credential, String> {
    if env("JIRA_URL").is_some_and(|u| !u.trim().is_empty()) {
        return Credential::from_fields(env, "the environment");
    }
    let file = env_file.unwrap_or(default_env_file);
    match std::fs::read_to_string(file) {
        Ok(_) => {
            let fields: HashMap<String, String> = dotenvy::from_path_iter(file)
                .map_err(|e| format!("{}: {e}", file.display()))?
                .collect::<Result<_, _>>()
                .map_err(|e| format!("{}: {e}", file.display()))?;
            if fields.contains_key("JIRA_URL") {
                return Credential::from_map(&fields, &file.display().to_string());
            }
        }
        Err(e) if env_file.is_some() => return Err(format!("{}: {e}", file.display())),
        Err(_) => {}
    }
    match keyring() {
        Ok(Some(block)) => Credential::from_map(
            &parse_block(&block),
            &format!("keyring entry {KEYRING_SERVICE}/{profile}"),
        ),
        Ok(None) => Err(format!(
            "no Jira credential: set JIRA_URL with JIRA_EMAIL and JIRA_TOKEN (Cloud) or \
             JIRA_PAT (Data Center) in the environment or a .env file, or run \
             `jira-review login --profile {profile}`"
        )),
        Err(e) => Err(format!(
            "no Jira credential in the environment or .env, and the keyring could not be \
             read: {e}"
        )),
    }
}

/// The keyring entry for `profile`, or `None` when there is none.
pub fn keyring_read(profile: &str) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(block) => Ok(Some(block)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn keyring_store(profile: &str, credential: &Credential) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, profile)
        .and_then(|e| e.set_password(&credential.to_block()))
        .map_err(|e| e.to_string())
}

pub fn keyring_delete(profile: &str) -> Result<bool, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// The name a plugin call's credential is lent under: `jira` for no
/// profile, `jira-<profile>` otherwise.
pub fn lent_name(profile: Option<&str>) -> String {
    match profile {
        None => "jira".into(),
        Some(p) => format!("jira-{p}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyring_block_parses_like_an_env_file() {
        let fields = parse_block(
            "# work\nJIRA_URL=https://x.atlassian.net/\n\nJIRA_EMAIL = me@x\nJIRA_TOKEN=\"t=1\"\n",
        );
        assert_eq!(fields["JIRA_TOKEN"], "t=1");
        let c = Credential::from_map(&fields, "block").unwrap();
        assert_eq!(c.url, "https://x.atlassian.net");
        assert_eq!(parse_block(&c.to_block()), {
            let mut f = fields.clone();
            f.insert("JIRA_URL".into(), "https://x.atlassian.net".into());
            f
        });
    }

    #[test]
    fn a_credential_never_prints_its_token() {
        let c = Credential {
            url: "https://x".into(),
            auth: Auth::Bearer {
                pat: "secret".into(),
            },
        };
        assert!(!format!("{c:?}").contains("secret"));
    }
}
