//! The controller's Codex login, lent to the target for the length of one
//! bootstrap and then taken back.
//!
//! Bootstrap's setup agent is Codex, and the target's codex-lb has no
//! accounts yet, so the agent needs a ChatGPT login of its own. The only one
//! available is the controller's `~/.codex/auth.json`. Copying it is
//! dangerous in a specific way: the file carries a single-use refresh token
//! that rotates on every refresh, so two copies share one token lineage and
//! the copy that refreshes second fails with `refresh_token_reused`. That is
//! how a controller's Codex login broke on 2026-09-28, after its file had
//! been copied elsewhere (which copy spent the token was never established),
//! and a copy bootstrap left on every box was the same hazard. So the copy
//! lives only as long as bootstrap needs it, and three rules keep the
//! lineage intact meanwhile:
//!
//! - Preflight refuses a login whose access token expires within
//!   [`MIN_REMAINING`]. Codex refreshes proactively only within five minutes
//!   of the access token's expiry (`should_refresh_proactively` in
//!   codex-rs/login/src/auth/manager.rs, checked 2026-09-28), so with that
//!   margin neither the target nor the controller refreshes during a run.
//! - A refresh can still happen after a 401. Whichever copy refreshed last
//!   holds the only live token, so before the target's copy is overwritten
//!   or deleted, the tokens of a newer copy of the same account are written
//!   back to the controller ([`ControllerLogin::reclaim`]).
//! - The target's copy is deleted when bootstrap ends, on failure too when
//!   the target is still reachable. Plain `codex` on the target uses codex-lb
//!   with `requires_openai_auth = false` and needs no local login. That flag
//!   alone is not enough: Codex still loads an `auth.json` that exists and
//!   keeps trying to refresh it, so the file must be absent.
//!
//! Nothing here logs or prints a token. Contents only move between the
//! controller file, memory and [`Transport::push_secret`].

use super::transport::Transport;
use anyhow::{Context, bail};
use base64::Engine;
use std::path::{Path, PathBuf};
use tracing::info;

/// Home-relative location of Codex's file-backed login, on both machines.
const AUTH_FILE: &str = ".codex/auth.json";

/// Remaining access-token lifetime preflight insists on. Bootstrap takes
/// hours; a day leaves room for a slow run plus the reruns that fix it,
/// while a freshly refreshed token (about ten days) passes easily.
const MIN_REMAINING: i64 = 24 * 60 * 60;

/// Markers [`READ_TARGET_LOGIN`] prints around the file. The transport runs
/// `bash -lc`, and a login profile may print to stdout before the script
/// runs or after it (an `EXIT` trap). Bracketing the file on both sides, as
/// base64 so it is one token with no newlines or non-UTF-8 bytes, keeps any
/// of that out of a credential kd might write back.
const BEGIN: &str = "KD-CODEX-AUTH-BEGIN ";
const END: &str = " KD-CODEX-AUTH-END";

/// Print the target's login as base64 between [`BEGIN`] and [`END`], or exit
/// 4 when there is none. A symlink (exit 3) is refused: kd did not put it
/// there and must not follow it into a file something else manages. The
/// encoded file stays in the shell (`printf` is a builtin), out of argv.
/// `tr` rather than GNU `base64 -w0`, so the script test also runs on a
/// macOS controller.
const READ_TARGET_LOGIN: &str = r#"f="$HOME/.codex/auth.json"
if [ -L "$f" ]; then exit 3; elif [ -e "$f" ]; then
  b=$(base64 < "$f" | tr -d '\n') || exit 1
  printf 'KD-CODEX-AUTH-BEGIN %s KD-CODEX-AUTH-END\n' "$b"
else exit 4; fi"#;

/// Delete the target's login. Plain `rm -f` is fine here: this runs over
/// the transport, not through the Codex agent whose command policy rejects
/// it.
const REMOVE_TARGET_LOGIN: &str = r#"rm -f -- "$HOME/.codex/auth.json""#;

/// The controller's login file, checked by preflight. Holds only the path:
/// the file is read again whenever it is used, because [`Self::reclaim`] may
/// have replaced it in between.
pub struct ControllerLogin {
    path: PathBuf,
}

