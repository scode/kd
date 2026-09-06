# SPEC.md

This file records intentional behavior that is easy to mistake for a bug during review.

## ImageMagick-dependent tests

The image resizing tests may skip themselves when the `magick` command is unavailable.

ImageMagick is still a runtime requirement for image operations. The skip exists so ordinary Rust validation can run in
environments that have the Rust toolchain but not the external image-processing binary installed. A test environment
that needs to prove end-to-end thumbnail behavior must install ImageMagick and run the same tests with `magick`
available on `PATH`.

This means a passing `cargo test` run without ImageMagick proves the pure Rust code still compiles and its
non-ImageMagick helpers still behave correctly. It does not prove that thumbnail resizing works end to end.

## kd gh repo apply-preferred-settings

- The preferred set enforced on every repo:
  - squash merge enabled; merge commits and rebase merges disabled
  - squash commit title `PR_TITLE`, squash commit message `PR_BODY`
  - delete branch on merge enabled
  - Actions `default_workflow_permissions` set to `read`, `can_approve_pull_request_reviews` set to `false`
    (`actions/permissions/workflow`)
  - Actions fork-PR contributor approval policy set to `all_external_contributors`
    (`actions/permissions/fork-pr-contributor-approval`) -- public repos only, see below
  - On non-public repos, the mirror-image Actions private-fork-workflow lockdown
    (`actions/permissions/fork-pr-workflows-private-repos`) instead: `run_workflows_from_fork_pull_requests`,
    `send_write_tokens_to_workflows`, `send_secrets_and_variables`, and `require_approval_for_fork_pr_workflows` all set
    to `false` -- see below
- Every setting is re-checked from scratch on every run; there is no stored state. Writes are issued only when an apply
  actually proceeds -- a delta exists or `--force` is given, and neither `--dry-run` nor a declined confirmation prompt
  stopped it. When it does proceed, each endpoint (merge settings, workflow permissions, fork-PR approval or
  private-fork-workflows) is written only when its own group has drift, except under `--force`, where merge settings and
  workflow permissions are always re-asserted and the applicable one of fork-PR approval / private-fork-workflows is
  re-asserted whenever it's applicable (see below) -- regardless of whether that particular endpoint's group has a
  delta.
- Writes are sequential and non-atomic: the merge-settings PATCH, then the workflow-permissions PUT, then the applicable
  fork-PR-approval or private-fork-workflows PUT, each awaited before the next starts. A failure partway through leaves
  the earlier writes in place; there is no rollback. Rerunning the command re-fetches settings and reconciles whatever's
  still outstanding, without repeating writes that already landed.
- Exactly one of the two fork-workflow endpoints is applicable to a given repo, and applicability is the mirror image of
  the other: public repos get the fork-PR-approval policy; non-public repos (private, or -- on GitHub Enterprise --
  internal) instead get the private-fork-workflow settings, where `run_workflows_from_fork_pull_requests=false` means
  fork-PR workflows never run on the repo's runners at all, making the other three fields moot in practice -- they're
  still asserted `false` so a future re-enable of fork workflows doesn't inherit permissive companions.
  `require_approval_for_fork_pr_workflows` in particular is only meaningful once fork workflows run again; `kd` never
  sets it `true`, only ever corrects it back to `false`. Each endpoint is read, asserted, and written only where it's
  applicable -- the other endpoint 422s there, so it's never even requested, and its absence is never mentioned in
  `deltas` or logged output, `--force` included. Applicability is keyed on the API's `visibility` field being exactly
  `"public"` vs. not, not on the separate boolean `private` field GitHub also returns (which can't distinguish `private`
  from `internal`). Because every run re-checks all settings, the applicable policy lands automatically on the first run
  after a repo's visibility changes -- nothing needs to notice or react to the transition itself.
- `first_time_contributors` and `first_time_contributors_new_to_github` are deliberately not acceptable values for the
  fork-PR approval policy, even though GitHub allows them: both auto-run a fork PR's workflow once that contributor has
  a single prior merged PR, which defeats the point of gating billable/self-hosted-style runner access behind maintainer
  approval. `all_external_contributors` is the only value `kd` treats as already-correct.
