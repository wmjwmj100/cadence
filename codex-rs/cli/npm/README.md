# @gradence/wecode

Install the Wecode CLI with npm:

```shell
npm i -g @gradence/wecode
wecode
```

This package contains the native `wecode` executable and a small Node.js launcher that selects the right binary for the current operating system and CPU architecture.

## Supported platforms

The launcher recognizes these target triples:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

## Release layout

Release automation should place binaries under:

```text
vendor/<target-triple>/wecode/wecode
vendor/<target-triple>/wecode/wecode.exe
```

Use `npm run prepare:local` to copy the current machine's locally built `wecode` binary into the expected vendor path for smoke testing from this repository.
