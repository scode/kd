//! What `kd ubiworker create` and `kd boatworker create` share for bringing
//! up a new worker: Tailscale enrollment and the first-boot script.
//!
//! Both fleets come up on the tailnet the same way. [`tailscale`] mints a
//! one-use auth key for the fleet's tag, reads the operator's own login, and
//! renders the policy rule `create` prints. [`init_script`] renders the root
//! shell script that joins the tailnet with Tailscale SSH and then
//! bootstraps apt packages in the background. This code handles a live auth
//! key and writes a script that runs as root, so there is one copy of it
//! rather than one per worker command.
//!
//! What differs between the fleets is passed in: the Tailscale tag, the
//! key's label, and the Unix user in the printed rule. Everything that only
//! one fleet needs stays with its command, including how the script reaches
//! the machine (`ubi vm create`'s init script, a setup file for `boat new`)
//! and the summary `create` prints. A change here changes what every new
//! worker of both kinds runs on first boot.
//!
//! The env preflight below lives here as well because both commands read the
//! same Tailscale OAuth pair through it. ubiworker's other subcommands also
//! use it, for `UBI_TOKEN`.

pub mod init_script;
pub mod tailscale;

use anyhow::bail;

// ── Env preflight ────────────────────────────────────────────────────────
// Every worker subcommand checks its credentials up front, before any
// provider CLI call or Tailscale request. Two rules shape the error:
// report *all* missing variables at once (so a fresh machine is fixed in
// one round-trip, not one variable per failed run), and tell the reader
// where to go get each value, since the person hitting this is usually a
// human with a browser rather than a script.

/// A credential kd needs from the environment, paired with where a human
/// goes to obtain it.
///
/// The `how_to_get` text is console-navigation prose. Vendor consoles move
/// things around without notice, so treat a stale path here as a bug to
/// fix, not as a reason to drop the hint — even a slightly-off pointer
/// beats a bare "is not set".
pub(crate) struct EnvVar {
    pub(crate) name: &'static str,
    pub(crate) how_to_get: &'static str,
}

/// Tailscale OAuth client secret, paired with the client id each worker
/// command defines for itself. Shown exactly once at creation, so a lost
/// secret means generating a new client.
///
/// The id is per command because its hint names the tag the client must
/// own, which differs between fleets. The secret's hint names no tag, so
/// one definition serves both.
pub(crate) const TS_API_CLIENT_SECRET: EnvVar = EnvVar {
    name: "TS_API_CLIENT_SECRET",
    how_to_get: "shown exactly once when the OAuth credential is generated at \
                 https://console.tailscale.com/admin/settings/trust-credentials (starts with \
                 tskey-client-); if you no longer have it, create a new credential to get a \
                 fresh id+secret",
};

/// Read a set of required env vars, failing with one combined error that
/// names every missing variable and how to obtain each.
///
/// Returns values in the same order as `vars`. Called directly from each
/// command's `run()`, not from any helper below it — helpers take
/// credentials as parameters instead, which is both what keeps them
/// testable without env mutation and what keeps the secret's flow through
/// the program legible from function signatures alone.
pub(crate) fn require_envs(vars: &[EnvVar]) -> anyhow::Result<Vec<String>> {
    require_envs_with(vars, |name| std::env::var(name).ok())
}

/// [`require_envs`] with the environment lookup injected, so the
/// report-everything behavior is testable without touching process env.
/// An empty value counts as unset: an `export FOO=` left behind by a
/// half-edited shell profile should get the same help as no `export` at all.
fn require_envs_with(
    vars: &[EnvVar],
    lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<String>> {
    let mut values = Vec::with_capacity(vars.len());
    let mut missing = Vec::new();
    for var in vars {
        match lookup(var.name) {
            Some(value) if !value.is_empty() => values.push(value),
            _ => missing.push(var),
        }
    }
    if missing.is_empty() {
        return Ok(values);
    }
    use std::fmt::Write as _;
    let mut msg = String::from("missing required environment variable");
    if missing.len() > 1 {
        msg.push('s');
    }
    msg.push(':');
    for var in &missing {
        // Writing into a String is infallible, so the Result is safe to drop.
        let _ = write!(
            msg,
            "\n  {}\n    how to get it: {}",
            var.name, var.how_to_get
        );
    }
    bail!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── require_envs ─────────────────────────────────────────────────────

    const A: EnvVar = EnvVar {
        name: "A",
        how_to_get: "get A here",
    };
    const B: EnvVar = EnvVar {
        name: "B",
        how_to_get: "get B here",
    };

    /// The whole point of the batch preflight: a machine missing several
    /// credentials gets every one named, with its how-to hint, in a single
    /// failure rather than one per run. Also pins that an empty value is
    /// reported as missing, not silently passed through as "".
    #[test]
    fn require_envs_reports_every_missing_var_with_instructions() {
        let err = require_envs_with(&[A, B], |name| match name {
            "A" => None,
            "B" => Some(String::new()),
            _ => unreachable!(),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("variables:"), "{msg}");
        assert!(msg.contains("A\n") && msg.contains("get A here"), "{msg}");
        assert!(msg.contains("B\n") && msg.contains("get B here"), "{msg}");
    }

    /// With everything set, values come back in request order so callers
    /// can destructure positionally.
    #[test]
    fn require_envs_returns_values_in_request_order() {
        let values = require_envs_with(&[B, A], |name| Some(format!("{name}-val"))).unwrap();
        assert_eq!(values, vec!["B-val".to_string(), "A-val".to_string()]);
    }

    /// A single missing var must not be lumped into a pluralized message:
    /// singular wording keeps the common case reading naturally.
    #[test]
    fn require_envs_uses_singular_for_one_missing_var() {
        let err =
            require_envs_with(&[A, B], |name| (name == "A").then(|| "x".to_string())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("variable:") && !msg.contains("variables:"),
            "{msg}"
        );
        assert!(msg.contains("B\n") && msg.contains("get B here"), "{msg}");
        assert!(!msg.contains("get A here"), "{msg}");
    }
}