- `--all` is bounded at 1,000 repos (`gh repo list --limit 1000`), and every repo costs up to ~6 API requests across
  `get_settings` and `apply_settings`. GitHub's 5,000-requests/hour rate limit is not handled -- a run that hits it
  stops with an error, and the fix is simply to rerun the command later: already-correct repos cost only cheap reads on
  the rerun, so no progress is lost. This is an explicitly accepted limitation, not an oversight.

## kd ubiworker

- Ownership of a VM is structural, not tracked in a side database: a VM is a "ubiworker" iff its name starts with
  `ubiworker-` _and_ it lives in location `us-east-a2`. Both conditions are required.
- Infra shape (location, size, storage, boot image) is a set of hardcoded constants, not CLI flags. A ubiworker is meant
  to be one fixed, disposable shape; something else should be built by hand with `ubi` directly. Apt packages are the
  one deliberate exception: `create` combines a hardcoded base set (`BASE_PACKAGES`, currently empty) with repeatable
  `--pkg` flags, because "what software this worker needs" is inherently a per-create decision in a way the rest of the
  shape isn't.
- A default worker name is `ubiworker-YYYYMMDD-HHMMSS` in the local timezone, with no collision-avoidance suffix.
  Ubicloud rejects a duplicate name server-side, so kd doesn't need to detect the collision itself.
- Every subcommand preflights the credential env vars it needs (`UBI_TOKEN` for all; `TS_API_CLIENT_ID` and
  `TS_API_CLIENT_SECRET` additionally for `create`) before any `ubi` call or Tailscale request. If any is missing, the
  error names _every_ missing variable at once, each with where a human obtains the value, so a fresh machine is fixed
  in one round-trip. An empty value counts as unset.
- `create` returns as soon as `ubi vm create` returns. It does not poll for the VM to actually join the tailnet.
- Every minted tailscale auth key is one-use, ephemeral, preauthorized, tagged `tag:ubicloud`, and expires after one
  hour. The VM's own first-boot script retries joining the tailnet several times over several minutes; because the key
  is only consumed by a _successful_ `tailscale up`, retrying with the same key across failed attempts is safe.
- Workers enroll with Tailscale SSH enabled (`tailscale up --ssh`). This is what makes `tailscale ssh <name>` — and
  therefore `kd ubiworker ssh` — work: a node that merely joins the tailnet is not a Tailscale SSH server and has no
  Tailscale-managed host key for clients to verify against. The Ubicloud-provisioned OpenSSH server (with every SSH key
  registered in the Ubicloud account installed on it) keeps running, but Tailscale SSH takes over port 22 on the
  worker's _tailnet_ address, and the MagicDNS name resolves to that address: plain `ssh <user>@<name>` is therefore
  still Tailscale SSH (tailnet policy, Tailscale host key), not an independent fallback. The system sshd and the
  installed keys are reachable only via a non-tailnet address such as the VM's public IP. Workers created before this
  behavior existed do not advertise a Tailscale SSH host key and are unreachable through `kd ubiworker ssh` until
  `tailscale set --ssh` is run on them (via their old access path) or they are recreated (see README).
- `create` installs _every_ SSH key currently registered in the Ubicloud account (`ubi sk list`) — there is no hardcoded
  key name or count. If none are registered, `create` fails before minting a Tailscale key or creating a VM: a worker
  with no authorized_keys entries would be unreachable over plain ssh, and that's a preflight failure worth having
  rather than a silently-bricked worker.
- `create --pkg PACKAGE` (repeatable) requests apt packages for the worker, on top of the hardcoded `BASE_PACKAGES` base
  set (currently empty). Every name in the _combined_ list is validated against Debian package-name syntax, with a
  trailing `-` additionally rejected (apt reads `name-` as "remove"). Validation happens before any billable or
  secret-minting step — the same preflight position as the unix-user check above — and again, defensively, inside the
  init-script renderer; a bad name is an error naming it, never silently dropped.
- Installation, plus an unconditional `apt-get dist-upgrade`, runs asynchronously on the worker in a transient systemd
  unit named `kd-bootstrap`, launched only _after_ tailscale enrollment has succeeded and never waited on by `create` or
  by the init script. After, not before or alongside, because tailscale's installer (`tailscale.com/install.sh`) runs
  its own `apt-get install` with no dpkg-lock timeout: a bootstrap started earlier — whose apt calls wait up to 600s for
  the lock and can hold it for minutes during a dist-upgrade — would make the installer fail instantly and could exhaust
  enrollment's 5×30s retry budget. Packages are installed with `apt-get satisfy` (apt ≥ 2.0), not `apt-get install`:
  `satisfy` matches names exactly, whereas `install` treats an unmatched name containing `.` as a regex and installs
  every match (`lib.` would install everything containing `lib`), and honors the trailing-`-` remove suffix.
