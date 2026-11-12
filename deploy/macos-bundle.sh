#!/usr/bin/env bash
# Make a macOS release directory self-contained (ADR-0011): copy every
# non-system dylib the binary needs (our pinned LGPL libav* and their
# third-party deps) into <stage>/lib, rewrite every install name to
# @rpath/<name>, give the binary an @executable_path/lib rpath, and sign
# inside-out (each dylib, then the binary). Dynamic linkage is kept, so a user
# can still replace the LGPL libraries.
#
# Usage: deploy/macos-bundle.sh <stage-dir> [signing-identity]
#   <stage-dir>        contains the `multiview` binary
#   signing-identity   a codesign identity; default "-" (ad-hoc). With a real
#                      identity the hardened runtime and a timestamp are used.
#
# Fails if any bundled object still references a non-system path, or if the
# binary cannot start with no DYLD_* override.
set -euo pipefail

STAGE="${1:?usage: macos-bundle.sh <stage-dir> [identity]}"
IDENTITY="${2:--}"
BIN="${STAGE}/multiview"
LIB="${STAGE}/lib"

[ "$(uname -s)" = Darwin ] || { echo "macos-bundle: run on macOS" >&2; exit 2; }
[ -x "${BIN}" ] || { echo "macos-bundle: ${BIN} missing" >&2; exit 2; }
mkdir -p "${LIB}"

