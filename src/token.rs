use std::{
    collections::HashMap,
    net::IpAddr,
    path::{
        Path,
        PathBuf,
    },
    sync::Mutex,
    time::Instant,
};

use anyhow::Context;

/// The shortest token `serve` accepts. Wrong tokens are throttled, but only
/// a long token makes guessing hopeless.
pub const MIN_TOKEN_LEN: usize = 32;

/// Wrong tokens one client address may present in a row before its tokens
/// are no longer checked.
const GUESS_BURST: f64 = 20.0;
/// How many of those come back per second.
const GUESS_REFILL_PER_SEC: f64 = 1.0;
/// Addresses tracked at once; beyond that, they share one allowance.
const GUESS_CLIENTS: usize = 10_000;

/// A new random token, as printed by `wordmaker token`.
pub fn generate() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Refuse tokens short enough to be guessed.
pub fn check_strength(token: &str) -> anyhow::Result<()> {
    let len = token.chars().count();
    anyhow::ensure!(
        len >= MIN_TOKEN_LEN,
        "the token has {len} characters but needs at least {MIN_TOKEN_LEN}; generate one with \
         `wordmaker token`"
    );
    Ok(())
}

/// Wrong admin tokens per client address, shared by every place that
/// checks one (`/api`, `/mcp` and the OAuth sign-in), so that guessing is
/// slow however many requests run in parallel. Behind a reverse proxy all
/// clients have the proxy's address and share one allowance.
#[derive(Debug, Default)]
pub struct Guesses {
    clients: Mutex<HashMap<Option<IpAddr>, (f64, Instant)>>,
}

impl Guesses {
    /// Whether a token from `peer` may be checked now.
    pub fn allowed(&self, peer: Option<IpAddr>) -> bool {
        self.allowed_at(peer, Instant::now())
    }

    /// Count a wrong token from `peer`.
    pub fn failed(&self, peer: Option<IpAddr>) {
        self.failed_at(peer, Instant::now());
    }

    fn allowed_at(&self, peer: Option<IpAddr>, now: Instant) -> bool {
        let clients = self.lock();
        let used = |key| {
            clients
                .get(&key)
                .map_or(0.0, |&(failures, since)| decayed(failures, since, now))
        };
        // Room for one more wrong token is needed. The shared allowance
        // counts for everyone once it is in use.
        let room = |key| used(key) + 1.0 <= GUESS_BURST;
        room(client_key(peer)) && room(None)
    }

    fn failed_at(&self, peer: Option<IpAddr>, now: Instant) {
        let mut clients = self.lock();
        let mut key = client_key(peer);
        if clients.len() >= GUESS_CLIENTS && !clients.contains_key(&key) {
            clients.retain(|_, &mut (failures, since)| decayed(failures, since, now) > 0.0);
            if clients.len() >= GUESS_CLIENTS {
                key = None;
            }
        }
        let entry = clients.entry(key).or_insert((0.0, now));
        *entry = (decayed(entry.0, entry.1, now) + 1.0, now);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Option<IpAddr>, (f64, Instant)>> {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn decayed(failures: f64, since: Instant, now: Instant) -> f64 {
    let refilled = now.saturating_duration_since(since).as_secs_f64() * GUESS_REFILL_PER_SEC;
    (failures - refilled).max(0.0)
}

/// IPv6 clients usually have a whole /64 to pick addresses from, so they
/// are told apart by that.
fn client_key(peer: Option<IpAddr>) -> Option<IpAddr> {
    peer.map(|ip| match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let bits = u128::from(v6) & !u128::from(u64::MAX);
            IpAddr::V6(bits.into())
        }
        v4 @ IpAddr::V4(_) => v4,
    })
}

/// The API token, given directly or as a file holding it. A file keeps the
/// token out of the process list and the environment, and is how systemd
/// credentials and Docker secrets hand one over.
#[derive(clap::Args, Debug, Default)]
pub struct TokenArgs {
    /// Bearer token for `/api` and `/mcp`.
    #[arg(
        long,
        env = "WORDMAKER_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<String>,

