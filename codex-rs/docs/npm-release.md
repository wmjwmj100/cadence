# npm Release for Wecode

The npm package lives in `cli/npm` and publishes the `wecode` command as `@gradence/wecode`.

## Install command

Users install Wecode with one command:

```shell
npm i -g @gradence/wecode
```

After installation, `wecode` is available on `PATH` through the package `bin` entry.

## Package design

`cli/npm/bin/wecode.js` is a small Node.js launcher. It maps `process.platform` and `process.arch` to a Rust target triple, then executes the matching native binary from `cli/npm/vendor`.

The package is intentionally monolithic for the first npm release: one package contains all supported binaries. That makes `npm i -g @gradence/wecode` independent of optional dependency behavior, registry propagation order, and per-platform package ownership. If package size becomes a problem later, the same target matrix can be split into platform packages.

Supported release targets:

| Platform | Arch | Target triple | Binary |
| --- | --- | --- | --- |
| Linux | x64 | `x86_64-unknown-linux-musl` | `wecode` |
| Linux | arm64 | `aarch64-unknown-linux-musl` | `wecode` |
| macOS | x64 | `x86_64-apple-darwin` | `wecode` |
| macOS | arm64 | `aarch64-apple-darwin` | `wecode` |
| Windows | x64 | `x86_64-pc-windows-msvc` | `wecode.exe` |
| Windows | arm64 | `aarch64-pc-windows-msvc` | `wecode.exe` |

Release automation must copy each binary to:

```text
cli/npm/vendor/<target-triple>/wecode/wecode
cli/npm/vendor/<target-triple>/wecode/wecode.exe
```

`cli/npm/vendor` is gitignored so local smoke-test binaries do not get committed. The package has its own `.npmignore`, so release-generated vendor binaries are still included in `npm pack`.

## Local smoke test

From the repository root:

```shell
cargo build --release -p codex-cli
cd cli/npm
npm run prepare:local
npm run smoke
npm pack --dry-run
```

For an install-like test without publishing:

```shell
cd cli/npm
npm pack
npm install -g ./gradence-wecode-*.tgz
wecode --version
```

## GitHub release workflow

Run the manual `npm release package` workflow with:

- `version`: the npm version to publish, for example `0.1.0`.
- `publish`: keep `false` for dry-run packaging; set `true` only when `NPM_TOKEN` is configured and `@gradence/wecode` publish rights are confirmed.

The workflow builds all supported targets with Rust `1.93.0`, stages binaries into `cli/npm/vendor`, runs the launcher, packs the npm tarball, installs the tarball into a temporary global prefix, and runs `wecode --version` through the installed command.

## Publishing checklist

1. Confirm npm organization/package ownership for `@gradence/wecode`.
2. Configure an npm automation token as the `NPM_TOKEN` GitHub Actions secret.
3. Pick one release version source of truth for the release; the workflow input currently drives `cli/npm/package.json` via `npm version --no-git-tag-version`.
4. Run the workflow once with `publish=false` and download the `npm-package` artifact.
5. Install the packed tarball on representative Linux, macOS, and Windows machines and run `wecode --version`.
6. Re-run the workflow with `publish=true` to publish to npm.
