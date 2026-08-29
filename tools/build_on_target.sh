#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <ssh-host> <vX.Y.Z>" >&2
  exit 2
fi

deploy_host=$1
release_tag=$2
image_name="ghcr.io/furinelle/hanabi:${release_tag}"

if [[ ! $release_tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid release tag: $release_tag" >&2
  exit 2
fi

release_commit=$(git rev-parse --verify "refs/tags/${release_tag}^{commit}")
remote_arch=$(ssh "$deploy_host" 'docker version --format "{{.Server.Arch}}"')
remote_build_dir=$(
  ssh "$deploy_host" "mktemp -d '/var/tmp/hanabi-build-${release_tag}.XXXXXX'"
)

case "$remote_build_dir" in
  "/var/tmp/hanabi-build-${release_tag}."*) ;;
  *)
    echo "unexpected remote build directory: $remote_build_dir" >&2
    exit 1
    ;;
esac

cleanup() {
  ssh "$deploy_host" "rm -rf -- '$remote_build_dir'"
}
trap cleanup EXIT

git archive "$release_tag" | ssh "$deploy_host" "tar -x -C '$remote_build_dir'"
ssh "$deploy_host" \
  "cd '$remote_build_dir' && docker build --pull \
    --build-arg VCS_REF='$release_commit' \
    -t '$image_name' ."

ssh "$deploy_host" \
  "docker image inspect '$image_name' \
    --format 'image={{.Id}} os={{.Os}} arch={{.Architecture}} revision={{index .Config.Labels \"org.opencontainers.image.revision\"}}'"

echo "built $image_name natively on $deploy_host ($remote_arch)"
echo "deploy with: docker compose up -d --pull never hanabi"
