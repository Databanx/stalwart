---
name: build-stalwart
description: Use when building, releasing, or testing the Databanx fork of stalwart for production. Covers the production feature set, the local Docker ARM64 build environment, the binary distribution to mx1/mx2/mx3, the in-place swap procedure, and the `vX.Y.Z-databanx-N` tag convention. Trigger phrases include "build the fork", "compile stalwart", "make a databanx tag", "deploy the patches", "ship to the mx servers", "test the binary on the server".
version: 2.1.0
---

# Building the Databanx fork of stalwart

The build is **slow and expensive to redo** — get the feature set right the first time. A cold build in the local Docker builder takes ~13 min (18 cores); the LTO fat + codegen-units=1 link step alone is several minutes. A version bump touches every workspace crate, so even with the cache warm expect ~15 min (v0.16.23: 14m46s).

## Hosts

- **Dev workstation** (Apple Silicon Mac, OrbStack) — **the build host**. Release binaries are produced in a local Linux ARM64 container (see "Docker build environment"). Native macOS builds are NOT release artifacts; macOS `cargo check -p <crate>` is fine for fast patch validation.
- **`mx1` / `mx2` / `mx3`** — production peers only. **None of them has a toolchain.** Always copy the built binary in, never try to build there.

> **History (2026-06-11):** mx1 used to be the build host (`~/stalwart-build`). The Oracle downsize to 2 vCPU / 8 GB wiped `~/stalwart-build` and `~/.cargo`/`~/.rustup`. The FDB client libs (7.4.6) are still installed on mx1, but do not rebuild the toolchain there — the build now lives in Docker on the workstation, which is faster and doesn't compete with production for CPU.

All three production hosts run the same systemd unit: binary at `/opt/stalwart/bin/stalwart`, config at `/opt/stalwart/etc/config.toml`, owned by `stalwart:stalwart`, listening on 25 / 993 / 8080.

## Production feature set

The upstream `default = ["rocks", "enterprise"]` in `crates/main/Cargo.toml` is **wrong for this deployment** — RocksDB is not used. Production (verified against `/opt/stalwart/etc/config.toml` on `mx2`/`mx3`) uses **only**:

| Component | Backend | Feature |
|---|---|---|
| Data store + lookup + in-memory + `directory.internal` | FoundationDB | `foundationdb` |
| Blob store | R2 (S3-compatible) | `s3` |
| Coordinator + PubSub + in-memory | Redis | `redis` |
| FTS | Meilisearch | (always-on, no flag) |
| Enterprise features | — | `enterprise` |

So the build is:

```
cargo build --release \
    --no-default-features \
    --features "foundationdb s3 redis enterprise"
```

(`JEMALLOC_SYS_WITH_LG_PAGE=16` must be in the environment — the builder image sets it; see below.)

**Reference size**: the stripped aarch64 binary grows slowly with upstream (v0.16.8: ~63 MB; v0.16.20: 69.6 MB; v0.16.22: 70.1 MB; v0.16.23: 70.3 MB). Compare against the previous release's size in its tag message, not against a fixed number. A jump of several MB from one release to the next means someone re-added a backend — check the rustc invocation for `--cfg feature="rocks"` etc. The backend `strings` check below is the real test.

**Do not add `rocks`, `sqlite`, `postgres`, `mysql`, `nats`, `azure`, `zenoh`, or `kafka`** unless you have first verified the production config actually references that backend. Each one drags in a heavy dependency that bloats the binary and lengthens the build with zero runtime benefit. If a future config change adds a backend, update the feature list *and* this table — don't preemptively pad it.

**Symptom of a missing feature**: the server boots and immediately exits with `Startup failed: Binary was not compiled with the selected data store backend` (or analogous for blob/coordinator). If you see this, the binary is wrong — the config is fine.

## Docker build environment

Image `stalwart-builder-arm64`: Ubuntu 24.04 (same glibc as the mx hosts) + FDB client 7.4.6 (same as the cluster) + rustup stable + `JEMALLOC_SYS_WITH_LG_PAGE=16` baked in (the Oracle ARM64 kernels use 16 KiB pages; without it jemalloc panics at **runtime**, not build time).

Dockerfile (recreate at `/tmp/stalwart-builder/Dockerfile` if the image is gone):