/// The parts of an `auth.json` kd reasons about, plus the file itself.
/// Codex's own schema (`AuthDotJson` in codex-rs/login) has more; kd needs
/// only which account it is, how fresh its access token is, and the three
/// tokens a write-back carries over.
#[derive(Debug)]
struct Login {
    /// The file's bytes as read, which is what gets pushed and compared.
    bytes: Vec<u8>,
    /// The same file parsed, which is what a write-back edits.
    json: serde_json::Value,
    /// `tokens.account_id`: the ChatGPT account. Two files with the same id
    /// are copies of one login lineage, or at least of one account.
    account_id: String,
    /// `exp` of `tokens.access_token`, in Unix seconds. A later expiry means
    /// a later refresh, since every access token gets the same lifetime.
    expires_at: i64,
}

/// What [`ControllerLogin::reclaim`] does with a login found on the target.
#[derive(Debug, PartialEq)]
enum Reclaim {
    /// Write the target's copy back to the controller: it is the same
    /// account and was refreshed after the controller's copy, so it holds
    /// the live refresh token and the controller's is spent.
    WriteBack,
    /// Leave the controller alone; the target's copy is the same age or
    /// older, or belongs to another account.
    Keep,
}

impl ControllerLogin {
    /// Controller preflight: the file exists, is a ChatGPT login, and its
    /// access token outlives a bootstrap. Fails with the fix, before any
    /// connection, like the other credential sources.
    pub fn preflight(home: &Path) -> anyhow::Result<Self> {
        let login = Self {
            path: home.join(AUTH_FILE),
        };
        login.read_fresh()?;
        Ok(login)
    }

    /// Put the controller's login on the target for the agent phases. Any
    /// copy already there is reclaimed first: a failed earlier run, or a
    /// box bootstrapped before logins stopped being left behind, may hold
    /// the newest token of this account.
    pub fn place(&self, t: &Transport) -> anyhow::Result<()> {
        self.reclaim(t)?;
        let login = self.read_fresh()?;
        info!("lending the controller's Codex login to {}", t.destination);
        t.push_secret(&login.bytes, AUTH_FILE)
    }

    /// Take the login back: reclaim it if the target refreshed it, then
    /// delete it from the target. If reclaiming fails, the target's copy is
    /// left in place, because it may be the only live token of the account.
    pub fn retire(&self, t: &Transport) -> anyhow::Result<()> {
        self.reclaim(t)?;
        t.run(REMOVE_TARGET_LOGIN)
            .context("removing ~/.codex/auth.json from the target failed")
    }

    /// Write the target's login back to the controller when it is newer.
    ///
    /// A target file kd cannot read as a ChatGPT login is an error, not
    /// something to skip: both callers overwrite or delete the file right
    /// after, and kd cannot rule out that it holds the only live token. The
    /// error leaves the file in place for a person to look at.
    ///
    /// This assumes no Codex process on the target writes the file between
    /// this read and the caller's overwrite or delete (one SSH round trip).
    /// Bootstrap already requires interactive sessions on the target to be
    /// closed; a Codex session left running is the one way to lose a token
    /// here.
    fn reclaim(&self, t: &Transport) -> anyhow::Result<()> {
        let captured = t.capture(READ_TARGET_LOGIN)?;
        let remote = match captured.status {
            0 => unframe(&captured.stdout)
                .context("reading the target's ~/.codex/auth.json returned no framed file")?,
            3 => bail!("refusing to follow a symlinked ~/.codex/auth.json on the target"),
            4 => return Ok(()),
            status => bail!(
                "reading the target's ~/.codex/auth.json exited with {status}: {}",
                captured.stderr.trim()
            ),
        };
        let remote = Login::parse(remote).context(
            "the target's ~/.codex/auth.json is not a ChatGPT login kd can reason about; \
             inspect it on the target, delete it by hand if it holds nothing you need, and rerun",
        )?;
        let local = self.read()?;
        match decide(&local, &remote) {
            Reclaim::Keep => Ok(()),
            Reclaim::WriteBack => {
                self.write_back(&local, &remote)?;
                info!(
                    "the target refreshed the Codex login; wrote the new token back to {}",
                    self.path.display()
                );
                Ok(())
            }
        }
    }

