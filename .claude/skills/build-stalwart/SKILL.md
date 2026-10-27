---
name: build-stalwart
description: Use when building, releasing, or testing the Databanx fork of stalwart for production. Covers the production feature set, the local Docker ARM64 build environment, the binary distribution to mx1, the in-place swap procedure on the 1.0 unit (stalwart-v1), and the `vX.Y.Z-databanx-N` tag convention. Trigger phrases include "build the fork", "compile stalwart", "make a databanx tag", "deploy the patches", "ship to the mx servers", "test the binary on the server".
version: 3.0.0
---

# Building the Databanx fork of stalwart

The build is **slow and expensive to redo** — get the feature set right the first time. A cold build in the local Docker builder takes ~13 min (18 cores); the LTO fat + codegen-units=1 link step alone is several minutes. A version bump touches every workspace crate, so even with the cache warm expect ~15–17 min (v0.16.23: 14m46s; v0.16.26: 17m11s; v0.16.27: 17m39s). The 1.0.0 build took 15m39s cold and 12m12s warm.

## Hosts

- **Dev workstation** (Apple Silicon Mac, OrbStack) — **the build host**. Release binaries are produced in a local Linux ARM64 container (see "Docker build environment"). Native macOS builds are NOT release artifacts; macOS `cargo check -p <crate>` is fine for fast patch validation.
- **`mx1`** — the only production host. **It has no toolchain.** Always copy the built binary in, never try to build there. `mx2`/`mx3` were TERMINATED on 2026-10-23 (only `mx1` had been active since 2026-10-13). Check the `localmail-server-ops` skill for the current topology.

> **History (2026-06-11):** mx1 used to be the build host (`~/stalwart-build`). The Oracle downsize to 2 vCPU / 8 GB wiped `~/stalwart-build` and `~/.cargo`/`~/.rustup`. The FDB client libs (7.4.6) are still installed on mx1, but do not rebuild the toolchain there — the build now lives in Docker on the workstation, which is faster and doesn't compete with production for CPU.

**Production runs Stalwart 1.0 (`v1.0.0-databanx-1`) since 2026-10-27 05:57 UTC**, after the `stalwart-migrate` cutover from 0.16.27:

- systemd unit `stalwart-v1`, binary `/opt/stalwart-v1/bin/stalwart` (`stalwart:stalwart`), bootstrap config `/etc/stalwart-v1/config.json` (data store only), plus the unit's env file. Everything else lives in the registry, stored in the FDB cluster `fdb-v1` (`/etc/foundationdb-v1/fdb.cluster`), and is read and changed over JMAP (`x:*` objects) or `stalwart-cli` ≥ 1.0.14.
- The unit only starts while the marker `/etc/stalwart-v1/VIRADA-AUTORIZADA` exists (`ConditionPathExists`), and `ExecStartPre=/opt/stalwart-v1/bin/guard.sh stalwart.service` refuses to start while the 0.16 unit is active.
- The 0.16 unit `stalwart` (`/opt/stalwart/bin/stalwart`, `/opt/stalwart/etc/config.toml`) is stopped and disabled, and its drop-in `stalwart.service.d/no-v1.conf` keeps it from starting while the marker exists. The old FDB cluster (`foundationdb`, `/etc/foundationdb/fdb.cluster`) is stopped with its data kept.
- Listeners: 25 / 465 / 587 / 993 / 995 / 4190 / 8080 (7 `network.listen-start` on boot).

## Production feature set

The upstream `default = ["rocks", "enterprise"]` in `crates/main/Cargo.toml` is **wrong for this deployment** — RocksDB is not used. Production (verified against the 1.0 registry on `mx1`, 2026-10-27) uses **only**:

| Component | Backend | Feature |
|---|---|---|
| Data store + lookup + `directory.internal` | FoundationDB | `foundationdb` |
| Blob store | R2 (S3-compatible) | `s3` |
| Coordinator + in-memory (`x:Coordinator`, `x:InMemoryStore`) | Redis | `redis` |
| Search index | FoundationDB (`SearchStore` = Default) since the 1.0 cutover | — |
| Enterprise features | — | `enterprise` |

Meilisearch (the FTS backend on 0.16) is no longer used. Its client is compiled in regardless of features, so it still shows up in the `strings` check below.

