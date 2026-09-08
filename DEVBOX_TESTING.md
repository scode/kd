# Driving and testing devbox operations

This runbook is for the controller-side agent or person operating kd. Read the devbox contract in [SPEC.md](SPEC.md) and
the implementation constraints in [SPEC_impl.md](SPEC_impl.md). Use the user's existing authorization and stated
preferences; resolve missing destination or state-handling decisions before dependent operations. A request to test a PR
does not by itself select a production source to suspend or a machine to rebuild.

## Choose the flow

Identify the controller, intended target user, target SSH destination and hostname. For stateful operations, also
identify the source profile and archive directory. Inspect the actual local configuration; do not infer it from examples
or old session notes. Different source and target hostnames are supported: the source profile's hostname selects the
archive, while `--hostname` controls the target's OS hostname.

| Flow              | Source handling                                                             | Target behavior                                     |
| ----------------- | --------------------------------------------------------------------------- | --------------------------------------------------- |
| Scratch bootstrap | No source or archive needed                                                 | Configure development tools; leave Hermes untouched |
| Restore rehearsal | Prefer an archive taken while suspended; the source may resume after backup | Restore Hermes with services disabled and stopped   |
| Move              | Suspend, then back up; keep the source suspended                            | Restore and start services on the explicit target   |
| Live backup test  | Deliberately leave the source running                                       | Best-effort archive only; no service control        |

Suspend stops current Hermes operation but does not disable startup after reboot. Keep the source stopped throughout a
move. A failed backup never implicitly resumes it; inspect the failure and use the user's intended recovery plan. An
incomplete archive is a failed backup, even if it is a valid ZIP. Do not use it as evidence of a successful move.

Repositories are cloned, not backed up. Review the preflight's dirty and unpushed work, sessions and processes before
relying on a migration. Docker volumes and other application state, including Farhelm supervisor state, are outside the
current backup contract. Do not treat a successful Hermes backup as a whole-machine backup.

## Use the intended code and private configuration

Inspect the working copy and selected PR revision, then build with `cargo build`. Invoke `target/debug/kd` from that
checkout rather than an older installed `kd`. If the working copy includes uncommitted changes or descendant PRs, record
that fact. A component test should execute the exact script rendered by that code, not a hand-rewritten approximation of
the proposed fix.

Controller configuration is `$XDG_CONFIG_HOME/kd/devboxes.toml`, falling back to `~/.config/kd/devboxes.toml`. Inspect
the selected file before running; do not overwrite saved profiles to make a test work. When isolation is needed, use a
private temporary directory with `kd/devboxes.toml` under it, and set `XDG_CONFIG_HOME` only for the child invocation.
That overrides configuration selection, not the source of every credential or Git identity. Check those separately.

Before bootstrap, verify the configured public key, the two global Git author fields, and the availability of all four
agent credential sources. Do not print credential contents. Bootstrap copies only Git `user.name` and `user.email`; do
not transplant the laptop's whole Git config. Credential-copy success is not evidence of provider endorsement or
assurance about account policy. Follow the user's chosen authentication approach without silently substituting another.

Keep personal hostnames, usernames, addresses, secrets and local environment details out of tracked docs, fixtures,
screenshots and PR text. Record operational evidence in the user's designated private/excluded log when one exists;
check that it is excluded before committing. kd itself does not create a durable run log. Review captured output before
sharing it, and keep any credential-bearing artifacts private.

## Command recipes

These are placeholders: `instance` is a configured profile and `developer@target.example` is the authorized SSH
destination. Run from the selected checkout. Omit `--enroll-tailscale` unless enrollment is wanted; a rehearsal cannot
use it. Use `--plain-ssh` when ordinary SSH is intended rather than automatic tailnet transport selection.

Scratch bootstrap, with enrollment explicitly requested:

```sh
target/debug/kd devbox bootstrap --target developer@target.example --hostname disposable --plain-ssh --enroll-tailscale
```

The intended account may be absent. Bootstrap tries it first, then root to provision the user and passwordless sudo.
After hardening, use the intended account for follow-up SSH; root login is disabled.

To take a consistent backup, run suspension first, then backup only after suspension succeeds:

```sh
target/debug/kd devbox suspend --profile instance &&
  target/debug/kd devbox backup --profile instance
```

Inspect the preflight and archive result. `--yes` skips backup's confirmation; use it only when the existing user
instruction covers proceeding after that report. For a backup-and-resume flow, resume after the successful backup:

```sh
target/debug/kd devbox resume --profile instance
```

That archive can be rehearsed on a separate target while the source operates:

```sh
target/debug/kd devbox bootstrap --target developer@target.example --hostname rehearsal --restore instance --rehearsal --plain-ssh
```

For an actual move, keep the source suspended after backup and restore on the destination:

```sh
target/debug/kd devbox bootstrap --target developer@target.example --hostname replacement --restore instance --plain-ssh
```

Bootstrap never suspends the source for you. Do not resume the old source after a successful move. Before a later move
back, make the profile refer to the currently active source and its archive identity, then suspend and back up that
source. Restoring the original archive would discard work done after the first move.

## Inspect the result

Read every probe verdict and both phase reports, including workarounds. Exit zero means the probe ran, not that every
check passed. A failure needs diagnosis or an explicit unresolved result; do not silently classify it as harmless.

Verify the behavior affected by the change. For login or onboarding work, use a real interactive session as well as the
headless request. Reaching a workspace-trust prompt can prove the login wizard was skipped without granting trust or
starting work. Close test sessions afterwards. Do not kill the user's sessions or race an active CLI's config writes;
coordinate closure when the operation requires it.

For timezone work, inspect localtime data, any legacy timezone file, systemd and relevant environment overrides;
checking today's offset alone misses daylight-saving errors. For Git author work, verify the intended user's global
defaults and remember that repository-local or environment overrides may select another identity for a particular
commit. For rehearsals, verify restored services remain disabled and stopped.

Manually starting Hermes on a rehearsal target is no longer a stopped rehearsal. If the source is active too, this forks
real state and may process external work twice. Do it only when the user explicitly accepts that operation; there is no
automatic isolation or reconciliation. `resume --profile` acts on the profile's source, not an arbitrary rehearsal
target.

## Reruns and reporting

An idempotent rerun still runs both agent phases and may upgrade packages, reboot or spend time building. Existing tools
can avoid some installation work, but a fast rerun is not guaranteed. A restore rerun reimports the selected archive and
discards state accumulated on the target; do not use it casually after an interactive stateful test.

Use a targeted script test to validate a narrow deterministic repair, a rerun to check convergence, and a fresh target
to exercise first-run setup. Unit tests, a targeted live repair, a full scratch bootstrap, a restore rehearsal and an
actual migration are distinct evidence. None automatically proves the next. Manual tests are performed when requested or
needed within the authorized task; this runbook does not require a fresh remote bootstrap for every PR.

Report the tested revision, validation level, remaining failures and relevant target software versions without
publishing private host details. State separately whether code was merged, the fix was applied live, and the complete
flow was exercised. Keep the implementation log current if the user requested one. Targets retain real credentials after
testing; follow the user's cleanup instructions rather than automatically destroying them or deleting archives.
