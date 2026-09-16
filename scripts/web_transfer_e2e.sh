#!/usr/bin/env bash
# T-WEB acceptance driver (plan 001, sub-phase 3.7) — everything the web
# transfer slice is gated on, in the order that makes a failure legible, with
# NO root and NO network namespaces: this feature is a server process, a
# browser and loopback, so a harness that needed sudo would be gating
# something the product does not do.
#
# Stages (each one is skippable through STAGES, all of them run by default):
#
#   assets   npm unit tests + a fresh bundle + the drift check (the embedded
#            dist is compiled INTO the binary, so a stale bundle silently
#            tests yesterday's app)
#   build    the debug binary and the e2e owner example, AFTER the bundle
#   rust     the serial Rust web-transfer suite (no-store, limits, room life,
#            legacy coexistence, protocol)
#   browser  Playwright on chromium, firefox and webkit — the three engines
#            the product claims
#   acceptance
#            the 5.4 gates alone — `T-WEB-MULTIPEER-FINAL`,
#            `T-WEB-OWNER-SEPARATION`, `T-WEB-SOURCE-ONLY` — on every engine
#            in ENGINES. NOT in the default STAGES because `browser` already
#            runs them; this stage exists to re-run the acceptance scenario
#            on its own without paying for the whole suite.
#   resources
#            the 6.1 resource gates — `T-WEB-SOAK`, `T-WEB-FDBUDGET`,
#            `T-WEB-FAIRNESS` — with the PLAN's soak window (SOAK_SECS,
#            default 300 s). NOT in the default STAGES: the `rust` stage
#            already runs the same three gates, but with the short window a
#            test suite can afford (15 s). The long window is what answers
#            "does it still hold after five minutes", which a 15 s run
#            cannot.
#   soak     `T-WEB-MULTIPEER` repeated on chromium (REPEATS, default 10).
#            NOT in the default STAGES because it costs minutes: it is the
#            4.4 acceptance criterion, which asks for the three-peer
#            scenario to be green repeatedly and not once. A scenario that
#            fails one run in ten is a flaky scenario, and the only way to
#            learn that is to run it ten times.
#   cross    `T-WEB-CROSS` — the engine PAIR matrix (6.4). Every other spec
#            puts the same engine on both sides; this one crosses them,
#            because the two peers negotiate. Driven by one project: the
#            spec names its own engines.
#   fuzz     the decoder fuzzers with the plan's budget (FUZZ_SECS, default
#            60 s PER DECODER). Deterministic and seeded, so a crash found
#            in CI reproduces here.
#   branded  `T-WEB-BRANDED` — Chrome and Edge, run when installed and
#            reported `NOT-INSTALLED` when not. Never a silent skip: a
#            release checklist item that goes green without running is worse
#            than one that never existed.
#   package  `T-WEB-PACKAGE` — pack the crate, build from the PACKED source
#            with no node tree, and ask the resulting binary for an asset.
#   release  `T-WEB-ACCEPTANCE` — the A/B/C scenario and the multipeer suite
#            against the RELEASE binary, which is the artefact that ships,
#            plus `T-WEB-README-RELEASE`: the README's own commands run
#            against that same artefact.
#   container
#            `T-WEB-NOSTORE-CONTAINER` — the shipped image with a read-only
#            root filesystem and no volume, moving a real file through its
#            relay in real browsers. Prints `N/A` without a usable docker.
#

# Rules this script obeys:
#   - the Rust suite runs SERIALLY (`--test-threads=1`): its tests bind real
#     ports and spawn real servers, and a parallel run fabricates failures;
#   - the browser suite is run on all three engines or not at all — a
#     compatibility claim proved on one engine is not a compatibility claim;
#   - a skipped stage is announced, never silent;
#   - the exit code is the verdict: any stage failing fails the script.
#
# Usage:
#   scripts/web_transfer_e2e.sh                 # everything
#   STAGES=rust scripts/web_transfer_e2e.sh     # one stage
#   ENGINES=chromium scripts/web_transfer_e2e.sh
#   STAGES=acceptance scripts/web_transfer_e2e.sh   # the 5.4 scenario alone
#   STAGES=soak scripts/web_transfer_e2e.sh     # the 4.4 no-flake criterion
#   STAGES=soak REPEATS=3 scripts/web_transfer_e2e.sh
#   STAGES=resources scripts/web_transfer_e2e.sh       # the 6.1 resource gates
#   STAGES=resources SOAK_SECS=60 scripts/web_transfer_e2e.sh
#   STAGES=cross scripts/web_transfer_e2e.sh          # the 6.4 pair matrix
#   STAGES=fuzz FUZZ_SECS=60 scripts/web_transfer_e2e.sh
#   STAGES=package scripts/web_transfer_e2e.sh        # the crate artifact
#   STAGES=branded scripts/web_transfer_e2e.sh        # release smoke
set -euo pipefail
export LC_ALL=C

