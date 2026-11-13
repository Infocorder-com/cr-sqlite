#!/usr/bin/env bash
# init_race_stress.sh -- multi-process stress test for the Init Race Wedge
# (docs/CRSQLITE_INIT_RACE_FIX.md, "Concurrency stress test").
#
# Usage:  tools/init_race_stress.sh ROUNDS PROCS [MODE ...]
#   MODE: fresh  -- N processes open one brand-new file at once
#         wal    -- N processes open an initialized WAL file at once (pool start)
#         switch -- N processes open an initialized rollback-journal file while
#                  the first of them switches it to WAL after its open
#   default modes: fresh wal switch
#
# Needs `make sqlite3 vanilla` in core/ first: dist/sqlite3 has cr-sqlite
# statically linked and auto-loaded through core_init.c; dist/vanilla-sqlite3
# has no cr-sqlite and is used to inspect files.
#
# Environment:
#   INIT_RACE_STRESS_DIR  work directory (default: $TMPDIR or /tmp, plus a
#                         unique name). Must be under /tmp or under this repo
#                         (use a directory under core/dist for a disk-backed
#                         run when /tmp is tmpfs). It is deleted at the end.
#   SQLITE3, VANILLA      override the two shells.
#
# Classification is by STDERR, not exit code: the shell exits 1 for an open
# failure and a failed statement alike. Each file is inspected on a COPY with
# the cr-sqlite-free shell BEFORE any cr-sqlite connection opens it again,
# because a cr-sqlite open after the fix heals a wedged file first.
#
# Exit status: 0 when there were no wedged/partial files, no failed opens and
# no hangs; 1 otherwise; 2 on a usage or setup error.

set -u

