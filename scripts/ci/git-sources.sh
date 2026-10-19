#!/usr/bin/env bash
# Let a pipeline run fetch a dependency on another repository from Azure.
#
#   SYSTEM_ACCESSTOKEN=... scripts/ci/git-sources.sh
#
# Every manifest names a dependency on another repository of The Test Cabinet
# by its public source, the GitHub mirror. A pipeline run holds no GitHub
# credential, and the mirrors are private while the project is; what the run
# does hold is the job's access token for Azure DevOps, where the same
# repositories live. So each public source is rewritten to the Azure
# repository over https, and the token is sent as the authorization header on
# every request to dev.azure.com. Cargo fetches through the git command line
# (`.cargo/config.toml`), which is what reads these settings.
#
# The pairs below are written by the repository kit from its one table of
# repositories, so `scripts/repos/render.py --update` in the superrepo is what
# changes them. Every repository is listed rather than the few this one may
# depend on, so a new edge needs no re-render to fetch.
set -euo pipefail

: "${SYSTEM_ACCESSTOKEN:?git-sources: SYSTEM_ACCESSTOKEN must carry the access token of the job}"

git config --global "http.https://dev.azure.com/.extraheader" \
	"AUTHORIZATION: bearer ${SYSTEM_ACCESSTOKEN}"

count=0
while read -r azure public; do
	[ -n "$azure" ] || continue
	git config --global "url.${azure}.insteadOf" "$public"
	count=$((count + 1))
done <<'SOURCES'
https://dev.azure.com/genyume/the-test-cabinet/_git/contracts https://github.com/TheClockwyrks/contracts
https://dev.azure.com/genyume/the-test-cabinet/_git/engines https://github.com/TheClockwyrks/engines
https://dev.azure.com/genyume/the-test-cabinet/_git/ui-common https://github.com/TheClockwyrks/ui-common
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-model-core https://github.com/TheClockwyrks/tool-model-core
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-sprite https://github.com/TheClockwyrks/tool-sprite
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-paint https://github.com/TheClockwyrks/tool-paint
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-voxel https://github.com/TheClockwyrks/tool-voxel
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-mc https://github.com/TheClockwyrks/tool-mc
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-sn https://github.com/TheClockwyrks/tool-sn
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-dc https://github.com/TheClockwyrks/tool-dc
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-particle https://github.com/TheClockwyrks/tool-particle
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-audio-core https://github.com/TheClockwyrks/tool-audio-core
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-sfx-synth https://github.com/TheClockwyrks/tool-sfx-synth
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-sfx-sample https://github.com/TheClockwyrks/tool-sfx-sample
https://dev.azure.com/genyume/the-test-cabinet/_git/tool-music https://github.com/TheClockwyrks/tool-music
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-backend https://github.com/TheClockwyrks/microservice-backend
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-dispatcher https://github.com/TheClockwyrks/microservice-dispatcher
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-driver https://github.com/TheClockwyrks/microservice-driver
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-artifacts https://github.com/TheClockwyrks/microservice-artifacts
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-repos https://github.com/TheClockwyrks/microservice-repos
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-publisher https://github.com/TheClockwyrks/microservice-publisher
https://dev.azure.com/genyume/the-test-cabinet/_git/microservice-arena https://github.com/TheClockwyrks/microservice-arena
https://dev.azure.com/genyume/the-test-cabinet/_git/gg https://github.com/TheClockwyrks/gg
https://dev.azure.com/genyume/the-test-cabinet/_git/the-spec-cabinet https://github.com/TheClockwyrks/the-spec-cabinet
https://dev.azure.com/genyume/the-test-cabinet/_git/client-tauri https://github.com/TheClockwyrks/client-tauri
SOURCES

echo "git-sources: ${count} public sources fetch from Azure"
