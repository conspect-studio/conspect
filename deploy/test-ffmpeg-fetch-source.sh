#!/usr/bin/env bash
# Deterministic fetch-policy tests; no network or real source downloads.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/ffmpeg-fetch-source.sh"
work="$(mktemp -d)"
trap 'test -n "${work}" && test -d "${work}/bin" && rm -rf -- "${work}"' EXIT
mkdir -p "${work}/bin"
cat > "${work}/bin/curl" <<'MOCK'
#!/usr/bin/env bash
if [[ "$*" == *'.asc' ]]; then exit "${SIGNATURE_STATUS:-0}"; fi
exit "${DOWNLOAD_STATUS:-0}"
MOCK
cat > "${work}/bin/sha256sum" <<'MOCK'
#!/usr/bin/env bash
exit "${SHA_STATUS:-0}"
MOCK
cat > "${work}/bin/gpg" <<'MOCK'
#!/usr/bin/env bash
exit "${GPG_STATUS:-0}"
MOCK
cat > "${work}/bin/tar" <<'MOCK'
#!/usr/bin/env bash
exit "${TAR_STATUS:-1}"
MOCK
cat > "${work}/bin/git" <<'MOCK'
#!/usr/bin/env bash
printf 'called\n' >> "${GIT_MARKER}"
if [[ "$*" == *'clone'* ]]; then mkdir -p "${@: -1}"; else printf 'pinned\n'; fi
MOCK
chmod +x "${work}/bin/"*
export PATH="${work}/bin:${PATH}" FFMPEG_SHA256=pinned FFMPEG_GIT_COMMIT=pinned
export GIT_MARKER="${work}/git-called"
check() {
  local name="$1" expected="$2" fallback="$3" status=0
  shift 3
  : > "${GIT_MARKER}"
  env DOWNLOAD_STATUS=0 SIGNATURE_STATUS=0 SHA_STATUS=0 GPG_STATUS=0 TAR_STATUS=1 "$@" bash "${script}" "${work}/dest" > "${work}/result" 2>&1 || status=$?
  if [[ "${status}" != "${expected}" ]]; then
    printf 'FAIL %s: exit %s expected %s\n' "${name}" "${status}" "${expected}" >&2
    exit 1
  fi
  if [[ "${fallback}" == yes ]]; then test -s "${GIT_MARKER}"; else test ! -s "${GIT_MARKER}"; fi
  printf 'PASS %s\n' "${name}"
}
check http-tarball 3 no DOWNLOAD_STATUS=22
check http-signature 3 no SIGNATURE_STATUS=22
check certificate 3 no DOWNLOAD_STATUS=60
check local-write 3 no DOWNLOAD_STATUS=23
check checksum 4 no SHA_STATUS=1
check signature 3 no GPG_STATUS=1
check extraction 4 no TAR_STATUS=1
for status in 5 6 7 28 35 52 55 56; do
  check "transport-${status}" 0 yes "DOWNLOAD_STATUS=${status}"
done
check signature-transport 0 yes SIGNATURE_STATUS=28