- Progress is visible on the worker via `systemctl status kd-bootstrap` and `/var/log/kd-bootstrap.log`; success is
  marked by `/var/lib/kd/bootstrap-done`. `create` does not wait for or report bootstrap completion — the summary names
  only the requested packages and the log path. An operator who `ssh`s in before bootstrap finishes may find
  `apt`/`dpkg` locked by the still-running unit. A `dist-upgrade` that installs a new kernel does not reboot the VM.
  Re-running the init script by hand on a VM where the unit already exists gets a harmless "Unit kd-bootstrap.service
  already exists" from `systemd-run`, swallowed by the same best-effort guard that covers a launch failure; clear the
  stale unit first with `systemctl reset-failed kd-bootstrap`. The init script itself runs once per instance.
- _Who_ may log in over Tailscale SSH, and as which Unix user, is decided by the tailnet policy's `ssh` section, which
  kd neither reads nor edits: workers are owned by `tag:ubicloud`, so Tailscale's default "SSH to your own devices" rule
  does not cover them and a rule granting the operator access to `tag:ubicloud` as the provisioned user must exist, as
  must an ordinary ACL/grant reaching `tag:ubicloud` on port 22. kd deliberately does not try to install that rule
  itself: it would require the OAuth client to hold policy-file write scope — authority over every access rule on the
  tailnet, far beyond the `auth_keys` scope kd otherwise needs — and the policy file is hand-edited HuJSON that a
  programmatic insert could easily mangle.
- Instead, `create`'s summary (stdout, printed on every successful create) states the provisioned Unix user and an
  example `ssh` rule _object_ granting that user on `tag:ubicloud` — an object to append to the policy's existing `ssh`
  array, never a whole `"ssh": [...]` property that would duplicate or replace the rules already there. The rule's `src`
  is the operator's own Tailscale login, resolved best-effort from the local `tailscale status --json` (`Self.UserID` →
  `User[..].LoginName`); when that can't be determined (no `tailscale`, tailscaled down, or a tagged node, whose login
  is the synthetic `tagged-devices`) the summary prints an unmistakable `<your-tailscale-login>` placeholder. kd never
  suggests `autogroup:member`: on a shared tailnet that is every member, and copy-pasted security examples tend to
  become production policy unchanged. The summary also names the port-22 ACL/grant requirement and the policy editor URL
  / console path, the latter two being best-effort guidance that Tailscale can move at any time.
- The Unix account provisioned on a worker (`ubi vm create --unix-user`) is the local username of whoever runs `create`,
  as reported by `id -un` — not a constant and not a flag. `ssh` logs in as the local username too, so the implied
  contract is that both are run by the same person under the same username; kd records nothing about which user a worker
  was created with. The username is validated exactly as reported (no whitespace normalization) against Ubicloud's own
  rule, `[a-z_][a-z0-9_-]{0,31}`, so a name Ubicloud would refuse fails before any key is minted or VM billed rather
  than at `ubi vm create`; `root` is additionally refused, since that is what `id -un` reports under `sudo` and
  accepting it would turn an accidental `sudo kd ubiworker create` into a worker whose only account — and printed policy
  grant — is direct remote root.
- The minted auth key passes through the `ubi` process's argv on the host running `kd`, and is visible to local process
  inspection (e.g. procfs) for as long as that `ubi` invocation runs. kd strips `UBI_DEBUG` from `ubi`'s environment and
  redacts its own logging/error output, but the argv exposure itself is accepted current behavior. The only way to
  remove it entirely is to stop shelling out to the `ubi` CLI and create VMs via Ubicloud's REST API instead — a larger
  change left for later.
- `list` prints nothing to stdout when no ubiworkers exist; it logs a note at info level to stderr instead, so a caller
  piping `list`'s output sees a clean empty stream rather than a line to filter out.
- `destroy` takes any number of names, or `--all` for every owned worker; with neither, it falls back to the old
  sole-worker behavior (exactly one owned worker, or an error). It always resolves targets by listing owned VMs first,
  and destroys by the VM's immutable id, not by name. Malformed `ubi vm list` output is a hard error rather than
  something `destroy`/`list` tolerate and skip past.
