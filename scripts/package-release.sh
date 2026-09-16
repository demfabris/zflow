#!/usr/bin/env bash
# Assemble an archive from binaries already built on the target platform.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
target=${1:?usage: package-release.sh TARGET [BINARY_DIRECTORY]}
binary_dir=${2:-$root/target/release}
output="$root/target/dist"
name="zflow-v$version-$target"
mkdir -p "$output"
stage=$(mktemp -d "$output/.archive.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir "$stage/$name"
case "$target" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)
        install -d "$stage/$name/bin" "$stage/$name/scripts" "$stage/$name/packaging"
        install -m 0755 "$binary_dir/zflow" "$binary_dir/zflowd" "$stage/$name/bin/"
        install -m 0755 "$root/scripts/install.sh" "$root/scripts/uninstall.sh" "$stage/$name/scripts/"
        for directory in config linux modules-load.d system-sleep systemd udev; do
            cp -R "$root/packaging/$directory" "$stage/$name/packaging/"
        done
        ;;
    x86_64-apple-darwin|aarch64-apple-darwin)
        codesign --verify --strict --deep "$binary_dir/zflow.app"
        ditto "$binary_dir/zflow.app" "$stage/$name/zflow.app"
        ;;
    *) printf 'Unsupported release target: %s\n' "$target" >&2; exit 1 ;;
esac
install -m 0644 "$root/LICENSE" "$root/README.md" "$stage/$name/"
COPYFILE_DISABLE=1 tar -czf "$output/$name.tar.gz" -C "$stage" "$name"
printf 'Created %s/%s.tar.gz\n' "$output" "$name"