    /// Replace the controller file, in one rename, with the controller's own
    /// JSON carrying the target's tokens (see [`merge_tokens`]). Refuses when
    /// the file changed since `local` was read moments earlier (a Codex
    /// process on the controller writing it at that instant): whatever wrote
    /// it knows something kd does not. A `codex login` earlier in the run
    /// never gets here, because its fresher token makes [`decide`] keep it.
    /// The compare and the rename are not one atomic step; a Codex write
    /// landing in between would be lost. That window is microseconds, and
    /// the expiry gate keeps the controller's Codex from refreshing during a
    /// run, so kd does not lock against it.
    fn write_back(&self, local: &Login, remote: &Login) -> anyhow::Result<()> {
        if self.path.is_symlink() {
            bail!(
                "{} is a symlink; not replacing it with the target's newer login",
                self.path.display()
            );
        }
        let dir = self.path.parent().context("auth.json has no parent")?;
        // NamedTempFile creates the file 0600 in the same directory, so the
        // rename is atomic and the token is never world-readable.
        let mut staged = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("cannot stage a file in {}", dir.display()))?;
        std::io::Write::write_all(&mut staged, &merge_tokens(local, remote)?)?;
        if std::fs::read(&self.path).ok().as_deref() != Some(local.bytes.as_slice()) {
            bail!(
                "{} changed during bootstrap; leaving it alone",
                self.path.display()
            );
        }
        staged
            .persist(&self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))?;
        Ok(())
    }

    fn read(&self) -> anyhow::Result<Login> {
        let bytes = std::fs::read(&self.path).map_err(|_| {
            anyhow::anyhow!(
                "codex: ~/{AUTH_FILE} is missing (set cli_auth_credentials_store = \"file\" in ~/.codex/config.toml and run `codex login`)"
            )
        })?;
        Login::parse(bytes).with_context(|| format!("codex: ~/{AUTH_FILE}"))
    }

    /// [`Self::read`] plus the expiry gate from the module docs.
    fn read_fresh(&self) -> anyhow::Result<Login> {
        let login = self.read()?;
        check_fresh(&login, jiff::Timestamp::now().as_second())?;
        Ok(login)
    }
}

impl Login {
    /// Parse the fields kd needs. A file without ChatGPT tokens (an API-key
    /// login, for instance) is rejected: kd cannot reason about its
    /// lifetime, and bootstrap's agent runs on a ChatGPT login.
    fn parse(bytes: Vec<u8>) -> anyhow::Result<Self> {
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).context("auth.json is not JSON")?;
        let tokens = json
            .get("tokens")
            .filter(|t| t.is_object())
            .context("auth.json holds no ChatGPT login (log in with `codex login`)")?;
        let account_id = tokens
            .get("account_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .context("auth.json has no tokens.account_id")?
            .to_owned();
        let access_token = tokens
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .context("auth.json has no tokens.access_token")?;
        let expires_at =
            jwt_expiry(access_token).context("cannot read the access token's expiry")?;
        // A write-back copies these into the controller's file, and Codex
        // refuses to load a file whose id_token is not a JWT or whose
        // last_refresh is not a timestamp. Check them here so a malformed
        // target file stops the run instead of breaking the controller.
        let id_token = tokens
            .get("id_token")
            .and_then(serde_json::Value::as_str)
            .context("auth.json has no tokens.id_token")?;
        jwt_claims(id_token).context("tokens.id_token is not a JWT")?;
        tokens
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .context("auth.json has no tokens.refresh_token")?;
        if let Some(last_refresh) = json.get("last_refresh").filter(|v| !v.is_null()) {
            last_refresh
                .as_str()
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .context("last_refresh is not an RFC 3339 timestamp")?;
        }
        Ok(Self {
            bytes,
            json,
            account_id,
            expires_at,
        })
    }
}

/// Extract the file [`READ_TARGET_LOGIN`] printed, ignoring anything a login
/// profile printed around it.
fn unframe(stdout: &str) -> anyhow::Result<Vec<u8>> {
    let (_, rest) = stdout.split_once(BEGIN).context("no begin marker")?;
    let (encoded, _) = rest.split_once(END).context("no end marker")?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("framed file is not base64")
}

/// The controller's `auth.json` with the target's tokens and `last_refresh`
/// swapped in, and nothing else from the target.
///
/// The target is a box an unsandboxed agent just configured, so a file from
/// it is trusted only as far as it has to be. Importing it whole would let a
/// tampered file change what kind of login the controller uses (an
/// `auth_mode` of `apikey` with some other `OPENAI_API_KEY`, say); copying
/// only the token fields cannot. What this does not stop is a tampered file
/// carrying another account's genuine tokens under this account's id: kd
/// does not verify JWT signatures, so it cannot tell. A target that can do
/// that already holds the GitHub, OpenCode and Muse logins bootstrap gave it.
fn merge_tokens(local: &Login, remote: &Login) -> anyhow::Result<Vec<u8>> {
    let mut merged = local.json.clone();
    let tokens = merged
        .get_mut("tokens")
        .and_then(serde_json::Value::as_object_mut)
        .context("controller auth.json has no tokens")?;
    for key in ["id_token", "access_token", "refresh_token"] {
        tokens.insert(key.to_owned(), remote.json["tokens"][key].clone());
    }
    if let Some(last_refresh) = remote.json.get("last_refresh").filter(|v| v.is_string()) {
        merged["last_refresh"] = last_refresh.clone();
    }
    // Codex writes this file with serde_json's pretty printer; match it so
    // the file looks like Codex's own.
    Ok(serde_json::to_vec_pretty(&merged)?)
}