So the build is:

```
cargo build --release \
    --no-default-features \
    --features "foundationdb s3 redis enterprise"
```

(`JEMALLOC_SYS_WITH_LG_PAGE=16` must be in the environment — the builder image sets it; see below.)

**Reference size**: the stripped aarch64 binary grows slowly with upstream (v0.16.8: ~63 MB; v0.16.20: 69.6 MB; v0.16.22: 70.1 MB; v0.16.23: 70.3 MB; v0.16.26: 70.4 MB; v0.16.27: 70.6 MB; v1.0.0: 82.5 MB, i.e. 82,494,656 bytes, an upstream jump with the same backends). Compare against the previous release's size in its tag message, not against a fixed number. A jump of several MB from one release to the next means someone re-added a backend — check the rustc invocation for `--cfg feature="rocks"` etc. The backend `strings` check below is the real test.

**Do not add `rocks`, `sqlite`, `postgres`, `mysql`, `nats`, `azure`, `zenoh`, or `kafka`** unless you have first verified the production config actually references that backend. Each one drags in a heavy dependency that bloats the binary and lengthens the build with zero runtime benefit. If a future config change adds a backend, update the feature list *and* this table — don't preemptively pad it.

**Symptom of a missing feature**: the server boots and immediately exits with `Startup failed: Binary was not compiled with the selected data store backend` (or analogous for blob/coordinator). If you see this, the binary is wrong — the config is fine.

## Docker build environment

Image `stalwart-builder-arm64`: Ubuntu 24.04 (same glibc as the mx hosts) + FDB client 7.4.6 (same as the cluster) + rustup stable + `JEMALLOC_SYS_WITH_LG_PAGE=16` baked in (the Oracle ARM64 kernels use 16 KiB pages; without it jemalloc panics at **runtime**, not build time).

Stalwart 1.0 needs **rustc ≥ 1.98**. The image was recreated on 2026-10-27 (`sha256:aa09b143…`, rustc 1.99.1) after the previous one vanished from OrbStack; an image older than that may carry a stable toolchain that is too old — check `rustc --version` inside it before a long build.

Dockerfile (recreate at `/tmp/stalwart-builder/Dockerfile` if the image is gone):

```dockerfile
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential clang libclang-dev pkg-config libssl-dev \
    curl ca-certificates git adduser \
    && rm -rf /var/lib/apt/lists/*

# FoundationDB client 7.4.6 (same as mx1).
# NOTE: the release asset is _aarch64.deb, NOT _arm64.deb.
# `adduser` above is a hard dep of this package on minimal images.
RUN curl -fsSL -o /tmp/fdb-clients.deb \
    https://github.com/apple/foundationdb/releases/download/7.4.6/foundationdb-clients_7.4.6-1_aarch64.deb \
    && dpkg -i /tmp/fdb-clients.deb && rm /tmp/fdb-clients.deb

RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH=/root/.cargo/bin:$PATH

# 16 KiB-page ARM64 kernels on the Oracle hosts
ENV JEMALLOC_SYS_WITH_LG_PAGE=16

WORKDIR /src
```

```
docker build -t stalwart-builder-arm64 /tmp/stalwart-builder
```

### Standard build flow

Run it from inside the worktree you are building (branch `update/<version>` with the fork patches applied), so `/src` is that tree:

```
SRC=$(git rev-parse --show-toplevel)
docker run --rm --name stalwart-build \
  -v "$SRC":/src \
  -v stalwart-cargo-registry:/root/.cargo/registry \
  -v stalwart-cargo-git:/root/.cargo/git \
  -v stalwart-target:/target \
  -e CARGO_TARGET_DIR=/target \
  stalwart-builder-arm64 \
  bash -xc 'cargo build --release --no-default-features --features "foundationdb s3 redis enterprise"'
```

The `bash -x` wrapper echoes the cargo command into the build log, so the review gate can confirm the feature set from the log instead of inferring it from the compiled crates (the v0.16.27 review had to infer it).

The named volumes (`stalwart-cargo-registry`, `stalwart-cargo-git`, `stalwart-target`) keep the cargo cache off the slow macOS bind mount and survive OrbStack restarts. Build time: ~13 min cold on 0.16, 15m39s cold on 1.0.0, a few minutes warm. Do not `docker volume rm` them casually.