- `destroy` has no confirmation prompt. This is deliberate, not an oversight: kd is a single-operator tool, and the
  interactive "Destroy \<name\>? [y/N]" step it used to have cost more in friction than it protected — `ubi`'s own
  destroy command has its own interactive confirmation, and kd bypasses it with `-f` precisely because kd's
  listing-based name resolution (only ever matching a VM kd considers owned, per the ownership convention above) is
  already the guard against destroying the wrong thing.
- Resolution of a multi-name request is fail-closed: every requested name is looked up against the listing _before_
  anything is destroyed. If any name doesn't match, the error names every missing one and nothing is destroyed — a typo
  in one of several names must not turn into a half-executed bulk destroy of the names that did resolve. Duplicate names
  (including names made equivalent by prefix normalization, e.g. `foo` and `ubiworker-foo`) are deduped; a name given
  twice destroys its VM once.
- `--all` performs no name lookup at all — it takes every owned worker from the listing. With zero owned workers it is a
  deliberate no-op success (not an error), so scripted "destroy everything, if anything exists" cleanup doesn't have to
  special-case the empty listing.
- Worker enumeration is a single `ubi vm list` page, and Ubicloud caps a page at 1,000 rows. Past that cap, listing and
  bulk destruction would operate on (at most) the first page — an accepted limitation for a single-operator tool whose
  fleet is a handful of VMs, recorded here so it isn't mistaken for a guarantee at larger scale.
- Once resolution succeeds, execution itself is not atomic: targets are destroyed one at a time, in sequence, and a
  failure partway through leaves everything destroyed so far destroyed, with no rollback. Re-running is the recovery for
  whatever's left.
- Destruction is asynchronous on Ubicloud's side (`ubi` reports "scheduled for destruction"), so a just-destroyed worker
  keeps showing up in `kd ubiworker list` until Ubicloud actually reaps it. That's Ubicloud's semantics surfacing, not
  stale caching in kd — kd holds no state of its own.
- `ssh` resolves its target through the same owned-worker listing as `destroy`, so it needs `UBI_TOKEN` and issues one
  `ubi vm list` round-trip before it ever touches the network to the VM itself. This means a typo'd name fails with a
  clear kd error naming the worker, rather than surfacing as a DNS/connection failure from `ssh` itself. With no name
  given, it targets the sole owned ubiworker, exactly like `destroy`'s no-argument form (an error if zero or multiple
  exist); it has no `--all` equivalent, since ssh only ever connects to one worker at a time.
- `ssh` takes a single trailing argument vector, not a separate name positional. The first element is the worker name
  unless it starts with `-`, in which case there is no name and every element is forwarded to `ssh`; this rule is sound
  because no valid worker name can start with a hyphen (the Ubicloud name charset requires an alphanumeric first
  character). One consequence worth calling out explicitly: `kd ubiworker ssh -- uptime` targets a worker named `uptime`
  (normalized to `ubiworker-uptime`), not a remote `uptime` command run on the sole worker — under the same charset, a
  bare word is indistinguishable from a name, and the rule always resolves it as one. A leading token that collides with
  one of kd's own registered flags (`-v`/`-q`/`-h`) is claimed by kd before `ssh` ever sees it, exactly as it would be
  anywhere else on the command line; `--` is the escape hatch that forces it through to `ssh` instead (e.g.
  `kd ubiworker ssh -- -v`). An ssh flag kd doesn't itself recognize (e.g. `-L 8080:localhost:80`) needs no `--` at all.
