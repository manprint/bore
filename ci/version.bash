#!/usr/bin/env bash
# Resolve the release/tag name for the current ref and write it to the GitHub
# Actions step outputs.
#
#   - tag push  (refs/tags/vX.Y.Z) -> name = the tag, tagged release.
#   - branch push (any branch)     -> name = "<branch>-<sha7>" (slashes -> "-"),
#                                     a pre-release tagged with that same name.
#
# WHETHER A RELEASE IS A PRERELEASE IS A PROPERTY OF THE TAG, not a switch a
# workflow sets. A semver tag carrying a prerelease identifier (`v1.2.0-rc.1`)
# is something the world may install ON PURPOSE, so it gets a real release with
# assets -- but it must never be what `/releases/latest/download/...` hands out,
# which is the same failure `is_tag` was introduced to stop for branch builds.
# Deriving both answers from the tag means the two can never disagree, and no
# release can be marked stable by forgetting to pass a flag.
#
# Outputs: `name` (release title + asset prefix), `tag` (git tag to create),
# `is_tag` ("true" for a version tag, else "false"), `is_prerelease` and
# `is_latest` (exactly one release is "latest", and never a prerelease).
set -e

if [[ "$GITHUB_REF" == refs/tags/* ]]; then
    NAME="${GITHUB_REF_NAME}"
    IS_TAG=true
    # `vMAJOR.MINOR.PATCH-<identifier>`: everything after the first `-` is
    # semver's prerelease field. A stable version tag has no `-` at all, so the
    # test is exact and needs no version parsing.
    if [[ "$NAME" == *-* ]]; then
        IS_PRERELEASE=true
    else
        IS_PRERELEASE=false
    fi
else
    SAFE_BRANCH="${GITHUB_REF_NAME//\//-}"
    NAME="${SAFE_BRANCH}-${GITHUB_SHA::7}"
    IS_TAG=false
    IS_PRERELEASE=true
fi

if [[ "$IS_TAG" == true && "$IS_PRERELEASE" == false ]]; then
    IS_LATEST=true
else
    IS_LATEST=false
fi

echo "name=${NAME}" >>"$GITHUB_OUTPUT"
echo "tag=${NAME}" >>"$GITHUB_OUTPUT"
echo "is_tag=${IS_TAG}" >>"$GITHUB_OUTPUT"
echo "is_prerelease=${IS_PRERELEASE}" >>"$GITHUB_OUTPUT"
echo "is_latest=${IS_LATEST}" >>"$GITHUB_OUTPUT"
