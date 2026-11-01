#!/usr/bin/env bash
# The release workflow's first job: refuse a tag that is not a finished,
# CI-tested release on main.
#
#   scripts/release-guard.sh vX.Y.Z
#
# Run from a full checkout of the tag. Fails unless the tag is "v" plus
# Cargo.toml's version, CHANGELOG.md has a section for that version, and the
# commit is on origin/main. Then it waits for every ci.yml run on the commit to
# finish, and fails on any conclusion other than success, or if no CI run
# appears at all. The tag and its commit are usually pushed together, so CI may
# not have started yet.
#
# Needs jq and a gh that can read the repository's runs. The waits are in
# seconds: GUARD_POLL between checks (30), GUARD_FIRST_RUN for a CI run to
# appear (300) and GUARD_TIMEOUT for every run to finish (1800). The tests in
# tests/release_guard.rs shorten them and put a fake gh on PATH.
set -euo pipefail

TAG="${1:?usage: release-guard.sh vX.Y.Z}"
REPO="${GITHUB_REPOSITORY:-matthart1983/diskwatch}"
POLL="${GUARD_POLL:-30}"
FIRST_RUN="${GUARD_FIRST_RUN:-300}"
TIMEOUT="${GUARD_TIMEOUT:-1800}"

# `::error::` marks the line as the failure in the Actions log.
die() { echo "::error::$*"; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ "v${version}" = "${TAG}" ] || die "${TAG} is not the crate's version: Cargo.toml says ${version}"
awk -v h="## [${version}]" 'index($0, h) == 1 { found = 1 } END { exit !found }' CHANGELOG.md ||
    die "CHANGELOG.md has no '## [${version}]' heading"

sha=$(git rev-parse HEAD)
git merge-base --is-ancestor "${sha}" origin/main ||
    die "${sha} is not on origin/main. Merge it first, then tag the merge."

start=$(date +%s)
while :; do
    runs=$(gh run list -R "${REPO}" --workflow ci.yml --commit "${sha}" \
        --json databaseId,conclusion,status,event,url)
    total=$(jq length <<< "${runs}")
    running=$(jq '[.[] | select(.status != "completed")] | length' <<< "${runs}")
    failed=$(jq '[.[] | select(.status == "completed" and .conclusion != "success")] | length' <<< "${runs}")
    elapsed=$(( $(date +%s) - start ))
    if [ "${failed}" -gt 0 ]; then
        jq -r '.[] | "\(.status) \(.conclusion) \(.event) \(.url)"' <<< "${runs}"
        # A later run on the commit does not clear a failed one, since every
        # finished run has to pass. Re-running the failed run replaces its
        # result.
        jq -r '.[] | select(.status == "completed" and .conclusion != "success")
            | "  gh run rerun \(.databaseId) --failed"' <<< "${runs}"
        die "CI did not pass on ${sha}. Fix it and tag the fix; or, if the failure was flaky, re-run the failed run with the command above, then re-run this workflow."
    fi
    if [ "${total}" -gt 0 ] && [ "${running}" -eq 0 ]; then
        jq -r '.[] | "\(.conclusion) \(.event) \(.url)"' <<< "${runs}"
        break
    fi
    if [ "${total}" -eq 0 ] && [ "${elapsed}" -ge "${FIRST_RUN}" ]; then
        die "No CI run for ${sha}. ci.yml runs by itself on the head of a push to main."
    fi
    if [ "${elapsed}" -ge "${TIMEOUT}" ]; then
        die "CI on ${sha} was still running after ${TIMEOUT} seconds"
    fi
    echo "Waiting on CI for ${sha}: ${total} run(s), ${running} still running"
    sleep "${POLL}"
done

echo "${TAG} is the crate's version, has a CHANGELOG section, is on main and passed CI"