Extract the binary and smoke-test it **inside the container** (it links `libfdb_c.so`, so it won't run on macOS):

```
docker run --rm -v stalwart-target:/target -v /tmp/stalwart-out:/out \
  stalwart-builder-arm64 bash -c 'cp /target/release/stalwart /out/ && /out/stalwart --version'
```

If OrbStack is memory-capped (`orb config show` → `memory_mib`), 16 GB is enough but tight for the LTO link; it was raised to 65536 on 2026-06-11. Changing it requires `orb stop` / `orb start` (kills running builds; the volumes preserve the cache).

### Verifying linked backends in the stripped binary

The release profile sets `strip = true`, so Rust module paths are gone — grep for **library-level markers** that survive stripping:

```
BIN=/tmp/stalwart-out/stalwart

# wanted backends — each count should be >= 1
# (Meilisearch is unused since 1.0 but always compiled in; v1.0.0-databanx-1: FoundationDB 7, x-amz 1, redis-rs 1, Meilisearch 4)
for m in "FoundationDB" "x-amz-content-sha256" "redis-rs" "Meilisearch"; do
    echo -n "$m: "; strings $BIN | grep -c "$m"
done

# should all be zero
for m in librocksdb tokio_postgres mysql_async rusqlite async_nats; do
    echo -n "$m: "; strings $BIN | grep -c "$m"
done
```

## Workstation shell gotchas (read this first)

- **SSH to the mx hosts can hang on a dead agent socket** in non-interactive harness shells (`ssh-add -l` itself hangs). Use `ssh -o IdentityAgent=none mx1 ...` — the `~/.ssh/databanx` key works without the agent. Symptom of the trap: the ssh process sits forever with no output and no error.
- **`git commit` / `git tag -a` hang on SSH commit signing** without a TTY (`ssh-keygen -Y sign` waits for a passphrase prompt that never comes). Use `git -c commit.gpgsign=false ...` / `git -c tag.gpgsign=false ...` for commits and tags made from the harness. Symptom: git sits silently; `ps` shows an `ssh-keygen -Y sign` child.
- **Pass long messages by file, never by `$(cat <<EOF...)` HEREDOC.** This applies to both `git tag -a` and `gh pr create --body` — the HEREDOC form has hung silently from the shell harness multiple times. Write the file with the `Write` tool, then pass it via `-F` / `--body-file`:

  ```
  git -c tag.gpgsign=false tag -a v0.16.8-databanx-1 -F /tmp/tag-msg.txt
  gh pr create --repo Databanx/stalwart --base main --head update/v0.16.8 \
      --title "Update to v0.16.8 with fork patches" --body-file /tmp/pr-body.md
  ```

  If you do hit the hang, `kill <pid>` clears it; verify with `git ls-remote --tags origin` / `gh pr list` before retrying — the resource may already have been created.

## Update flow (new upstream version)

This flow covers 1.x point releases (and, as history, 0.16.x). The 0.16 → 1.0 move was **not a bump**: new database layout, no in-place upgrade, data copied to a new store with `stalwart-migrate`, which reads only from 0.16.27 (https://stalw.art/docs/migration/upgrade-0-16/, read on 2026-10-26), registry schema renamed and renumbered, fork patches rewritten. That migration is documented in `localmail-server/docs/stalwart-v1/` (private repo). A 1.x point release is a normal bump again: same store, binary swap.

1. `git fetch upstream --tags` and compare with the fork's latest `vX.Y.Z-databanx-N` tag. Read the new `CHANGELOG.md` entries and check which ones touch our backends (FoundationDB, Redis, S3, the FDB search store, OIDC) — that is the case for bumping.
2. `git checkout -b update/vX.Y.Z vX.Y.Z` (the upstream tag).
3. Cherry-pick the fork commits from the previous release, oldest first. List them in two parts: the bump branch itself (`git log --reverse --no-merges --oneline vPREV..vPREV-databanx-1`), then whatever landed on `main` after it (`git log --reverse --no-merges --ancestry-path --oneline vPREV-databanx-1..origin/main`). Do **not** use `upstream..origin/main`, which also lists every stale copy from older bumps (and, since the 1.0 merge, every 0.16 patch), and do not use `--first-parent` from a later `-databanx-N` tag: when that release was cut from a branch off `main` (v0.16.26-databanx-2), first-parent walks `main`'s own history and lists stale copies too. As of v1.0.0-databanx-1 they are:
   - `c2816c4ca ci: remove os workflows do upstream na v1.0.0` — conflicts whenever upstream touches `.github/`. Resolution: `git rm` every upstream workflow (1.0 has no `tests/`, so `test.yml` goes too) and keep the fork's `dependabot.yml` (cargo only). `c2816c4ca` does **not** remove `.github/allowed-pr-authors.txt`: run `git rm .github/allowed-pr-authors.txt` as an explicit step of the bump (it came back with v1.0.0 and only left the branch in the merge with `main`).
   - `412651b74 docs: traz a skill build-stalwart e docs/databanx do main para a v1.0.0` + `895fbd1a3 docs: traz o .github/README.md citado pelo dependabot do fork`, plus the docs-only commits made on `main` after the previous tag, so the branch carries the current skill.
   - `db5e184e1 fix: count OIDC password attempts towards fail2ban` (rewritten for 1.0: `Basic` against an external OIDC directory now emits `AuthEvent::Failed` with `AccountName`; two unit tests in the same crate).
   - `5a4380c25`, `908569c38`, `beca3bc8e`, `6abe07813` — SearchSnippet/get fetches blobs concurrently (`crates/jmap/src/email/snippet.rs`): up to `SNIPPET_BLOB_CONCURRENCY` = 8 in flight, sized per batch as `SNIPPET_PARALLEL_MAX_BYTES` (16 MiB) / largest message, clamped to 1–8, because 1.0 reserves read memory per download. **Constants, no registry property**, so there is no discriminant to collide anymore.

   Use `git -c commit.gpgsign=false cherry-pick ...`. Not ported to 1.0, don't bring them back: the `receivedAt` sort fixes (PR #25; 1.0 sorts from the message cache), the `SnippetConcurrency` property and its discriminant moves, `fix: correct total count with collapseThreads` (upstream fixed it in v0.16.12) and the rust-s3 pin (upstream moved to rust-s3 0.38.0 in v0.16.26).
4. **0.16 only (history): validate the `Property` enum.** On 0.16 the SearchSnippet patch added `Property::SnippetConcurrency` with a hardcoded discriminant, and upstream variants in the same range auto-merged into **two variants with the same number** without any conflict marker. The 1.x patch has no property, so skip this step on 1.x; on 1.x, check instead that `SNIPPET_BLOB_CONCURRENCY` / `SNIPPET_PARALLEL_MAX_BYTES` survived the cherry-pick and that upstream did not change the per-download memory reservation they are sized against. The 0.16 check was:

   ```
   python3 -c "
   import re; src=open('crates/registry/src/schema/properties.rs').read()
   body=re.search(r'pub enum Property \{(.*?)\n\}', src, re.S).group(1)
   v=[int(x) for x in re.findall(r'^\s+\w+ = (\d+),', body, re.M)]
   print(len(v), min(v), max(v), 'dup:', [x for x in set(v) if v.count(x)>1])"
   grep -n 'const COUNT' crates/registry/src/schema/properties_impl.rs
   ```

   Expected: `len == max+1`, `min == 0`, `dup: []`, and the `Property` `const COUNT` equal to `len`. On a collision, move `SnippetConcurrency` to the first free number and fix the reverse-parse arm (`NNN => Some(Property::SnippetConcurrency)` in `properties_impl.rs`) and `const COUNT`. In `structs_impl.rs`, keep `snippet_concurrency` **last** in pickle/unpickle and adjust `Map::with_capacity` (upstream value + 1). It has collided on most bumps (v0.16.26: upstream `ServerId = 934` → moved to 935).
5. Validate: `cargo check -p registry -p jmap -p directory` (native macOS is fine for this). The unused-import warnings in `common`/`jmap`/`services` come from feature flags upstream — not ours.
6. Push the branch, build in Docker, verify backends, tag.
7. **Review gate — mandatory before opening the PR.** Launch a review agent (see "Review gate" below) and fix what it finds. Only then open the PR to `main` (merge commit, like the previous `update/*` PRs), with the review verdict in the PR body.
8. **Review gate again before swapping production** if anything changed after the first review (merge of `origin/main` into the branch, conflict resolution, rebuilt binary). Do not touch `/opt/stalwart-v1/bin/stalwart` without an approved review of the exact tree and binary being deployed.

## Review gate (required before the PR and before production)

Decided 2026-10-15: every bump gets an **independent review agent** — the session that did the bump does not review its own work. Run it twice:

- **Before `gh pr create`** — reviews the branch as built.
- **Before the in-place swap** — confirms the tree being deployed is the reviewed one (re-review whatever changed since: merges from `origin/main`, conflict resolutions, a rebuild).

Launch it with the `Agent` tool (`subagent_type: "pr-review-toolkit:code-reviewer"`, or `general-purpose` if that type is missing) and give it the concrete inputs, not a vague "review the PR":

- **Fork diff vs upstream:** `git diff vX.Y.Z update/vX.Y.Z -- crates Cargo.toml Cargo.lock resources .cargo` — every hunk must belong to a known fork patch (list in step 3) or to a commit made in this bump; anything else is a mistake from a cherry-pick or conflict resolution.
- **Commits made in this bump** (discriminant moves, conflict resolutions, retired patches), with the reason for each.
- **SearchSnippet patch (1.x):** the two constants and the per-batch concurrency formula are intact, and upstream did not change the per-download memory reservation in the same code path. (0.16 only: the `Property` enum check from step 4.)
- **Conflict resolutions:** for every file resolved with `--ours`/`--theirs`, confirm nothing upstream wanted was dropped (e.g. new upstream `[patch.crates-io]` entries, new dependencies in `Cargo.lock`).
- **Retired / kept patches:** evidence that a retired patch is really covered upstream (released crate version, test or changelog entry), and that kept patches still apply to the code they patch (e.g. upstream changed the same function).
- **Build provenance:** `git diff <tag> HEAD -- crates Cargo.toml Cargo.lock resources .cargo` must be empty (otherwise the binary is not the tagged tree; it embeds `resources/` through `include_bytes!`), binary sha256 = tag message = what was copied to the host, backend `strings` check results.
- **Changelog risk for our setup:** upstream changes that touch FoundationDB, Redis, S3/R2, the search store (FDB since 1.0), OIDC, the registry schema or the pickle format, and whether any need config or migration on deploy.

The agent returns a verdict (**approve** / **changes needed**) with findings. Act on every blocking finding, rerun the gate if code changed, and record the verdict (date, what was reviewed, sha256 reviewed) in the PR body or a PR comment. A pending or failed review blocks both the PR and the deploy — "the diff is small" is not an exemption.

## Tag and release convention

After a successful build that you intend to deploy, tag it on the **fork repo** (`Databanx/stalwart`) so we can map a binary to a specific source tree.

Format: `vX.Y.Z-databanx-N` — annotated tag on top of the upstream `vX.Y.Z` base, where `N` increments for each fork-specific build.

- Examples: `v0.16.5-databanx-1`, `v0.16.8-databanx-1`.
- Do **not** bump version strings in `Cargo.toml`. Crates stay pinned to the upstream version they branched from.
- The tag points at the commit whose tree produced the deployed binary. Docs-only commits after the tag (skill updates, a merge of `origin/main` resolved in favour of the branch) are fine as long as `git diff <tag> HEAD -- crates Cargo.toml Cargo.lock resources .cargo` stays empty; any code change after the tag needs a rebuild and a new `-databanx-N` tag.
- The annotated tag message should list the fork-specific commits (short hash + subject) and the binary sha256, so provenance is obvious from `git show <tag>` alone.
- Do not push `update/<version>` directly to `main` — open a PR for review.

## Distributing the binary to mx1

mx1 gets the binary from the workstation (it builds nothing):

```
scp -o IdentityAgent=none /tmp/stalwart-out/stalwart mx1:/tmp/stalwart.new
ssh -o IdentityAgent=none mx1 'sha256sum /tmp/stalwart.new'
sha256sum /tmp/stalwart-out/stalwart   # both sums must match (and the tag message)
```

## In-place swap on mx1 (1.0 unit)

With a single host, the swap takes e-mail down (SMTP/IMAP/JMAP) for the seconds between `stop` and the listeners coming back. Warn before doing it in business hours. Keep `/etc/stalwart-v1/VIRADA-AUTORIZADA` in place: without it `stalwart-v1` won't start (`ConditionPathExists`) and the 0.16 drop-in stops blocking `stalwart`.

```
# Run on mx1.
TS=$(date +%Y%m%d-%H%M%S)
PRIOR_TAG=v1.0.0-databanx-1   # whatever version you're replacing
BIN=/opt/stalwart-v1/bin/stalwart

sudo cp -p $BIN $BIN.bak-${PRIOR_TAG}-${TS}

sudo systemctl stop stalwart-v1
sudo mv /tmp/stalwart.new $BIN
sudo chown stalwart:stalwart $BIN
sudo chmod 755 $BIN
sudo systemctl start stalwart-v1

sleep 4
sudo systemctl is-active stalwart-v1         # expect: active
sudo ss -tlnp | grep -c '"stalwart"'          # 7 listeners (match by process name, not port pattern)
$BIN --version
timeout 5 bash -c 'exec 3<>/dev/tcp/127.0.0.1/25; head -1 <&3'      # 220 ... Stalwart ESMTP
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/healthz/live   # 200
curl -s -o /dev/null -w '%{http_code}\n' https://server.localmail.com.br/jmap/session   # 200 = Caddy and Stalwart up
```

If `start` does nothing, check `systemctl status stalwart-v1`: a failed `ExecStartPre` guard means the 0.16 unit is active; an unmet condition means the marker is missing.

**Server logs are NOT in the journal.** Stalwart (1.0 too) writes its own log to `/opt/stalwart/logs/stalwart.log.YYYY-MM-DD` (UTC, rotated daily); the journal (`journalctl -u stalwart-v1`) only has systemd's start/stop lines. A `journalctl | grep -E 'ERROR|WARN'` returning nothing proves nothing (this gave a false "clean boot" on the v0.16.23 rollout). Check the file:

```
F=/opt/stalwart/logs/stalwart.log.$(date -u +%F)
B=$(grep -n 'server.startup' $F | tail -1 | cut -d: -f1)       # line of the new boot
sed -n "${B},\$p" $F | grep -E ' (WARN|ERROR) ' | grep -oE '\([a-z0-9-]+\.[a-z0-9.-]+\)' | sort | uniq -c
```

Expected on the 1.0 boot (v1.0.0-databanx-1, 2026-10-27): `server.startup` with the new version, `server.licensing`, `cluster.startup` + `cluster.publisher-start` + `cluster.subscriber-start`, `queue.started`, 7 `network.listen-start`, and a single `registry.build-warning` (DnsResolver cannot validate DNSSEC, DANE disabled — pre-existing, not a regression). A burst of `store.blob-missing-marker` WARNs is pre-0.16 legacy blobs, not a regression (Databanx/stalwart#18). Steady-state noise that is not a regression: `smtp.invalid-ehlo` bursts from scanners and `security.ip-blocked`. `auth.failed` is Debug-level upstream, and production raises it to **Info** with an `x:EventTracingLevel` override (since the 2026-10-27 cutover), so failed logins show up in the log (~15/h of scanner brute force on 2026-10-27). `auth.error` is already Error-level upstream (`crates/trc/src/event/enums_impl.rs`) and runs at its default; after a swap, look for `(auth.error)` together with the WARN/ERROR grep. Compare the WARN/ERROR event counts against the same window before the swap rather than expecting zero.

A `resource.error` for the web UI on boot means the `x:Application` `resourceUrl` is wrong: the 05:57 cutover boot logged it because `releases/latest` of `stalwartlabs/webui` had no 1.0 manifest. Production points it at `https://github.com/stalwartlabs/webui/releases/download/v2/webui.zip` (then the `UpdateApps` action); if the error comes back, check that URL first.

The freshly scp'd binary in `/tmp/stalwart.new` arrives owned by `ubuntu:ubuntu`. The `chown` keeps the binary's owner consistent with the current install (`stalwart:stalwart`); it does not affect what the process can read or write, since the unit runs it as `User=stalwart` whoever owns the file. For future installs, `root:root 755` is the hardening recommendation (the service user then cannot overwrite its own binary); production has not been changed to it.

**Port bind can lag the `active` state by a few seconds** (observed on mx1 with 0.16: `active` with 0 listeners at +4 s, all listening at ~+15 s). If the listener count is 0 right after start, re-check before assuming failure — a real crash shows in the journal as a failed/restarting unit (`journalctl -u stalwart-v1 -n 20`, `systemctl show stalwart-v1 -p NRestarts`) and its cause in the log file above.

### Rollback

```
BIN=/opt/stalwart-v1/bin/stalwart
sudo systemctl stop stalwart-v1
sudo cp -p $BIN.bak-<prior-tag>-<ts> $BIN
sudo chown stalwart:stalwart $BIN
sudo systemctl start stalwart-v1
```

This rolls back a 1.x binary only. Going back to 0.16 is a data migration (`stalwart-migrate rollback`), not a binary swap — see `localmail-server/docs/stalwart-v1/`.

Backups accumulate — periodically prune old ones (`ls /opt/stalwart-v1/bin/stalwart.bak-*`) once you're confident in a release.

## Local validation on the workstation (not a release path)

For fast iteration on a patch before pushing:

```
cargo check -p jmap          # or whichever single crate the patch touches
cargo test -p <crate>        # unit tests live in the crates; 1.0 has no tests/ workspace crate
```

With the production feature set (no `rocks`), workspace checks avoid `librocksdb-sys` entirely. Per-crate `cargo check -p common` may surface unrelated `mail_auth::dkim::generate` import errors due to feature flags — these go away with the production feature set and are not regressions.

## Reporting fork fixes upstream

Upstream (`stalwartlabs/stalwart`) does not take issues or PRs from us directly:

- GitHub issues are **auto-closed** (`blank_issues_enabled: false`; the only template is "auto-closed"). Bugs go to **support.stalw.art** (GitHub login); a maintainer converts confirmed ones into issues.
- PRs are limited to **vouched contributors** (ask at support.stalw.art first), need the Fiduciary Contributor License Agreement, and the `CONTRIBUTING.md` bans AI-generated code.

So for a fork fix: write the bug report (symptom, cause with file paths, suggested fix in prose) and have the user post it on support.stalw.art under their own account — never post on their behalf. Keep the patch in the fork until an upstream release carries an equivalent fix, and re-check at every bump.

## What NOT to do

- Do not build on mx1 — no toolchain (it lost its build env in the 2026-06 downsize and is the only production host).
- Do not produce a release binary natively on macOS — it must be a Linux aarch64 ELF linked against the FDB client.
- Do not run `cargo build` without `--no-default-features` + the production feature list. The upstream default set (`rocks`, `enterprise`) boots and dies because the data store backend is wrong.
- Do not pad the feature list with backends prod doesn't use ("just in case").
- Do not `docker volume rm stalwart-target` / `stalwart-cargo-registry` casually — you'll pay the cold-build tax.
- Do not push `update/<version>` directly to `main`. Use a PR.
- Do not bump `version` fields in `Cargo.toml`. Use the `vX.Y.Z-databanx-N` git tag instead.
- Do not leave the new binary in `/opt/stalwart-v1/bin/` owned by `ubuntu`: `chown` it to match the install (`stalwart:stalwart` today; `root:root` is the hardening target for future installs).
- Do not swap the binary of the stopped 0.16 unit (`/opt/stalwart/bin/stalwart`) thinking it is production — production is `stalwart-v1`.
- Do not remove `/etc/stalwart-v1/VIRADA-AUTORIZADA` or the `no-v1.conf` drop-in: they keep 0.16 and 1.0 from running at the same time.
- Do not open the PR or swap the production binary without an approved review from an independent review agent (see "Review gate").
- Do not trust `journalctl` for server errors — Stalwart logs to `/opt/stalwart/logs/stalwart.log.YYYY-MM-DD`.
- Do not open issues or PRs on `stalwartlabs/stalwart` from the harness — see "Reporting fork fixes upstream".
