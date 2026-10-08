use std::path::{
    Path,
    PathBuf,
};

use anyhow::Context;

/// The API token, given directly or as a file holding it. A file keeps the
/// token out of the process list and the environment, and is how systemd
/// credentials and Docker secrets hand one over.
#[derive(clap::Args, Debug, Default)]
pub struct TokenArgs {
    /// Bearer token of the `WordMaker` server's API.
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
        let token = match (&self.token, &self.token_file) {
            (Some(token), _) => token.trim().to_owned(),
            (None, Some(path)) => read_secret_file(path, "token")?,
            (None, None) => return Ok(None),
        };
        Ok(Some(token).filter(|token| !token.is_empty()))
    }

    /// The token, or an error saying how to give one.
    pub fn require(&self, hint: &str) -> anyhow::Result<String> {
        self.resolve()?.with_context(|| {
            format!("set WORDMAKER_TOKEN or WORDMAKER_TOKEN_FILE (--token, --token-file){hint}")
        })
    }
}

/// A secret kept in a file, without the whitespace around it; `what` names
/// it in the error.
pub fn read_secret_file(path: &Path, what: &str) -> anyhow::Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the {what} file {}", path.display()))?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_trims_token_files() {
        let path =
            std::env::temp_dir().join(format!("wordmaker-admin-token-{}", std::process::id()));
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