- `ssh` connects via `tailscale ssh <user>@<name>`, never via plain `ssh`, and passes no host-key options of its own.
  Ordinary known-hosts pinning is a poor fit for this fleet: worker names are reused and every fresh VM boot mints a
  fresh OpenSSH host key, so reconnecting to a worker recreated under a previously-used name would hard-fail with a
  "REMOTE HOST IDENTIFICATION HAS CHANGED" refusal (and `~/.ssh/known_hosts` would fill with entries for VMs that no
  longer exist). `tailscale ssh` sidesteps that without giving up verification: it wraps the system `ssh`, resolving the
  name via MagicDNS and checking the worker's host key against the Tailscale-managed key advertised through the
  coordination server (materialized into a Tailscale-managed known-hosts file passed as `UserKnownHostsFile` under
  strict checking, with a `ProxyCommand` through tailscaled for transport where needed), which is tied to the node's
  tailnet identity rather than to whatever OpenSSH key the VM generated this boot. That is why the previous
  implementation's deliberate `StrictHostKeyChecking=no` bypass is gone rather than merely optional: kd's ssh path is
  meant to be safe by default, and anyone who wants unverified plain ssh can run it by hand outside kd. The residual
  risk that remains is the recreation race: recreating a worker under a previously-used name races Tailscale's own
  reaping of the old ephemeral node, and if the old node hasn't been reaped yet the new worker's tailnet identity gets a
  `-1` (or similar) suffix instead of the plain name, so the plain short name can keep pointing at the _stale_,
  destroyed node for a window after recreation. With Tailscale SSH the failure mode is a connection error or a refusal,
  not a silent connection to the wrong host — but it is still accepted rather than handled; the known remedy, left as a
  possible follow-up, is resolving against `tailscale status --json` (which reports live node identities) before
  connecting.
- The user is passed explicitly as `<user>@<name>` (the local username, per the `create` note above) even though
  `tailscale ssh` would default to the same value, so the destination is self-describing in `ps` and error output and
  does not depend on `tailscale ssh`'s defaulting rules.
- `ssh` does not poll or wait for a worker to finish enrolling into the tailnet before connecting (mirroring `create`'s
  no-polling stance above): connecting to a not-yet-enrolled worker just surfaces the ordinary connection error.
- `ssh` execs directly into the `tailscale` binary, replacing the `kd` process rather than spawning and waiting on it.
  The exit code, signals, and tty handling all pass straight through unmodified, exactly as if `tailscale ssh` had been
  invoked directly. Any arguments after the worker name (or after `--`, if no name is given) are forwarded verbatim
  after the connection destination — `tailscale ssh` hands them to the underlying `ssh` unchanged, so a remote command
  or ssh flags like `-L` work as they would with plain `ssh`. The child's environment has
  `UBI_TOKEN`/`TS_API_CLIENT_ID`/`TS_API_CLIENT_SECRET` stripped before the exec: neither `tailscale` nor `ssh` needs
  them, and an ssh-config helper (`ProxyCommand`, `SendEnv`, etc.) would otherwise inherit them by default.
- On an exec failure (e.g. `tailscale` not found on `PATH`), the error names only the program and the destination
  (`<user>@ubiworker-foo`), never the full forwarded argv: forwarded ssh arguments can carry secrets, and joining them
  into one string for an error message would also lose their original argument boundaries.

## kd devbox

NOTE: This is a solo-developer convenience for bootstrapping disposable environments and moving a stateful instance. The
box can be anything that runs Ubuntu, can be reinstalled out of band, and comes back reachable over SSH with a public
key you supplied (a hosted VM with a reinstall button is the typical case, but nothing depends on that). It is
deliberately not a provisioning framework. The deterministic code is kept small on purpose, and mechanical work on the
target is done by a Codex agent running on that target; see `SPEC_impl.md` for the division of labor. This section
describes only what the user sees.

`kd` never wipes anything. Create or reinstall machines by hand. Hermes is currently the only migrated application
state. Repositories are cloned from GitHub, not copied: local files, uncommitted changes, unpushed commits, Docker
volumes, and other application state are not backed up. Agent credentials come from the controller.

`suspend → backup → resume` returns the services to operation and leaves a new archive.
`suspend → backup → bootstrap
--restore NAME` moves the instance to an explicitly selected target, leaving the source
suspended. Bootstrap never contacts or suspends the source, so the operator must keep it suspended to avoid two active
copies.

Terms used below:

- The **controller** is the machine `kd devbox` runs on, in practice your laptop. It is the one machine that survives
  the reinstall, so it is where Hermes archives land, where the agent CLI credentials to copy onto the devbox come from,
  and where prompts are answered. It is never the box being rebuilt.
- An **instance** is the stateful application described by a named profile. Its current machine is the **source**. The
  **target** is the explicit SSH destination bootstrap writes to. A target need not replace any existing machine. A
  **rehearsal** is an explicit `--rehearsal` restore that leaves restored services disabled and stopped.

### Profiles

- `backup`, `suspend`, and `resume` require `--profile <name>`, even when only one exists. Scratch bootstrap needs only
  shared settings; `--restore NAME` selects a stateful profile when restoring.