if [ $# -lt 2 ]; then
  sed -n '2,30p' "$0" >&2
  exit 2
fi
ROUNDS=$1
PROCS=$2
shift 2
MODES=("$@")
[ ${#MODES[@]} -eq 0 ] && MODES=(fresh wal switch)

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SQLITE3=${SQLITE3:-$ROOT/core/dist/sqlite3}
VANILLA=${VANILLA:-$ROOT/core/dist/vanilla-sqlite3}
for bin in "$SQLITE3" "$VANILLA"; do
  if [ ! -x "$bin" ]; then
    echo "missing $bin -- run 'make sqlite3 vanilla' in core/" >&2
    exit 2
  fi
done

BASE=${TMPDIR:-/tmp}
WORK=${INIT_RACE_STRESS_DIR:-$BASE/init_race_stress.$$}
WORK=$(realpath -m "$WORK")
case "$WORK" in
  /tmp/*|"$ROOT"/*) ;;
  *) echo "refusing work directory $WORK (must be under /tmp or $ROOT)" >&2; exit 2 ;;
esac
mkdir -p "$WORK" || exit 2
cleanup() {
  case "$WORK" in
    /tmp/*|"$ROOT"/*) rm -rf -- "$WORK" ;;
  esac
}
trap cleanup EXIT

echo "init_race_stress: rounds=$ROUNDS procs=$PROCS modes=${MODES[*]}"
echo "  sqlite3=$SQLITE3"
echo "  work=$WORK ($(stat -f -c %T "$WORK" 2>/dev/null || echo '?'))"
echo "  crsql_build_id: $("$SQLITE3" :memory: 'SELECT crsql_build_id();' 2>&1)"

# Classify a file from a copy, without cr-sqlite. Prints one word:
# untouched | complete | wedged | partial | unreadable
inspect() {
  local db=$1 copy=$WORK/inspect.db s
  rm -f -- "$copy" "$copy-journal" "$copy-wal" "$copy-shm"
  [ -e "$db" ] || { echo untouched; return; }
  cp -- "$db" "$copy"
  for s in -journal -wal; do
    [ -e "$db$s" ] && cp -- "$db$s" "$copy$s"
  done
  local n
  n=$("$VANILLA" "$copy" \
    "SELECT count(*) FROM sqlite_master WHERE instr(lower(name),'crsql') > 0;" 2>/dev/null) ||
    { echo unreadable; return; }
  if [ "$n" = 0 ]; then echo untouched; return; fi
  local tables
  tables=$("$VANILLA" "$copy" "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('crsql_tracked_peers','crsql_db_versions','crsql_master','crsql_site_id');" 2>/dev/null)
  local has_master version
  has_master=$("$VANILLA" "$copy" "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='crsql_master';" 2>/dev/null)
  if [ "$has_master" = 1 ]; then
    version=$("$VANILLA" "$copy" "SELECT count(*) FROM crsql_master WHERE key='crsqlite_version';" 2>/dev/null)
    if [ "$version" = 0 ]; then echo wedged; return; fi
  fi
  if [ "$tables" = 4 ]; then
    local ok
    ok=$("$VANILLA" "$copy" "SELECT (SELECT count(*) FROM crsql_site_id WHERE ordinal=0)=1 AND (SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name='crsql_site_id')=3 AND (SELECT count(*) FROM sqlite_master WHERE type='index' AND name='crsql_site_id_site_id')=1;" 2>/dev/null)
    if [ "$ok" = 1 ]; then echo complete; return; fi
  fi
  echo partial
}

overall_bad=0

for MODE in "${MODES[@]}"; do
  case "$MODE" in fresh|wal|switch) ;; *) echo "unknown mode $MODE" >&2; exit 2 ;; esac
  ok=0; failed_open=0; refused=0; hang=0; pragma_fail=0; other=0
  rounds_with_failure=0
  untouched=0; complete=0; wedged=0; partial=0; unreadable=0
  quiet_ok=0; quiet_fail=0
  spread_max=0; spread_sum=0
  t_start=$(date +%s)

  for ((r = 1; r <= ROUNDS; r++)); do
    db=$WORK/r.db
    rm -f -- "$db" "$db-journal" "$db-wal" "$db-shm" "$WORK"/err.* "$WORK"/start.*
    case "$MODE" in
      wal) "$SQLITE3" "$db" 'PRAGMA journal_mode=WAL; SELECT crsql_site_id();' >/dev/null 2>&1 ;;
      switch) "$SQLITE3" "$db" 'SELECT crsql_site_id();' >/dev/null 2>&1 ;;
    esac

    # Barrier that cannot hang: the script holds the FIFO open read-write,
    # so opening it never blocks and a child that dies early strands nobody.
    fifo=$WORK/barrier
    rm -f -- "$fifo"
    mkfifo "$fifo"
    exec 3<>"$fifo"
    pids=()
    for ((j = 0; j < PROCS; j++)); do
      sql='SELECT crsql_site_id();'
      if [ "$MODE" = switch ] && [ $j -eq 0 ]; then
        sql='SELECT crsql_site_id(); PRAGMA journal_mode=WAL;'
      fi
      timeout 30 bash -c '
        read -r -u 3 _
        printf "%s\n" "$EPOCHREALTIME" > "$1"
        exec "$2" "$3" "$4"
      ' _ "$WORK/start.$j" "$SQLITE3" "$db" "$sql" >/dev/null 2>"$WORK/err.$j" &
      pids+=($!)
    done
    sleep 0.05
    printf '\n%.0s' $(seq "$PROCS") >&3
    codes=()
    for p in "${pids[@]}"; do
      wait "$p"
      codes+=($?)
    done
    exec 3>&-
    rm -f -- "$fifo"

    # Barrier spread (ms) from the start timestamps.
    spread=$(cat "$WORK"/start.* 2>/dev/null | sort -n | awk 'NR==1{a=$1} {b=$1} END{if (NR) printf "%d", (b-a)*1000; else print 0}')
    spread_sum=$((spread_sum + spread))
    [ "$spread" -gt "$spread_max" ] && spread_max=$spread

    round_failed=0
    for ((j = 0; j < PROCS; j++)); do
      e=$WORK/err.$j
      if [ "${codes[$j]}" = 124 ]; then
        hang=$((hang + 1)); round_failed=1
      elif grep -q 'unable to open database' "$e"; then
        failed_open=$((failed_open + 1)); round_failed=1
        grep -q 'version 0 is not supported' "$e" && refused=$((refused + 1))
      elif [ -s "$e" ]; then
        if [ "$MODE" = switch ] && [ $j -eq 0 ] && grep -q 'database is locked' "$e"; then
          pragma_fail=$((pragma_fail + 1))
        else
          other=$((other + 1))
          if [ $other -le 5 ]; then echo "  [$MODE r$r p$j] other: $(head -c 300 "$e")"; fi
        fi
      else
        ok=$((ok + 1))
      fi
    done
    rounds_with_failure=$((rounds_with_failure + round_failed))

    state=$(inspect "$db")
    case "$state" in
      untouched) untouched=$((untouched + 1)) ;;
      complete) complete=$((complete + 1)) ;;
      wedged) wedged=$((wedged + 1)) ;;
      partial) partial=$((partial + 1)) ;;
      *) unreadable=$((unreadable + 1)) ;;
    esac
    if [ "$state" = wedged ] || [ "$state" = partial ]; then
      if [ $((wedged + partial)) -le 3 ]; then
        echo "  [$MODE r$r] $state file: $("$VANILLA" "$WORK/inspect.db" "SELECT group_concat(name, ',') FROM sqlite_master;" 2>&1)"
      fi
    fi

    # Only now open it with cr-sqlite (which heals after the fix).
    if "$SQLITE3" "$db" 'SELECT crsql_site_id();' >/dev/null 2>&1; then
      quiet_ok=$((quiet_ok + 1))
    else
      quiet_fail=$((quiet_fail + 1))
    fi
  done

  elapsed=$(($(date +%s) - t_start))
  echo "mode=$MODE rounds=$ROUNDS procs=$PROCS elapsed=${elapsed}s"
  echo "  processes: ok=$ok failed_open=$failed_open (version-0 refusals=$refused) hang=$hang pragma_locked=$pragma_fail other_error=$other"
  echo "  rounds with >=1 failed open or hang: $rounds_with_failure / $ROUNDS"
  echo "  files after the round: untouched=$untouched complete=$complete wedged=$wedged partial=$partial unreadable=$unreadable"
  echo "  later cr-sqlite open: ok=$quiet_ok failed=$quiet_fail"
  echo "  barrier spread: max=${spread_max}ms mean=$((spread_sum / ROUNDS))ms"
  if [ $((failed_open + hang + wedged + partial + unreadable)) -gt 0 ]; then
    overall_bad=1
  fi
done

exit $overall_bad