    /// File holding the bearer token, e.g. `$CREDENTIALS_DIRECTORY/token`
    /// or `/run/secrets/wordmaker_token`. Surrounding whitespace is ignored.
    #[arg(long, env = "WORDMAKER_TOKEN_FILE", value_name = "PATH")]
    token_file: Option<PathBuf>,
}

impl TokenArgs {
    /// The token, if one was given; an empty one counts as none.
    pub fn resolve(&self) -> anyhow::Result<Option<String>> {
        read_secret(self.token.as_deref(), self.token_file.as_deref(), "token")
    }

    /// The token, or an error saying how to give one.
    pub fn require(&self, hint: &str) -> anyhow::Result<String> {
        self.resolve()?.with_context(|| {
            format!("set WORDMAKER_TOKEN or WORDMAKER_TOKEN_FILE (--token, --token-file){hint}")
        })
    }
}

/// A secret given directly or as a file holding it, trimmed; an empty one
/// counts as none. `what` names it in errors.
pub fn read_secret(
    value: Option<&str>,
    file: Option<&Path>,
    what: &str,
) -> anyhow::Result<Option<String>> {
    let secret = match (value, file) {
        (Some(value), _) => value.trim().to_owned(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the {what} file {}", path.display()))?
            .trim()
            .to_owned(),
        (None, None) => return Ok(None),
    };
    Ok(Some(secret).filter(|secret| !secret.is_empty()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn generated_tokens_are_strong_enough() {
        check_strength(&generate()).unwrap();
        check_strength(&"x".repeat(MIN_TOKEN_LEN)).unwrap();
        let err = check_strength("hunter2").unwrap_err().to_string();
        assert!(err.contains("at least 32"), "{err}");
    }

    #[test]
    fn wrong_tokens_are_throttled_per_address() {
        let guesses = Guesses::default();
        let start = Instant::now();
        let one: Option<IpAddr> = Some("203.0.113.1".parse().unwrap());
        let other: Option<IpAddr> = Some("203.0.113.2".parse().unwrap());
        for _ in 0..20 {
            assert!(guesses.allowed_at(one, start));
            guesses.failed_at(one, start);
        }
        assert!(!guesses.allowed_at(one, start));
        assert!(guesses.allowed_at(other, start));
        // The allowance comes back over time.
        let later = start + Duration::from_millis(10);
        assert!(!guesses.allowed_at(one, later));
        let later = start + Duration::from_secs(1);
        assert!(guesses.allowed_at(one, later));
        guesses.failed_at(one, later);
        assert!(!guesses.allowed_at(one, later));

        // Addresses in one IPv6 /64 count together.
        let a = Some("2001:db8:1:2::1".parse().unwrap());
        let b = Some("2001:db8:1:2:ffff::9".parse().unwrap());
        for _ in 0..20 {
            guesses.failed_at(a, start);
        }
        assert!(!guesses.allowed_at(b, start));
        assert!(guesses.allowed_at(Some("2001:db8:1:3::1".parse().unwrap()), start));
    }

    #[test]
    fn reads_and_trims_token_files() {
        let path =
            std::env::temp_dir().join(format!("wordmaker-token-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&path, "  secret\n").unwrap();
        let args = TokenArgs {
            token: None,
            token_file: Some(path.clone()),
        };
        assert_eq!(args.require("").unwrap(), "secret");

        std::fs::write(&path, "\n").unwrap();
        assert!(args.require("").is_err(), "an empty file is no token");

        std::fs::remove_file(&path).unwrap();
        let err = args.require("").unwrap_err().to_string();
        assert!(err.contains("cannot read the token file"), "{err}");

        let none = TokenArgs::default();
        assert!(none.resolve().unwrap().is_none());
    }
}
