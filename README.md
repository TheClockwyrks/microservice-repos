# microservice-repos

The write-through store of run implementations as git bundles, and the public
push.

This repository is one component of
[The Test Cabinet](https://github.com/TheClockwyrks/TheTestCabinet). What it is
and how it fits with the others is stated by the documentation site, which
lives in the superrepo at `apps/docs/src/content/docs/`. Its Repositories page,
`apps/docs/src/content/docs/development/repositories.md`, draws the edges
between the repositories.

## The service

`crates/repos` is `tcab-repos`, an Axum server on `0.0.0.0:8792` over a private
Azure Blob container that holds one git bundle per run:

| Route | Caller | Credential |
| --- | --- | --- |
| `POST /runs/{id}/bundle` | The driver, uploading the run's bundle | The per-job token, with `x-tcab-job-id` |
| `GET /runs/{id}/bundle` | The backend, serving a download | The service token |
| `DELETE /runs/{id}/bundle` | The backend, pruning a deleted run | The service token |
| `POST /runs/{id}/publish` | The publisher Job | The per-publish-job token, with `x-tcab-publish-job-id` |

What the service must do is stated by the superrepo's
`apps/docs/src/content/docs/components/repos/overview.md`. Its configuration is
entirely environment variables, documented in `crates/repos/src/config.rs`.
To run it on its own, start Azurite's Blob service, copy `.env.repos.example`
to `.env.repos` and run:

```sh
npx azurite-blob --silent --location /tmp/azurite &
cargo run --bin tcab-repos
```

Against Azurite the service creates its container at startup. A publish needs
`TCAB_GITHUB_ORG` and `TCAB_GITHUB_TOKEN`, and the backend the run's record is
read from; without them it answers `503`.

## Getting started

Open the superrepo in its devcontainer, which carries every toolchain the
repositories need, and work from this repository's root.
A build here inherits the superrepo's Cargo configuration, which redirects
every dependency on another repository to the sibling checkout, so nothing is
fetched from a mirror.

One command runs every gate:

```sh
make gate
```

Each gate is a file under `ci/gates/` whose stem is the gate's id.
`uv run --quiet --project ci gate list` names them all, and
`azure-pipelines.yml` runs each one as a step of its own on every push, inside
the CI images the superrepo builds.

## The image

The `Dockerfile` at the root is the repository's image, built from the root
with `.dockerignore` as its allowlist. On every gated push to `master` and
`staging` the pipeline builds it natively for amd64 and arm64
(`scripts/ci/image.sh`) and fuses the two into the multi-arch
`testcabinet.azurecr.io/microservice-repos:<commit>` a deployment pins
(`scripts/ci/manifest.sh`). The `build-context` gate holds every `COPY` to the
allowlist and the Rust pin to `rust-toolchain.toml`. The kit wrote the
`Dockerfile` once, and it is the repository's to grow.
