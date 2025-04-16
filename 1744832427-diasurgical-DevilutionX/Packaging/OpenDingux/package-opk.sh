#!/usr/bin/env bash

declare OPK_DESKTOP_NAME
declare OPK_DESKTOP_EXEC
declare -a OPK_EXTRA_FILES

package_opk() {
	local ext
	if [[ $TARGET == rg350 ]] || [[ $TARGET == gkd350h ]]; then
		ext=gcw0
	else
		ext="$TARGET"
	fi
	local -r tmp="${BUILD_DIR}/opk"
	set -x
	rm -rf "$tmp"
	mkdir -p "$tmp"
	cp "Packaging/OpenDingux/${TARGET}.desktop" "${tmp}/default.${ext}.desktop"
	cp "Packaging/OpenDingux/${TARGET}-manual.txt" "${tmp}/readme.${ext}.txt"

	if [[ -v OPK_DESKTOP_NAME ]]; then
		sed -i "s/Name=.*/Name=${OPK_DESKTOP_NAME}/" "${tmp}/default.${ext}.desktop"
	fi
	if [[ -v OPK_DESKTOP_EXEC ]]; then
		sed -i "s/Exec=.*/Exec=${OPK_DESKTOP_EXEC}/" "${tmp}/default.${ext}.desktop"
	fi

	mksquashfs "${BUILD_DIR}/devilutionx" \
		"${tmp}/default.${ext}.desktop" \
		"${tmp}/readme.${ext}.txt" Packaging/resources/icon_32.png \
		"${BUILD_DIR}/assets/" \
		"${OPK_EXTRA_FILES[@]}" \
		"${BUILD_DIR}/devilutionx-${TARGET}.opk" \
		-all-root -no-xattrs -noappend -no-exports -no-progress
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
	set -euo pipefail

	cd "$(dirname "${BASH_SOURCE[0]}")/../.."

	source Packaging/OpenDingux/targets.sh

	usage() {
		echo "Usage: package-opk.sh [target]"
		usage_target
	}

	if ! check_target "$@"; then
		usage
		exit 64
	fi

	declare -r TARGET="$1"
	declare -r BUILD_DIR="build-${TARGET}"
	package_opk
fi