- Profiles live in `$XDG_CONFIG_HOME/kd/devboxes.toml`, falling back to `~/.config/kd/devboxes.toml`. The file holds
  identity only and no secrets, so its permissions are not checked. Shared settings and optional named instances:

  ```toml
  [bootstrap]
  user = "scode" # default intended user on a target; source login default too
  public_key = "~/.ssh/id_ed25519.pub" # controller key to authorize on the box; ~ is expanded
  repos = ["scode/kd", "scode/voice"] # GitHub owner/name, each cloned to ~/git/<name>

  [devbox.NAME]
  host = "203.0.113.5" # current source for backup, suspend and resume
  hostname = "devbox" # archive identity and default hostname when restoring
  backup_dir = "~/devbox-backups" # controller archive directory; ~ is expanded
  # user = "other" # optional source login override
  ```

- Nothing privacy-sensitive (IPs, hostnames, usernames, key material) is compiled into `kd`; package and tool
  preferences are. Two things are deliberately in the second group even though they look like identity, and should not
  be "fixed" into profile fields: the target timezone (`America/Los_Angeles`), and the two repos `scode/dotfiles` and
  `scode/voice`, which are always cloned whether or not `repos` lists them because the dotfiles installer is what
  configures the box. This tool has one user.
- Move `user`, `public_key`, and `repos` from the old per-box format into `[bootstrap]`; remove per-box `public_key` and
  `repos`. Unknown fields fail rather than silently changing their meaning. No local config is migrated automatically.

### kd devbox backup --profile NAME [--yes]

- Checks the profile and creates its backup directory if needed. Agent credentials are not required.
- Prints a read-only preflight from the devbox — running agent processes, tmux sessions, and which `~/git/*` repos are
  dirty or have unpushed work — then asks "ok to proceed?". `--yes` skips the question but still prints the report. That
  question is the whole attestation; nothing is enumerated or waived item by item.
- Takes a full `hermes backup`, pulls the archive into the profile's backup directory, checks that its SHA-256 matches
  the one computed on the devbox, sets it to mode `0600`, and deletes the copy on the devbox. An incomplete archive is a
  failure, not a warning.
- Never stops or starts services, including on failure. Suspend first for a consistent migration backup. A live backup
  can fail if Hermes reports an incomplete archive (including skipped live sockets); suspend and retry rather than
  treating that archive as a complete move. `--keep-running` is removed because state preservation is unconditional.
- Prints the archive path and source `hermes --version`. It does not imply that the source is suspended or safe to wipe.
- `kd` never deletes, rotates, or prunes archives on the controller.

### kd devbox suspend --profile NAME

- Stops the Hermes gateway and dashboard without taking a backup. Already-stopped services are acceptable, but a
  surviving process or failure to verify suspension is an error. This suspends current operation; it does not disable
  service startup after a reboot. Keep the source stopped and do not reboot it during a move.

### kd devbox resume --profile NAME

- Starts the Hermes gateway and dashboard on the source again. This is the inverse of `suspend`, not a restore command
  or a required step after successful bootstrap.

### kd devbox bootstrap --target [USER@]HOST [--hostname NAME] [--restore PROFILE] [--rehearsal] [--plain-ssh] [--enroll-tailscale]

- Starting state, which is the premise of the whole command: a minimal Ubuntu LTS install that is already up, has
  outbound internet, and accepts an SSH connection from the controller, either as `root` or as the intended user with
  passwordless `sudo`. That is what a provider's reinstall or a freshly created ubiworker leaves behind, and it is all
  bootstrap assumes. Bootstrap does not install the OS, does not create or power on the machine, and does not need the
  intended user, hostname, packages, or anything else to exist yet. Everything from that state to a working devbox is
  bootstrap's job.
- Configures that target from shared settings: real user with passwordless sudo, full OS upgrade (rebooting if the
  upgrade asks for it), timezone `America/Los_Angeles`, SSH hardening (key-only, no root, no passwords), default-deny
  inbound firewall with SSH and the Tailscale interface allowed, unattended security updates without automatic reboot,
  the development toolchain and CLIs, every repo in the shared manifest cloned as a colocated Jujutsu repo,
  `scode/dotfiles` installed via its own installer, passwordless key-based `ssh localhost`, the four agent CLIs
  authenticated from the controller's caches, and `gh` authenticated. From scratch no archive is required or placed, and
  no Hermes components are installed or probed. `--hostname` is required from scratch; a restore defaults to the profile
  hostname, with an explicit override allowed. Archive selection always uses the source profile hostname.
