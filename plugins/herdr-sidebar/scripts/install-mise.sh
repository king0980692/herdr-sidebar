#!/bin/sh
# Install only the explicitly requested sidebar release; never bootstrap mise.
# Usage: sh scripts/install-mise.sh [--dry-run] [X.Y.Z]
set -eu

usage() {
  printf '%s\n' 'Usage: install-mise.sh [--dry-run] [X.Y.Z]' \
    'Default release: 0.16.4. Requires an existing mise installation.'
}

version=0.16.4
version_given=0
dry_run=0
for arg do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    --help|-h) usage; exit 0 ;;
    -*) usage >&2; exit 2 ;;
    *)
      if [ "$version_given" = 1 ]; then usage >&2; exit 2; fi
      version=$arg
      version_given=1
      ;;
  esac
done

# Validate without eval, external parsers, or accepting mise's version selectors.
case "$version" in
  ''|*[!0-9.]*|.*|*.|*..*)
    printf '%s\n' 'herdr-sidebar: version must be exactly X.Y.Z (digits only).' >&2
    exit 2 ;;
esac
old_ifs=$IFS
IFS=.
set -- $version
IFS=$old_ifs
if [ "$#" != 3 ]; then
  printf '%s\n' 'herdr-sidebar: version must be exactly X.Y.Z (digits only).' >&2
  exit 2
fi

if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
  printf '%s\n' 'herdr-sidebar: HOME must name an existing directory.' >&2
  exit 1
fi

if command -v mise >/dev/null 2>&1; then
  mise_bin=$(command -v mise)
  # Resolve relative PATH entries before leaving the project directory.
  case "$mise_bin" in /*) ;; *) mise_bin="$PWD/$mise_bin" ;; esac
elif [ -x "$HOME/.local/bin/mise" ]; then
  mise_bin="$HOME/.local/bin/mise"
elif [ -x "$HOME/.local/share/mise/bin/mise" ]; then
  mise_bin="$HOME/.local/share/mise/bin/mise"
else
  printf '%s\n' 'herdr-sidebar: mise was not found on PATH or in its standard user locations.' \
    'Install mise yourself using https://mise.jdx.dev/getting-started.html, then rerun this script.' >&2
  exit 127
fi

pattern='herdr-sidebar-{{ arch(x64="x86_64", arm64="aarch64") }}-{{ os(linux="unknown-linux-musl", macos="apple-darwin", windows="pc-windows-msvc") }}.tar.gz'
tool="github:king0980692/herdr-sidebar@$version"
# Avoid project trust/hooks. Do not use --yes or change global trust settings.
cd "$HOME"
set -- --cd "$HOME" use --global --pin --minimum-release-age 0 \
  --tool-option "asset_pattern=$pattern" --tool-option bin_path=bin "$tool"
if [ "$dry_run" = 1 ]; then
  "$mise_bin" "$@" --dry-run
  exit 0
fi
"$mise_bin" "$@"
# `where` only resolves the requested installation; never exec or install others.
install_dir=$("$mise_bin" --cd "$HOME" where "$tool")
printf '%s/bin/herdr-sidebar\n' "${install_dir%/}"