/// A JWT's payload claims, the shape check Codex itself applies to
/// `id_token`. The signature is not checked: kd only reads claims from
/// tokens it already holds, and the server is what validates them.
fn jwt_claims(jwt: &str) -> anyhow::Result<serde_json::Value> {
    let mut parts = jwt.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("not a JWT");
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("JWT payload is not base64url")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).context("JWT payload is not JSON")?;
    if !claims.is_object() {
        bail!("JWT payload is not an object");
    }
    Ok(claims)
}

/// `exp` from a JWT's payload, which decides how old a token is.
fn jwt_expiry(jwt: &str) -> anyhow::Result<i64> {
    jwt_claims(jwt)?
        .get("exp")
        .and_then(serde_json::Value::as_i64)
        .context("JWT has no exp")
}

/// The expiry gate. `now` is a parameter so tests need no clock.
fn check_fresh(login: &Login, now: i64) -> anyhow::Result<()> {
    let remaining = login.expires_at - now;
    if remaining < MIN_REMAINING {
        bail!(
            "codex: the controller's login expires in {:.1}h; bootstrap needs {}h so neither \
             copy refreshes mid-run. Run `codex login` on the controller for a fresh one.",
            remaining as f64 / 3600.0,
            MIN_REMAINING / 3600
        );
    }
    Ok(())
}