- The target is required and never inferred from a source profile. A bare host uses `[bootstrap].user`; an explicit user
  overrides it. `root` cannot be the intended user; bootstrap falls back to root to create a missing account. SSH
  aliases and `ssh://USER@HOST:PORT` destinations are supported. Existing SSH authentication must work; the shared
  public key is authorized during seeding, not forced as an SSH identity file.
- A tailnet peer uses `tailscale ssh`; otherwise, or with `--plain-ssh`, use ordinary SSH. Scratch bootstrap and
  rehearsals use the user's SSH configuration and host-key handling. A non-rehearsal restore over ordinary SSH asks for
  console confirmation of the destination's SHA256/MD5 fingerprint and uses a temporary per-run known-hosts file, since
  a replacement may reuse an address with an old key. SSH aliases and ports are resolved before scanning. Restore
  destinations using a proxy must also be directly reachable by `ssh-keyscan`. The completion reminder names
  `ssh-keygen -R`; the user handles any stale key in their own known-hosts file. Tailscale authenticates its own peers.
- `--rehearsal` requires `--restore` and conflicts with `--enroll-tailscale`. Like every bootstrap, it first tries the
  intended user and falls back to root if that login fails, provisioning the user, key and passwordless sudo. An
  existing user login needs passwordless sudo for setup. Hermes and its dashboard are restored without enabling or
  starting them, so they cannot compete with the source. It has no kd prompts; SSH may still require authentication or
  host-key acceptance.
- If `gh` on the target is unauthenticated, a rehearsal copies the controller's `gh auth token`; other runs prompt for a
  new classic token using the prefilled URL (scopes `repo`, `workflow`, `read:org`, `gist`). Authenticated reruns skip
  token collection. The four agent credentials must exist locally before connecting.
- `--enroll-tailscale` opts into installation, enrollment and its probe check. Existing enrollment is preserved. Before
  the agent phases, an unenrolled target prompts to free the intended hostname if replacing a stale device. At the end
  it prints the browser login URL and waits up to ten minutes. Enrollment is untagged, non-ephemeral and without
  Tailscale SSH. Without the flag, bootstrap neither installs nor changes Tailscale enrollment.
- A running Hermes gateway on the target means a live devbox that was not reinstalled. A non-rehearsal restore asks
  before continuing, because a previous attempt may already have restored Hermes; a rehearsal or scratch bootstrap
  refuses outright. Failure to check for a process is an error. Directory existence is never a guard: every phase is
  idempotent, so the recovery for any failure is "fix or ignore, then rerun the whole command". There is no partial
  resume.
- The GitHub token and the agent auth files are placed on the target as mode `0600` files for the agent to consume; the
  token file is deleted once `gh` has it. They are never passed as arguments or logged.
- With `--restore`, Hermes is restored from the newest archive in the backup directory whose name carries this profile's
  hostname, so two profiles can share a backup directory. A rerun re-imports that archive and discards whatever Hermes
  state the previous attempt accumulated. Outside a rehearsal, its gateway and loopback-only dashboard are enabled and
  started.
- Ends with a probe report printed as is: hostname, timezone, `gh auth status`, repo count against the manifest,
  `ssh localhost`, Docker as the user, and one real request through each agent CLI. Restores additionally check Hermes
  gateway state and, outside rehearsals, dashboard reachability. Tailscale is checked only with `--enroll-tailscale`.
  Probe failures are reported, never fatal: bootstrap exits 0 once the probe has run. After the probe, each agent
  phase's final message is printed whole, which is where the agent lists anything it had to work around, even when the
  run succeeded.
- After a rehearsal the worker is left running for inspection with a reminder that it holds real credentials; `kd` does
  not destroy it.
- Logging goes to stderr. There are no log files, receipts, or run records.
- The old bootstrap `--profile` and `--no-hermes` flags are removed. Restore is explicitly opt-in with `--restore`.
- If there is no terminal, the GitHub token is read as one plain line from stdin instead of the hidden prompt, so a
  scripted real run can pipe `y` for the fingerprint and then the token.
- Not in scope: triggering the reinstall through a provider API, Hermes version pinning or same-version restore, archive
  retention, deleting Tailscale devices, migrating anything beyond the Hermes archive and the four agent auth files.
