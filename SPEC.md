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