/// Write back only a newer login of the same account. Another account on
/// the target is not the controller's business, and an older copy of the
/// same account (a box bootstrapped long ago) holds a spent token.
fn decide(local: &Login, remote: &Login) -> Reclaim {
    if remote.account_id == local.account_id && remote.expires_at > local.expires_at {
        Reclaim::WriteBack
    } else {
        Reclaim::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal auth.json in Codex's shape with an unsigned access token.
    /// Only `account_id` and the token's `exp` matter to kd.
    fn auth_json(account: &str, exp: i64) -> Vec<u8> {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!(
            "{}.{}.sig",
            engine.encode(r#"{"alg":"none"}"#),
            engine.encode(format!(r#"{{"exp":{exp}}}"#))
        );
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": token.clone(), "access_token": token, "refresh_token": "r", "account_id": account},
            "last_refresh": "2026-09-18T02:29:40Z"
        })
        .to_string()
        .into_bytes()
    }

    fn login(account: &str, exp: i64) -> Login {
        Login::parse(auth_json(account, exp)).unwrap()
    }

    /// The gate is what keeps both copies from refreshing during a run, so
    /// pin its boundary: exactly the minimum passes, a second less fails.
    #[test]
    fn preflight_requires_a_day_of_access_token_lifetime() {
        let now = 1_000_000;
        assert!(check_fresh(&login("a", now + MIN_REMAINING), now).is_ok());
        let err = check_fresh(&login("a", now + MIN_REMAINING - 1), now).unwrap_err();
        assert!(format!("{err:#}").contains("codex login"));
    }

    /// Only a newer token of the same account may replace the controller's.
    /// Importing an older one would swap a live refresh token for a spent
    /// one; importing another account would silently switch who the
    /// controller's Codex bills.
    #[test]
    fn write_back_only_a_newer_login_of_the_same_account() {
        let local = login("a", 100);
        assert_eq!(decide(&local, &login("a", 101)), Reclaim::WriteBack);
        assert_eq!(decide(&local, &login("a", 100)), Reclaim::Keep);
        assert_eq!(decide(&local, &login("a", 99)), Reclaim::Keep);
        assert_eq!(decide(&local, &login("b", 101)), Reclaim::Keep);
    }

    /// Codex writes padless base64url; tolerate padding anyway, and reject
    /// files kd cannot reason about instead of guessing their lifetime.
    #[test]
    fn parse_reads_expiry_and_rejects_logins_without_chatgpt_tokens() {
        assert_eq!(login("a", 1_790_000_000).expires_at, 1_790_000_000);
        assert!(Login::parse(br#"{"OPENAI_API_KEY":"sk-x"}"#.to_vec()).is_err());
        assert!(Login::parse(b"{".to_vec()).is_err());
        let no_account = String::from_utf8(auth_json("", 1)).unwrap();
        assert!(Login::parse(no_account.into_bytes()).is_err());
        assert!(jwt_expiry("h.eyJleHAiOjF9==.s").is_ok_and(|e| e == 1));
        // What a write-back would copy into the controller's file must be
        // loadable by Codex there.
        for (pointer, bad) in [
            ("/tokens/id_token", serde_json::json!("not-a-jwt")),
            ("/last_refresh", serde_json::json!("yesterday")),
            ("/last_refresh", serde_json::json!(5)),
        ] {
            let mut json: serde_json::Value = serde_json::from_slice(&auth_json("a", 1)).unwrap();
            *json.pointer_mut(pointer).unwrap() = bad;
            assert!(
                Login::parse(json.to_string().into_bytes()).is_err(),
                "{pointer}"
            );
        }
    }

    /// The target-side read decides between "nothing to reclaim", "refuse",
    /// and "here are the bytes"; a wrong exit code there either skips a
    /// live token or follows a symlink kd did not create. Real bash, with
    /// the home injected into the child only.
    #[test]
    fn target_read_distinguishes_absent_symlinked_and_present_logins() {
        let run = |home: &Path| {
            std::process::Command::new("bash")
                .args(["-c", READ_TARGET_LOGIN])
                .env("HOME", home)
                .output()
                .unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path()).status.code(), Some(4));

        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        let file = dir.path().join(".codex/auth.json");
        std::os::unix::fs::symlink("/nonexistent", &file).unwrap();
        assert_eq!(run(dir.path()).status.code(), Some(3));

        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, b"{}\n").unwrap();
        let out = run(dir.path());
        assert!(out.status.success());
        assert_eq!(
            unframe(&String::from_utf8(out.stdout).unwrap()).unwrap(),
            b"{}\n"
        );
    }

    /// Only the tokens cross from the target. A tampered target file must not
    /// be able to switch the controller to an API key or another auth mode.
    #[test]
    fn write_back_carries_only_the_tokens() {
        let local = login("a", 100);
        let mut remote_json: serde_json::Value =
            serde_json::from_slice(&auth_json("a", 200)).unwrap();
        remote_json["auth_mode"] = "apikey".into();
        remote_json["OPENAI_API_KEY"] = "sk-other".into();
        remote_json["tokens"]["refresh_token"] = "r2".into();
        remote_json["last_refresh"] = "2026-09-28T00:00:00Z".into();
        let remote = Login::parse(remote_json.to_string().into_bytes()).unwrap();
        let merged: serde_json::Value =
            serde_json::from_slice(&merge_tokens(&local, &remote).unwrap()).unwrap();
        assert_eq!(merged["OPENAI_API_KEY"], serde_json::Value::Null);
        assert!(merged.get("auth_mode").is_none());
        assert_eq!(merged["tokens"]["refresh_token"], "r2");
        assert_eq!(
            merged["tokens"]["access_token"],
            remote.json["tokens"]["access_token"]
        );
        assert_eq!(merged["tokens"]["account_id"], "a");
        assert_eq!(merged["last_refresh"], "2026-09-28T00:00:00Z");
    }

    /// Profile output around the framed file, before or after, must not leak
    /// into the credential; a missing marker is an error, never an empty file.
    #[test]
    fn unframe_ignores_profile_output_on_both_sides() {
        let framed = format!("motd\n{BEGIN}e30={END}\nbye from an EXIT trap\n");
        assert_eq!(unframe(&framed).unwrap(), b"{}");
        assert!(unframe("motd\n").is_err());
        assert!(unframe(&format!("{BEGIN}e30=")).is_err());
    }

    /// The write-back must not clobber a controller file that changed after
    /// kd read it, and must leave a fresh 0600 file behind when it does write.
    #[test]
    fn write_back_refuses_a_controller_file_that_changed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let controller = ControllerLogin { path: path.clone() };
        let local = login("a", 100);
        let remote = login("a", 200);

        std::fs::write(&path, b"changed").unwrap();
        assert!(controller.write_back(&local, &remote).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed");

        std::fs::write(&path, &local.bytes).unwrap();
        controller.write_back(&local, &remote).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            merge_tokens(&local, &remote).unwrap()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
