#!/bin/sh
# Multiview container entrypoint — portable GPU device-group resolution.
#
# The wgpu/Vulkan compositor (and DRM-based hwaccel) must open the kernel DRM
# render nodes /dev/dri/renderD* (owned root:render, mode 0660). The host's
# `render` group GID is NOT fixed — it varies by distro/host (105 on
# Debian/Ubuntu, but 39/992/… elsewhere) — so we CANNOT bake it into the image
# or hardcode a `group_add` in compose. Instead we detect the GID(s) of the
# device nodes actually mounted into THIS container at runtime and grant exactly
# those as supplementary groups to the unprivileged app user. Works on any host
# with zero configuration; degrades cleanly to no extra groups on a GPU-free /
# software-only deployment.
#
# Security: this script runs as root ONLY to resolve+set the groups, then
# IMMEDIATELY drops to the unprivileged uid:gid via setpriv. The multiview
# process NEVER runs as root. If we are already non-root (an image run with a
# fixed `user:`), we exec directly and rely on the caller's group_add.
set -eu

APP_UID=10001
APP_GID=10001

# Collect the distinct GIDs of the GPU device nodes the app needs to open.
collect_gids() {
    _gids=""
    for _node in /dev/dri/renderD* /dev/dri/card* /dev/nvidia-caps/*; do
        [ -e "$_node" ] || continue
        _gid=$(stat -c '%g' "$_node" 2>/dev/null) || continue
        # skip root group (0) — already accessible; keep only real device groups
        [ "$_gid" = "0" ] && continue
        case ",$_gids," in
            *",$_gid,"*) : ;;                       # already collected
            *) _gids="${_gids:+$_gids,}$_gid" ;;
        esac
    done
    printf '%s' "$_gids"
}

BIN=/usr/local/bin/multiview

if [ "$(id -u)" = "0" ]; then
    DEVICE_GIDS=$(collect_gids)
    if [ -n "$DEVICE_GIDS" ]; then
        echo "multiview-entrypoint: granting GPU device groups: $DEVICE_GIDS (uid $APP_UID)" >&2
        exec setpriv --reuid "$APP_UID" --regid "$APP_GID" \
            --groups "$APP_GID,$DEVICE_GIDS" --inh-caps=-all -- "$BIN" "$@"
    fi
    echo "multiview-entrypoint: no GPU device nodes found — running software-only" >&2
    exec setpriv --reuid "$APP_UID" --regid "$APP_GID" \
        --groups "$APP_GID" --inh-caps=-all -- "$BIN" "$@"
fi

# Already unprivileged (image run with an explicit `user:`); honor it as-is and
# rely on the caller's group membership / group_add for device access.
exec "$BIN" "$@"
