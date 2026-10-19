# Docker（群晖 / NAS，无桌面）

当前 AIC 预发布仅提供 tar 包，不发布 GHCR 镜像；下面的镜像地址预留给后续稳定版本。可在发布包就绪后用 `docker compose build` 本地构建。

The current AIC prerelease publishes tarballs only. The GHCR image below is reserved for future stable releases; build locally after the release tarball is available.

本目录是 [`ghcr.io/teddyli18000/uu-remote-linux`](https://github.com/teddyli18000/uu-remote-linux/pkgs/container/uu-remote-linux) 镜像的构建文件。
**部署和使用步骤见 [docs/deploy-nas-docker.md](../docs/deploy-nas-docker.md)。**

This directory builds the `ghcr.io/teddyli18000/uu-remote-linux` image; see
[docs/deploy-nas-docker.md](../docs/deploy-nas-docker.md#english) for deployment.

自行构建 / build locally: `docker compose build`（`Dockerfile` 从 GitHub 发布包安装 `uuremote-server`，可用 `--build-arg VERSION=vX.Y.Z` 指定版本）。