STAGES="${STAGES:-assets,build,rust,browser}"
ENGINES="${ENGINES:-chromium,firefox,webkit}"
REPEATS="${REPEATS:-10}"
SOAK_SECS="${SOAK_SECS:-300}"
FUZZ_SECS="${FUZZ_SECS:-60}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
web="$root/web/transfer"

pass=0
fail=0

# `set -e` would abort on the first failing stage, so every stage runs
# through this: the script's job is to report ALL of them, and the exit code
# at the end is the verdict.
run() {
  local label="$1"
  shift
  if "$@"; then
    result 0 "$label"
  else
    result $? "$label"
  fi
}

say() { printf '\n== %s\n' "$*"; }
result() {
  if [ "$1" -eq 0 ]; then
    pass=$((pass + 1))
    printf 'PASS %s\n' "$2"
  else
    fail=$((fail + 1))
    printf 'FAIL %s\n' "$2"
  fi
}
wants() { case ",$STAGES," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

if wants assets; then
  say "assets — unit tests, fresh bundle, drift check"
  ( cd "$web" && npm ci --no-audit --no-fund >/dev/null 2>&1 || npm install --no-audit --no-fund >/dev/null )
  run assets bash -c 'cd "$0" && npm run check' "$web"
else
  echo "SKIP assets (not in STAGES=$STAGES)"
fi

if wants build; then
  say "build — debug binary with the bundle embedded"
  # The binary embeds `web/transfer/dist` at COMPILE time: building it after
  # the bundle is what makes every later stage test the app that was built.
  run build cargo build --all-features --bin bore --example web_transfer_e2e_owner
else
  echo "SKIP build (not in STAGES=$STAGES)"
fi

if wants rust; then
  say "rust — serial web-transfer suite"
  run rust cargo test --all-features --test web_transfer_test -- --test-threads=1
else
  echo "SKIP rust (not in STAGES=$STAGES)"
fi

if wants browser; then
  say "browser — Playwright on $ENGINES"
  projects=""
  IFS=',' read -r -a engines <<<"$ENGINES"
  for engine in "${engines[@]}"; do
    projects="$projects --project=$engine"
  done
  run browser bash -c 'cd "$0" && npx playwright test "$@" --reporter=line' "$web" $projects
else
  echo "SKIP browser (not in STAGES=$STAGES)"
fi

if wants acceptance; then
  say "acceptance — the 5.4 scenario on $ENGINES"
  projects=""
  IFS=',' read -r -a engines <<<"$ENGINES"
  for engine in "${engines[@]}"; do
    projects="$projects --project=$engine"
  done
  run acceptance bash -c 'cd "$0" && npx playwright test folder-zip.spec.mjs "$@" --reporter=line' "$web" $projects
else
  echo "SKIP acceptance (not in STAGES=$STAGES)"
fi

if wants soak; then
  say "soak — T-WEB-MULTIPEER x$REPEATS on chromium"
  # One flake anywhere in the repetitions fails the stage: Playwright's
  # `--repeat-each` reports every run, and the exit code is the verdict.
  # The pattern is the FULL title on purpose: 5.4 added
  # `T-WEB-MULTIPEER-FINAL`, and a bare `-g T-WEB-MULTIPEER` would quietly
  # start soaking two different scenarios and stop measuring the one this
  # stage is the criterion for.
  run soak bash -c 'cd "$0" && npx playwright test --project=chromium -g "T-WEB-MULTIPEER A direct to B" --repeat-each="$1" --reporter=line' "$web" "$REPEATS"
else
  echo "SKIP soak (not in STAGES=$STAGES)"
fi

if wants resources; then
  say "resources — the 6.1 gates with a ${SOAK_SECS}s soak window"
  # One cargo invocation per gate: `cargo test` takes a single filter, and
  # three named gates in one command would silently run only the first.
  export BORE_WEB_SOAK_SECS="$SOAK_SECS"
  run resources-soak cargo test --all-features --test web_transfer_test t_web_soak -- --test-threads=1 --nocapture
  run resources-fdbudget cargo test --all-features --test web_transfer_test t_web_fdbudget -- --test-threads=1
  run resources-fairness cargo test --all-features --test web_transfer_test t_web_fairness -- --test-threads=1 --nocapture
else
  echo "SKIP resources (not in STAGES=$STAGES)"
fi

if wants cross; then
  say "cross — T-WEB-CROSS engine pair matrix"
  # The matrix names its own engines inside the spec, so it runs under ONE
  # project: selecting three here would run the same ten transfers thrice.
  run cross bash -c 'cd "$0" && npx playwright test cross.spec.mjs --project=chromium --reporter=line' "$web"
else
  echo "SKIP cross (not in STAGES=$STAGES)"
fi

if wants fuzz; then
  say "fuzz — ${FUZZ_SECS}s per decoder"
  # CI-safe by construction: the budget is per decoder and the seed is
  # printed, so a crash found here reproduces exactly.
  run fuzz env BORE_WEB_FUZZ_SECS="$FUZZ_SECS" cargo test --all-features --test web_transfer_fuzz -- --nocapture
else
  echo "SKIP fuzz (not in STAGES=$STAGES)"
fi

if wants branded; then
  say "branded — T-WEB-BRANDED, Chrome and Edge when installed"
  # A channel that is not installed must SAY so. A silent skip is how a
  # release checklist item becomes green without ever having run, so the
  # stage prints `not installed` per channel and the release procedure —
  # not this script — decides whether that is acceptable.
  for pair in "chrome:branded-chrome:/opt/google/chrome/chrome" "msedge:branded-edge:/opt/microsoft/msedge/msedge"; do
    channel="${pair%%:*}"
    rest="${pair#*:}"
    project="${rest%%:*}"
    binary="${rest#*:}"
    if [ -x "$binary" ] || command -v "$channel" >/dev/null 2>&1; then
      run "branded-$channel" bash -c 'cd "$0" && npx playwright test --project="$1" --reporter=line' "$web" "$project"
    else
      printf 'NOT-INSTALLED %s (%s) — release checklist must run it on a machine that has it\n' "$channel" "$project"
    fi
  done
else
  echo "SKIP branded (not in STAGES=$STAGES)"
fi

if wants release; then
  say "release — T-WEB-ACCEPTANCE from the RELEASE artefact"
  # The acceptance claim is about the binary that ships. `--release` is a
  # different compiler configuration, so running the scenario against the
  # debug build proves it about something nobody deploys.
  run release-build cargo build --release --all-features --bin bore --example web_transfer_e2e_owner
  projects=""
  IFS=',' read -r -a engines <<<"$ENGINES"
  for engine in "${engines[@]}"; do
    projects="$projects --project=$engine"
  done
  run release-acceptance env \
    BORE_E2E_BIN="$root/target/release/bore" \
    BORE_E2E_OWNER_BIN="$root/target/release/examples/web_transfer_e2e_owner" \
    bash -c 'cd "$0" && npx playwright test folder-zip.spec.mjs multipeer.spec.mjs "$@" --reporter=line' "$web" $projects
  # The RELEASE binary's own help is what a reader will see; `t_web_readme`
  # compares the DEBUG one, and a feature gated differently between profiles
  # would leave the guide describing flags the shipped binary does not have.
  run release-help bash -c '
    set -euo pipefail
    bin="$1"; readme="$2"
    "$bin" server --help | grep -o -- "--web-transfer-[a-z-]*" | sort -u > /tmp/bore-release-help-flags
    "$bin" transfer web --help | grep -o -- "--[a-z-]*" | sort -u >> /tmp/bore-release-help-flags
    missing=0
    while read -r flag; do
      [ "$flag" = "--help" ] && continue
      # The guide names a flag either in prose (`--open`) or inside the help
      # block it quotes verbatim (`-v, --verbose...`), so the match is on the
      # WORD, not on a backtick.
      grep -qE -- "(^|[^a-zA-Z0-9_-])${flag}([^a-zA-Z0-9_-]|$)" "$readme" || { echo "MISSING in README: $flag"; missing=1; }
    done < /tmp/bore-release-help-flags
    exit "$missing"
  ' _ "$root/target/release/bore" "$root/README.md"
  # T-WEB-README-RELEASE: the README's OWN commands, run against the artefact
  # that ships. `readme.spec.mjs` starts the server and the room exactly as the
  # guide prints them, so pointing it at the release binaries is what turns
  # "the guide worked on my debug build" into "the guide works on the build a
  # reader will download".
  run release-readme env \
    BORE_E2E_BIN="$root/target/release/bore" \
    BORE_E2E_OWNER_BIN="$root/target/release/examples/web_transfer_e2e_owner" \
    bash -c 'cd "$0" && npx playwright test readme.spec.mjs "$@" --reporter=line' "$web" $projects
else
  echo "SKIP release (not in STAGES=$STAGES)"
fi

if wants container; then
  say "container — T-WEB-NOSTORE-CONTAINER from the shipped image"
  run container bash "$root/scripts/web_transfer_container_test.sh"
else
  echo "SKIP container (not in STAGES=$STAGES)"
fi

if wants package; then
  say "package — T-WEB-PACKAGE, the crate artifact serves the shell without Node"
  run package bash "$root/scripts/web_transfer_package_test.sh"
else
  echo "SKIP package (not in STAGES=$STAGES)"
fi

printf '\nPASS: %d FAIL: %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
