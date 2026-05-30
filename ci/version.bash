#!/usr/bin/env bash
# Resolve the artifact version name and whether this is a tagged release.
# Writes `name` and `release` to the GitHub Actions step outputs.
set -e

if [[ "$GITHUB_REF" == refs/tags/* ]]; then
    echo "name=${GITHUB_REF#refs/tags/}" >>"$GITHUB_OUTPUT"
    echo "release=true" >>"$GITHUB_OUTPUT"
else
    echo "name=${GITHUB_REF_NAME}-${GITHUB_SHA::7}" >>"$GITHUB_OUTPUT"
    echo "release=false" >>"$GITHUB_OUTPUT"
fi