```dockerfile
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential clang libclang-dev pkg-config libssl-dev \
    curl ca-certificates git adduser \
    && rm -rf /var/lib/apt/lists/*

# FoundationDB client 7.4.6 (same as mx1/mx2/mx3).
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

From the repo checkout (branch `update/<version>` with the fork patches applied):

```
docker run --rm --name stalwart-build \
  -v /Users/eldon/code/databanx/localmail/stalwart:/src \
  -v stalwart-cargo-registry:/root/.cargo/registry \
  -v stalwart-target:/target \
  -e CARGO_TARGET_DIR=/target \
  stalwart-builder-arm64 \
  cargo build --release --no-default-features --features "foundationdb s3 redis enterprise"
```

The named volumes (`stalwart-cargo-registry`, `stalwart-target`) keep the cargo cache off the slow macOS bind mount and survive OrbStack restarts. Build time: ~13 min cold, a few minutes warm. Do not `docker volume rm` them casually.

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
for m in "FoundationDB" "x-amz-content-sha256" "redis-rs" "Meilisearch"; do
    echo -n "$m: "; strings $BIN | grep -c "$m"
done

# should all be zero
for m in librocksdb tokio_postgres mysql_async rusqlite async_nats; do
    echo -n "$m: "; strings $BIN | grep -c "$m"
done
```

## Workstation shell gotchas (read this first)

- **SSH to the mx hosts can hang on a dead agent socket** in non-interactive harness shells (`ssh-add -l` itself hangs). Use `ssh -o IdentityAgent=none mx1 ...` — the `~/.ssh/databanx` key has no passphrase, so bypassing the agent always works. Symptom of the trap: the ssh process sits forever with no output and no error.
- **`git commit` / `git tag -a` hang on SSH commit signing** without a TTY (`ssh-keygen -Y sign` waits for a passphrase prompt that never comes). Use `git -c commit.gpgsign=false ...` / `git -c tag.gpgsign=false ...` for commits and tags made from the harness. Symptom: git sits silently; `ps` shows an `ssh-keygen -Y sign` child.
- **Pass long messages by file, never by `$(cat <<EOF...)` HEREDOC.** This applies to both `git tag -a` and `gh pr create --body` — the HEREDOC form has hung silently from the shell harness multiple times. Write the file with the `Write` tool, then pass it via `-F` / `--body-file`:

  ```
  git -c tag.gpgsign=false tag -a v0.16.8-databanx-1 -F /tmp/tag-msg.txt
  gh pr create --repo Databanx/stalwart --base main --head update/v0.16.8 \
      --title "Update to v0.16.8 with fork patches" --body-file /tmp/pr-body.md
  ```

  If you do hit the hang, `kill <pid>` clears it; verify with `git ls-remote --tags origin` / `gh pr list` before retrying — the resource may already have been created.

## Update flow (new upstream version)

