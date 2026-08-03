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
- `ssh` deliberately bypasses host-key checking
  (`-o UserKnownHostsFile=/dev/null -o
  GlobalKnownHostsFile=/dev/null -o StrictHostKeyChecking=no`) rather than
  pinning host keys normally — both the per-user and the system-wide known-hosts files are bypassed, since an entry in
  either could otherwise still trigger the failure this exists to avoid. Worker names are reused and every fresh VM boot
  mints a fresh host key, so ordinary known-hosts pinning would hard-fail reconnecting to a worker recreated under a
  previously-used name. This also avoids permanently polluting `~/.ssh/known_hosts` with entries for VMs that no longer
  exist. With host keys off, endpoint identity is delegated entirely to Tailscale: only a device the tailnet's ACLs
  permit to reach a worker's tailscale address can connect to it at all. That's reachability, not the one-use enrollment
  key itself — the key only gates how a worker _joins_ the tailnet, not who can subsequently reach it once it has. This
  is judged an acceptable trade (friction-free reconnection to a disposable, frequently-recreated fleet, in exchange for
  trusting the tailnet's own ACL enforcement instead of host-key pinning), but two specific residual risks are accepted
  as a result, not eliminated by it:
  - the short MagicDNS name (`ubiworker-foo`, without a tailnet suffix) is the _only_ host-identity claim `ssh` is
    given; if it fails to resolve via MagicDNS, an unresolved name can fall through to the system's ordinary DNS search
    domains and potentially reach a host that isn't the intended worker at all, with no host-key check to catch it.
  - recreating a worker under a previously-used name races Tailscale's own reaping of the old ephemeral node: if the old
    node hasn't been reaped yet, the new worker's tailnet identity gets a `-1` (or similar) suffix instead of the plain
    name, so the plain short name can keep resolving to the _stale_, destroyed node for a window after recreation. Both
    are accepted for now given the single-operator, disposable-fleet scope. The known remedy, left as a possible
    follow-up rather than implemented here, is verifying resolution against `tailscale status --json` (which reports
    live node identities) instead of trusting bare MagicDNS/DNS resolution.
- `ssh` does not poll or wait for a worker to finish enrolling into the tailnet before connecting (mirroring `create`'s
  no-polling stance above): connecting to a not-yet-enrolled worker just surfaces `ssh`'s own ordinary connection error.
- `ssh` execs directly into the `ssh` binary, replacing the `kd` process rather than spawning and waiting on it. `ssh`'s
  exit code, signals, and tty handling all pass straight through unmodified, exactly as if `ssh` had been invoked
  directly. Any arguments after the worker name (or after `--`, if no name is given) are forwarded to `ssh` verbatim,
  after the connection destination. The child's environment has `UBI_TOKEN`/`TS_API_CLIENT_ID`/`TS_API_CLIENT_SECRET`
  stripped before the exec: `ssh` never needs them, and an ssh-config helper (`ProxyCommand`, `SendEnv`, etc.) would
  otherwise inherit them by default.
- On an exec failure (e.g. `ssh` not found on `PATH`), the error names only the program and the destination
  (`scode@ubiworker-foo`), never the full forwarded argv: forwarded ssh arguments can carry secrets, and joining them
  into one string for an error message would also lose their original argument boundaries.
