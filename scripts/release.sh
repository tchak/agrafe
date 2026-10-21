#!/usr/bin/env bash
# Cuts a release of every crate at one version: bumps the workspace's
# version and its internal requirements, gives the changelog's Unreleased
# section the version and today's date, commits "Release <version>" on top
# of main, tags it v<version> and, once confirmed, pushes main and the tag.
# The tag starts .github/workflows/release.yml, which publishes the Docker
# images, the crates and the GitHub release.
#
#   scripts/release.sh 0.2.0
#   scripts/release.sh 0.2.0-rc.1
#
# A pre-release leaves the changelog alone: Unreleased is its notes, and
# stays Unreleased until the release, which gets the section.
#
# Run it from an empty working copy on top of main, as `jj new main` leaves
# it, with the Unreleased section written.
set -euo pipefail
cd "$(dirname "$0")/.."

die() {
	echo "release: $*" >&2
	exit 1
}

semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'

# Whether version $1 precedes $2 in semver's order: the core numerically,
# then a pre-release before its release, then the pre-release identifiers
# one by one, numbers numerically and before words, a prefix first.
precedes() {
	local a=$1 b=$2 ap="" bp="" i p q
	local -a x y
	[[ $a == *-* ]] && ap=${a#*-}
	[[ $b == *-* ]] && bp=${b#*-}
	IFS=. read -ra x <<<"${a%%-*}"
	IFS=. read -ra y <<<"${b%%-*}"
	for i in 0 1 2; do
		((10#${x[i]} < 10#${y[i]})) && return 0
		((10#${x[i]} > 10#${y[i]})) && return 1
	done
	[[ -z $ap ]] && return 1
	[[ -z $bp ]] && return 0
	IFS=. read -ra x <<<"$ap"
	IFS=. read -ra y <<<"$bp"
	for ((i = 0; ; i++)); do
		((i == ${#y[@]})) && return 1
		((i == ${#x[@]})) && return 0
		p=${x[i]} q=${y[i]}
		[[ $p == "$q" ]] && continue
		if [[ $p =~ ^[0-9]+$ && $q =~ ^[0-9]+$ ]]; then
			((10#$p < 10#$q))
			return
		fi
		[[ $p =~ ^[0-9]+$ ]] && return 0
		[[ $q =~ ^[0-9]+$ ]] && return 1
		[[ $p < "$q" ]]
		return
	done
}

# The greatest of the versions on standard input, or nothing.
greatest() {
	local v max=""
	while read -r v; do
		if [[ -z $max ]] || precedes "$max" "$v"; then max=$v; fi
	done
	echo "$max"
}

# Sourced, the script defines the functions above and stops, so that the
# ordering can be tried on its own: `source scripts/release.sh; precedes 0.2.0-rc.1 0.2.0`.
[[ ${BASH_SOURCE[0]} == "$0" ]] || return 0

version=${1:-}
[[ $version =~ $semver ]] ||
	die "usage: scripts/release.sh <version>, such as 0.2.0 or 0.2.0-rc.1"
tag=v$version
prerelease=false
[[ $version == *-* ]] && prerelease=true

current=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[[ -n $current ]] || die "no version in [workspace.package]"
repository=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml)

jj git fetch --remote origin
[[ -z $(jj diff --summary) ]] || die "the working copy has changes; start from \`jj new main\`"
[[ -n $(jj log --no-graph -r '@- & main & main@origin' -T commit_id) ]] ||
	die "the working copy's parent must be main, and main what origin has"

versions=$(jj tag list --all-remotes -T 'name ++ "\n"' 'glob:v*' | sed 's/^v//' | sort -u |
	grep -E "$semver" || true)
grep -qxF "$version" <<<"$versions" && die "$tag exists"
# The last tag, which this one must follow, and the last release, which the
# changelog's links compare against.
previous=$(greatest <<<"$versions")
released=$({ grep -v -- - <<<"$versions" || true; } | greatest)
if [[ -n $previous ]] && ! precedes "$previous" "$version"; then
	die "$version is not after v$previous"
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

if [[ $prerelease == false ]]; then
	if [[ -n $released ]]; then
		link="$repository/compare/v$released...$tag"
	else
		link="$repository/releases/tag/$tag"
	fi
	VERSION=$version DATE=$(date +%F) TAG=$tag LINK=$link REPOSITORY=$repository perl -0pi -e '
		s/^## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{VERSION}] - $ENV{DATE}\n/m
			or die "no Unreleased heading\n";
		s/^\[Unreleased\]: .*$/[Unreleased]: $ENV{REPOSITORY}\/compare\/$ENV{TAG}...HEAD\n[$ENV{VERSION}]: $ENV{LINK}/m
			or die "no Unreleased link\n";
	' CHANGELOG.md
fi

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
