#!/usr/bin/env bash
# Stall-aware, resumable, integrity-checked artifact downloader for CI/Docker.
# Small JSON/control-plane probes deliberately do not use this helper.
set -euo pipefail

usage() {
  echo "usage: $0 --url URL --output PATH --sha256 HEX [--expected-size BYTES]" >&2
  exit 2
}

url="" output="" expected_sha="" expected_size=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --url) url=${2:-}; shift 2 ;;
    --output) output=${2:-}; shift 2 ;;
    --sha256) expected_sha=${2:-}; shift 2 ;;
    --expected-size) expected_size=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$url" ] && [ -n "$output" ] && [ -n "$expected_sha" ] || usage
[[ "$expected_sha" =~ ^[0-9a-fA-F]{64}$ ]] || usage
if [ -n "$expected_size" ]; then case "$expected_size" in *[!0-9]*) usage ;; esac; fi

curl_bin=${SOLDR_DOWNLOAD_CURL_BIN:-curl}
retries=${SOLDR_DOWNLOAD_RETRIES:-5}
connect_timeout=${SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS:-30}
speed_limit=${SOLDR_DOWNLOAD_SPEED_LIMIT_BYTES_PER_SEC:-1024}
speed_time=${SOLDR_DOWNLOAD_SPEED_TIME_SECS:-90}
max_time=${SOLDR_DOWNLOAD_MAX_TIME_SECS:-7200}
part="${output}.part"
mkdir -p "$(dirname "$output")"

attempt=1
while :; do
  before=0; [ -f "$part" ] && before=$(wc -c < "$part")
  started=$(date +%s)
  set +e
  "$curl_bin" --fail --location --show-error --silent \
    --connect-timeout "$connect_timeout" \
    --speed-limit "$speed_limit" --speed-time "$speed_time" \
    --max-time "$max_time" --continue-at - --output "$part" "$url"
  status=$?
  set -e
  ended=$(date +%s)
  received=0; [ -f "$part" ] && received=$(wc -c < "$part")
  if [ "$status" -eq 0 ] && { [ -z "$expected_size" ] || [ "$received" -eq "$expected_size" ]; }; then
    actual=$(sha256sum "$part" | awk '{print $1}')
    if [ "$actual" != "${expected_sha,,}" ]; then
      echo "download failure=integrity bytes=$received expected_sha=$expected_sha actual_sha=$actual" >&2
      rm -f "$part"
      exit 1
    fi
    mv -f "$part" "$output"
    echo "download success bytes=$received attempts=$attempt output=$output" >&2
    exit 0
  fi
  elapsed=$((ended - started))
  if [ "$status" -eq 22 ]; then category=http
  elif [ "$status" -eq 6 ] || [ "$status" -eq 7 ]; then category=connect
  elif [ "$status" -eq 28 ] && [ "$elapsed" -ge "$max_time" ]; then category=global-safety
  elif [ "$status" -eq 28 ]; then category=stall
  else category=transport; fi
  if [ "$status" -eq 0 ]; then category=truncation; fi
  echo "download failure=$category curl_status=$status bytes=$received delta=$((received-before)) attempt=$attempt/$retries" >&2
  if [ "$attempt" -ge "$retries" ]; then exit 1; fi
  delay=$((2 ** (attempt - 1)))
  echo "download retry delay_secs=$delay resume_bytes=$received" >&2
  sleep "$delay"
  attempt=$((attempt + 1))
done