1. `git fetch upstream --tags` and compare with the fork's latest `vX.Y.Z-databanx-N` tag. Read the new `CHANGELOG.md` entries and check which ones touch our backends (FoundationDB, Redis, S3, Meilisearch, OIDC) — that is the case for bumping.
2. `git checkout -b update/vX.Y.Z vX.Y.Z` (the upstream tag).
3. Cherry-pick the fork commits from the previous release, oldest first. List them from the previous tag (`git log --reverse --no-merges --first-parent --oneline vPREV..vPREV-databanx-N`); do **not** use `upstream..origin/main`, which also lists every stale copy from older bumps. As of v0.16.23-databanx-1 they are:
   - `perf: parallelize SearchSnippet/get blob fetches` (adds the `snippetConcurrency` setting)
   - `fix: patch rust-s3 to unblock blob purge DELETEs on Cloudflare R2` (pin to `Databanx/rust-s3`; drop it once crates.io ships the durch/rust-s3#466 fix and upstream bumps)
   - `ci: remove os workflows de release do upstream deste fork` + `ci: remove o ci-retry.yml do upstream`
   - `docs: track .claude skills; …` (this skill)
   - `fix: count OIDC password attempts towards fail2ban` (`crates/directory/src/backend/oidc/lookup.rs`)

   Use `git -c commit.gpgsign=false cherry-pick ...`. `fix: correct total count with collapseThreads` is retired (upstream fixed it in v0.16.12) — don't port it.
4. **Validate the `Property` enum even if the cherry-pick applied cleanly.** The SearchSnippet patch adds `Property::SnippetConcurrency` with a hardcoded discriminant; when upstream adds variants in the same range, git auto-merges into **two variants with the same number** without any conflict marker (it only flags `const COUNT`, if anything):

   ```
   python3 -c "
   import re; src=open('crates/registry/src/schema/properties.rs').read()
   body=re.search(r'pub enum Property \{(.*?)\n\}', src, re.S).group(1)
   v=[int(x) for x in re.findall(r'^\s+\w+ = (\d+),', body, re.M)]
   print(len(v), min(v), max(v), 'dup:', [x for x in set(v) if v.count(x)>1])"
   grep -n 'const COUNT' crates/registry/src/schema/properties_impl.rs
   ```

   Expected: `len == max+1`, `min == 0`, `dup: []`, and the `Property` `const COUNT` equal to `len`. On a collision, move `SnippetConcurrency` to the first free number and fix the reverse-parse arm (`NNN => Some(Property::SnippetConcurrency)` in `properties_impl.rs`) and `const COUNT`. In `structs_impl.rs`, keep `snippet_concurrency` **last** in pickle/unpickle and adjust `Map::with_capacity`.
5. Validate: `cargo check -p registry -p jmap -p directory` (native macOS is fine for this). The unused-import warnings in `common`/`jmap`/`services` come from feature flags upstream — not ours.
6. Push the branch, build in Docker, verify backends, tag, open the PR to `main` (merge commit, like the previous `update/*` PRs).

## Tag and release convention

After a successful build that you intend to deploy, tag it on the **fork repo** (`Databanx/stalwart`) so we can map a binary to a specific source tree.

Format: `vX.Y.Z-databanx-N` — annotated tag on top of the upstream `vX.Y.Z` base, where `N` increments for each fork-specific build.

- Examples: `v0.16.5-databanx-1`, `v0.16.8-databanx-1`.
- Do **not** bump version strings in `Cargo.toml`. Crates stay pinned to the upstream version they branched from.
- The tag points at the HEAD commit of `update/<version>` that's actually being deployed.
- The annotated tag message should list the fork-specific commits (short hash + subject) and the binary sha256, so provenance is obvious from `git show <tag>` alone.
- Do not push `update/<version>` directly to `main` — open a PR for review.

## Distributing the binary to mx1 / mx2 / mx3

All three hosts get the binary from the workstation (mx1 no longer builds anything):

```
for h in mx1 mx2 mx3; do
    scp -o IdentityAgent=none /tmp/stalwart-out/stalwart $h:/tmp/stalwart.new
    ssh -o IdentityAgent=none $h 'sha256sum /tmp/stalwart.new'
done
sha256sum /tmp/stalwart-out/stalwart   # all four sums must match
```

## In-place swap on a peer

Per-host downtime is ~1 second between `stop` and `start`. Do them **sequentially** (mx2 → mx3 → mx1, or any order), one host at a time, so a failed host can be rolled back without all three being down.

```
# Run on the target host.
TS=$(date +%Y%m%d-%H%M%S)
PRIOR_TAG=v0.16.6-databanx-1   # whatever version you're replacing

sudo cp -p /opt/stalwart/bin/stalwart \
           /opt/stalwart/bin/stalwart.bak-${PRIOR_TAG}-${TS}

sudo systemctl stop stalwart
sudo mv /tmp/stalwart.new /opt/stalwart/bin/stalwart
sudo chown stalwart:stalwart /opt/stalwart/bin/stalwart
sudo chmod 755 /opt/stalwart/bin/stalwart
sudo systemctl start stalwart

sleep 4
sudo systemctl is-active stalwart            # expect: active
sudo ss -tlnp | grep -E ":(25|993|8080)"     # all three should be listening
/opt/stalwart/bin/stalwart --version
timeout 5 bash -c 'exec 3<>/dev/tcp/127.0.0.1/25; head -1 <&3'      # 220 ... Stalwart ESMTP
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/healthz/live   # 200
```

**Server logs are NOT in the journal.** Stalwart writes its own log to `/opt/stalwart/logs/stalwart.log.YYYY-MM-DD` (UTC, rotated daily); the journal only has systemd's start/stop lines. A `journalctl | grep -E 'ERROR|WARN'` returning nothing proves nothing (this gave a false "clean boot" on the v0.16.23 rollout). Check the file:

```
F=/opt/stalwart/logs/stalwart.log.$(date -u +%F)
B=$(grep -n 'server.startup' $F | tail -1 | cut -d: -f1)       # line of the new boot
sed -n "${B},\$p" $F | grep -E ' (WARN|ERROR) ' | grep -oE '\([a-z0-9-]+\.[a-z0-9.-]+\)' | sort | uniq -c
```

Expected on boot: `server.startup` with the new version, `cluster.startup`, `queue.started`, `network.listen-start` per listener, and a single `registry.build-warning` (DnsResolver cannot validate DNSSEC, DANE disabled — pre-existing, not a regression). Steady-state noise that is not a regression: `smtp.invalid-ehlo` bursts from scanners and `security.ip-blocked`. Note that `auth.failed` is a **Debug**-level event and never reaches the Info log — failed logins are invisible there; to see them, read the fail2ban counters in Redis (keys `0x05` + login/IP bytes + 8-byte day bucket, limit `authBanRate` = 100/day) or look for `security.authentication-ban` (Info). Compare the WARN/ERROR event counts against the same window before the swap rather than expecting zero.

The freshly scp'd binary in `/tmp/stalwart.new` arrives owned by `ubuntu:ubuntu` — the `chown stalwart:stalwart` is mandatory or systemd will start the process but it won't be able to read its config / write logs. **Don't skip it.**

**Port bind can lag the `active` state by a few seconds** (observed on mx1: `active` with 0 listeners at +4 s, all 3 listening at ~+15 s). If the listener count is 0 right after start, re-check before assuming failure — a real crash shows in the journal as a failed/restarting unit (`journalctl -u stalwart -n 20`, `systemctl show stalwart -p NRestarts`) and its cause in the log file above.

### Rollback

```
sudo systemctl stop stalwart
sudo cp -p /opt/stalwart/bin/stalwart.bak-<prior-tag>-<ts> /opt/stalwart/bin/stalwart
sudo chown stalwart:stalwart /opt/stalwart/bin/stalwart
sudo systemctl start stalwart
```

Backups accumulate — periodically prune old ones (`ls /opt/stalwart/bin/stalwart.bak-*`) once you're confident in a release.

## Local validation on the workstation (not a release path)

For fast iteration on a patch before pushing:

```
cargo check -p jmap          # or whichever single crate the patch touches
cargo check -p tests --tests # for test-only changes
```

With the production feature set (no `rocks`), workspace checks avoid `librocksdb-sys` entirely. Per-crate `cargo check -p common` may surface unrelated `mail_auth::dkim::generate` import errors due to feature flags — these go away with the production feature set and are not regressions.

## Reporting fork fixes upstream

Upstream (`stalwartlabs/stalwart`) does not take issues or PRs from us directly:

- GitHub issues are **auto-closed** (`blank_issues_enabled: false`; the only template is "auto-closed"). Bugs go to **support.stalw.art** (GitHub login); a maintainer converts confirmed ones into issues.
- PRs are limited to **vouched contributors** (ask at support.stalw.art first), need the Fiduciary Contributor License Agreement, and the `CONTRIBUTING.md` bans AI-generated code.

So for a fork fix: write the bug report (symptom, cause with file paths, suggested fix in prose) and have the user post it on support.stalw.art under their own account — never post on their behalf. Keep the patch in the fork until an upstream release carries an equivalent fix, and re-check at every bump.

## What NOT to do

- Do not build on mx1, mx2 or mx3 — no toolchain on any of them (mx1 lost its build env in the 2026-06 downsize; it has 2 vCPU / 8 GB and is a production peer).
- Do not produce a release binary natively on macOS — it must be a Linux aarch64 ELF linked against the FDB client.
- Do not run `cargo build` without `--no-default-features` + the production feature list. The upstream default set (`rocks`, `enterprise`) boots and dies because the data store backend is wrong.
- Do not pad the feature list with backends prod doesn't use ("just in case").
- Do not `docker volume rm stalwart-target` / `stalwart-cargo-registry` casually — you'll pay the cold-build tax.
- Do not push `update/<version>` directly to `main`. Use a PR.
- Do not bump `version` fields in `Cargo.toml`. Use the `vX.Y.Z-databanx-N` git tag instead.
- Do not skip `chown stalwart:stalwart` after dropping the new binary into `/opt/stalwart/bin/`.
- Do not swap all three peers in parallel. One at a time, verify, then move on.
- Do not trust `journalctl` for server errors — Stalwart logs to `/opt/stalwart/logs/stalwart.log.YYYY-MM-DD`.
- Do not open issues or PRs on `stalwartlabs/stalwart` from the harness — see "Reporting fork fixes upstream".
