# Releasing

1. Bump `version` in `Cargo.toml`, then run `cargo build` so `Cargo.lock` picks it up.
2. Commit, and push to `main`.
3. Tag and push:

   ```sh
   git tag -a v0.1.6 -m "lantiq-exporter v0.1.6"
   git push origin v0.1.6
   ```

The tag starts `build-deb.yml`. It builds the `.deb`, attaches it to a GitHub
release, and then triggers `update-repo.yml` in `charlieh0tel/apt-repo`, which
needs the `APT_REPO_TOKEN` secret.

See [charlieh0tel/deb-workflows](https://github.com/charlieh0tel/deb-workflows)
for how the reusable workflows work.
