#!/usr/bin/env bash
# Resolve the release/tag name for the current ref and write it to the GitHub
# Actions step outputs.
#
#   - tag push  (refs/tags/vX.Y.Z) -> name = the tag, tagged release.
#   - branch push (any branch)     -> name = "<branch>-<sha7>" (slashes -> "-"),
#                                     a pre-release tagged with that same name.
#
# Outputs: `name` (release title + asset prefix), `tag` (git tag to create),
# `is_tag` ("true" for a vX.Y.Z tag, else "false").
set -e

if [[ "$GITHUB_REF" == refs/tags/* ]]; then
    NAME="${GITHUB_REF_NAME}"
    IS_TAG=true
else
    SAFE_BRANCH="${GITHUB_REF_NAME//\//-}"
    NAME="${SAFE_BRANCH}-${GITHUB_SHA::7}"
    IS_TAG=false
fi

echo "name=${NAME}" >>"$GITHUB_OUTPUT"
echo "tag=${NAME}" >>"$GITHUB_OUTPUT"
echo "is_tag=${IS_TAG}" >>"$GITHUB_OUTPUT"
