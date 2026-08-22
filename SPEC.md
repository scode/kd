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

## kd ubiworker

- Ownership of a VM is structural, not tracked in a side database: a VM is a "ubiworker" iff its name starts with
  `ubiworker-` _and_ it lives in location `us-east-a2`. Both conditions are required.
- Infra shape (location, size, storage, boot image) is a set of hardcoded constants, not CLI flags. A ubiworker is meant
  to be one fixed, disposable shape; something else should be built by hand with `ubi` directly.
- A default worker name is `ubiworker-YYYYMMDD-HHMMSS` in the local timezone, with no collision-avoidance suffix.
  Ubicloud rejects a duplicate name server-side, so kd doesn't need to detect the collision itself.
- `create` returns as soon as `ubi vm create` returns. It does not poll for the VM to actually join the tailnet.
- Every minted tailscale auth key is one-use, ephemeral, preauthorized, tagged `tag:ubicloud`, and expires after one
  hour. The VM's own first-boot script retries joining the tailnet several times over several minutes; because the key
  is only consumed by a _successful_ `tailscale up`, retrying with the same key across failed attempts is safe.
- Workers enroll with Tailscale SSH enabled (`tailscale up --ssh`). This is what makes `tailscale ssh <name>` — and
  therefore `kd ubiworker ssh` — work: a node that merely joins the tailnet is not a Tailscale SSH server and has no
  Tailscale-managed host key for clients to verify against. The Ubicloud-provisioned OpenSSH server (with the `laptop`
  and `devbox` keys) keeps running, but Tailscale SSH takes over port 22 on the worker's _tailnet_ address, and the
  MagicDNS name resolves to that address: plain `ssh <user>@<name>` is therefore still Tailscale SSH (tailnet policy,
  Tailscale host key), not an independent fallback. The system sshd and the installed keys are reachable only via a
  non-tailnet address such as the VM's public IP. Workers created before this behavior existed do not advertise a
  Tailscale SSH host key and are unreachable through `kd ubiworker ssh` until `tailscale set --ssh` is run on them (via
  their old access path) or they are recreated (see README).
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