# A reference is a system one when it lives in the OS: /usr/lib or
# /System. Everything else (Homebrew, the FFmpeg prefix) is bundled.
is_system() {
  case "$1" in
    /usr/lib/* | /System/*) return 0 ;;
    *) return 1 ;;
  esac
}

rpaths_of() {
  otool -l "$1" | awk '$1 == "cmd" && $2 == "LC_RPATH" { r = 1; next } r && $1 == "path" { print $2; r = 0 }'
}

# Non-system, non-@rpath dependency paths of a Mach-O (first line of
# `otool -L` is the object itself for a dylib; skip it via -D comparison).
deps_of() {
  local self
  self="$(otool -D "$1" 2>/dev/null | sed -n 2p || true)"
  otool -L "$1" | tail -n +2 | awk '{print $1}' | while read -r dep; do
    [ "${dep}" = "${self}" ] && continue
    case "${dep}" in
      @rpath/* | @loader_path/* | @executable_path/*) continue ;;
    esac
    is_system "${dep}" || echo "${dep}"
  done
}

# Copy the licence texts of the package a bundled library came from
# (Homebrew keeps them at Cellar/<formula>/<version>/). Upstream texts are
# copied verbatim, never edited. A library outside a Cellar must bring its own
# texts (the FFmpeg prefix's are copied by the release workflow).
collect_licences() {
  local real="$1" pkgdir formula
  case "${real}" in
    */Cellar/*/*/lib/*) ;;
    *) return 0 ;;
  esac
  pkgdir="${real%/lib/*}"
  formula="$(basename "$(dirname "${pkgdir}")")"
  [ -d "${STAGE}/licenses/${formula}" ] && return 0
  mkdir -p "${STAGE}/licenses/${formula}"
  find "${pkgdir}" -maxdepth 1 -type f \( -iname 'COPYING*' -o -iname 'LICENSE*' -o -iname 'LICENCE*' -o -iname 'NOTICE*' -o -iname 'AUTHORS*' \) \
    -exec cp -f {} "${STAGE}/licenses/${formula}/" \;
  if [ -z "$(ls -A "${STAGE}/licenses/${formula}")" ]; then
    echo "macos-bundle: no licence text found for ${formula} (${pkgdir})" >&2
    exit 4
  fi
}

# Breadth-first copy of the dependency closure (bash 3.2 safe: no
# associative arrays). SOURCES records "<basename> <realpath>" per bundled
# library, so two different libraries with one basename are an error, not a
# silent collapse.
SOURCES="$(mktemp)"
queue=("${BIN}")
i=0
while [ "${i}" -lt "${#queue[@]}" ]; do
  obj="${queue[${i}]}"
  i=$((i + 1))
  while read -r dep; do
    [ -n "${dep}" ] || continue
    name="$(basename "${dep}")"
    real="$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "${dep}")"
    prev="$(awk -v n="${name}" '$1 == n { print $2; exit }' "${SOURCES}")"
    if [ -n "${prev}" ]; then
      if [ "${prev}" != "${real}" ] && ! cmp -s "${prev}" "${real}"; then
        echo "macos-bundle: two different libraries named ${name}: ${prev} and ${real}" >&2
        exit 5
      fi
      continue
    fi
    printf '%s %s\n' "${name}" "${real}" >> "${SOURCES}"
    cp -f "${real}" "${LIB}/${name}"
    chmod u+w "${LIB}/${name}"
    collect_licences "${real}"
    queue+=("${LIB}/${name}")
  done < <(deps_of "${obj}")
done

# Rewrite install names and references.
rewrite() {
  local obj="$1"
  while read -r dep; do
    [ -n "${dep}" ] || continue
    install_name_tool -change "${dep}" "@rpath/$(basename "${dep}")" "${obj}"
  done < <(deps_of "${obj}")
}
for dylib in "${LIB}"/*.dylib; do
  [ -e "${dylib}" ] || continue
  install_name_tool -id "@rpath/$(basename "${dylib}")" "${dylib}"
  rewrite "${dylib}"
  # A bundled dylib finds its siblings next to itself.
  install_name_tool -add_rpath "@loader_path" "${dylib}" 2>/dev/null || true
done
rewrite "${BIN}"
install_name_tool -add_rpath "@executable_path/lib" "${BIN}" 2>/dev/null || true
# Drop every rpath that points outside the bundle (e.g. a build prefix), so
# nothing can resolve through it on the user's machine.
for obj in "${BIN}" "${LIB}"/*.dylib; do
  [ -e "${obj}" ] || continue
  for rp in $(rpaths_of "${obj}"); do
    case "${rp}" in
      @executable_path/lib | @loader_path) ;;
      *) install_name_tool -delete_rpath "${rp}" "${obj}" ;;
    esac
  done
done

# Verify: every load command of every object resolves to the OS or to a file
# in the bundle. @rpath / @loader_path / @executable_path references are
# resolved here, not exempted.
# Resolve one load command exactly as dyld would for this object: expand
# @loader_path / @executable_path, and try each LC_RPATH in order for @rpath.
# The first existing candidate must be inside the bundle (or the OS).
STAGE_ABS="$(cd "${STAGE}" && pwd -P)"
expand() {
  local obj="$1" path="$2" odir edir
  odir="$(cd "$(dirname "${obj}")" && pwd -P)"
  edir="$(cd "$(dirname "${BIN}")" && pwd -P)"
  path="${path/#@loader_path/${odir}}"
  path="${path/#@executable_path/${edir}}"
  printf '%s' "${path}"
}
inside_bundle() {
  local real
  real="$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$1")"
  case "${real}" in
    "${STAGE_ABS}"/*) return 0 ;;
    *) return 1 ;;
  esac
}
resolves() {
  local obj="$1" dep="$2" cand rp
  case "${dep}" in
    /usr/lib/* | /System/*) return 0 ;;
    @loader_path/* | @executable_path/*)
      cand="$(expand "${obj}" "${dep}")"
      [ -f "${cand}" ] && inside_bundle "${cand}"
      return ;;
    @rpath/*)
      # rpaths of the referencing object, then (for dylibs) the executable's.
      for rp in $(rpaths_of "${obj}") $( [ "${obj}" != "${BIN}" ] && rpaths_of "${BIN}" ); do
        cand="$(expand "${obj}" "${rp}")/${dep#@rpath/}"
        if [ -f "${cand}" ]; then
          inside_bundle "${cand}"
          return
        fi
      done
      return 1 ;;
    *) return 1 ;;
  esac
}
bad=0
for obj in "${BIN}" "${LIB}"/*.dylib; do
  [ -e "${obj}" ] || continue
  self="$(otool -D "${obj}" 2>/dev/null | sed -n 2p || true)"
  while read -r dep; do
    [ -n "${dep}" ] || continue
    [ "${dep}" = "${self}" ] && continue
    if ! resolves "${obj}" "${dep}"; then
      echo "macos-bundle: ${obj} references ${dep}, which is not in the bundle or the OS" >&2
      bad=1
    fi
  done < <(otool -L "${obj}" | tail -n +2 | awk '{print $1}')
done
[ "${bad}" = 0 ] || exit 3

# Sign inside-out. install_name_tool invalidated every signature, and arm64
# refuses to run unsigned code, so ad-hoc signing is the minimum.
# The executable keeps hardened runtime but disables library validation, so a
# user can replace a bundled LGPL dylib with their own (differently signed)
# build, as the LGPL requires. The libraries themselves need no entitlement.
ENTITLEMENTS="$(mktemp)"
cat > "${ENTITLEMENTS}" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.cs.disable-library-validation</key><true/>
</dict></plist>
PLIST
sign() {
  local extra=()
  [ "$1" = "${BIN}" ] && extra=(--entitlements "${ENTITLEMENTS}")
  if [ "${IDENTITY}" = "-" ]; then
    codesign --force --sign - ${extra[@]+"${extra[@]}"} "$1"
  else
    codesign --force --options runtime --timestamp --sign "${IDENTITY}" ${extra[@]+"${extra[@]}"} "$1"
  fi
}
for dylib in "${LIB}"/*.dylib; do
  [ -e "${dylib}" ] || continue
  sign "${dylib}"
done
sign "${BIN}"
codesign --verify --strict --verbose=1 "${BIN}"

# The bundle must start with no loader override.
env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH "${BIN}" --version
echo "macos-bundle: OK — $(ls "${LIB}" | wc -l | tr -d ' ') dylib(s) bundled"
