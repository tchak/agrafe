#!/usr/bin/env bash
# Cuts a release of every crate at one version: bumps the workspace's
# version and its internal requirements, gives the changelog's Unreleased
# section the version and today's date, commits "Release <version>" on top
# of main, tags it v<version> and, once confirmed, pushes main and the tag.
# The tag starts .github/workflows/release.yml, which publishes the Docker
# images, the crates and the GitHub release.
#
#   scripts/release.sh 0.2.0
#
# Run it from an empty working copy on top of main, as `jj new main` leaves
# it, with the Unreleased section written.
set -euo pipefail
cd "$(dirname "$0")/.."

die() {
	echo "release: $*" >&2
	exit 1
}

version=${1:-}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] ||
	die "usage: scripts/release.sh <version>, such as 0.2.0 or 0.2.0-rc.1"
tag=v$version

current=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[[ -n $current ]] || die "no version in [workspace.package]"
repository=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml)

jj git fetch --remote origin
[[ -z $(jj diff --summary) ]] || die "the working copy has changes; start from \`jj new main\`"
[[ -n $(jj log --no-graph -r '@- & main & main@origin' -T commit_id) ]] ||
	die "the working copy's parent must be main, and main what origin has"

tags=$(jj tag list --all-remotes -T 'name ++ "\n"' 'glob:v*' | sort -u)
grep -qxF "$tag" <<<"$tags" && die "$tag exists"
previous=$(sed 's/^v//' <<<"$tags" | sort -V | tail -n 1)
# The first release ships the version the manifests already carry.
if [[ -n $previous || $version != "$current" ]]; then
	[[ $(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -n 1) == "$version" &&
		$version != "$current" ]] || die "$version is not after $current"
fi

# Where `jj op restore` takes the repository back to if the release is not
# pushed.
start=$(jj op log --no-graph -n 1 -T 'id.short(12)')

# What the Unreleased section says, up to the next version's heading.
notes=$(awk '/^## \[Unreleased\]/ { on = 1; next } /^## \[/ { on = 0 } on' CHANGELOG.md)
[[ -n ${notes//[[:space:]]/} ]] || die "CHANGELOG.md has nothing under Unreleased"

if [[ $version != "$current" ]]; then
	sed -i.bak -E \
		-e "s/^version = \"$current\"$/version = \"$version\"/" \
		-e "s/^(agrafe[a-z-]*) = \{ version = \"$current\",/\1 = { version = \"$version\",/" \
		Cargo.toml
	rm Cargo.toml.bak
	cargo update --workspace --quiet
fi

if [[ -n $previous ]]; then
	link="$repository/compare/v$previous...$tag"
else
	link="$repository/releases/tag/$tag"
fi
VERSION=$version DATE=$(date +%F) TAG=$tag LINK=$link REPOSITORY=$repository perl -0pi -e '
	s/^## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{VERSION}] - $ENV{DATE}\n/m
		or die "no Unreleased heading\n";
	s/^\[Unreleased\]: .*$/[Unreleased]: $ENV{REPOSITORY}\/compare\/$ENV{TAG}...HEAD\n[$ENV{VERSION}]: $ENV{LINK}/m
		or die "no Unreleased link\n";
' CHANGELOG.md

jj commit -m "Release $version"
jj tag set "$tag" -r @-
jj bookmark set main -r @-
jj show --stat @-

read -r -p "Push main and $tag to origin? [y/N] " answer
if [[ $answer == [yY] ]]; then
	jj git push --remote origin --bookmark main --tag "$tag"
	echo "Pushed. The release workflow: $repository/actions/workflows/release.yml"
else
	echo "Not pushed. Later: jj git push --remote origin --bookmark main --tag $tag"
	echo "Or drop it: jj op restore $start"
fi
