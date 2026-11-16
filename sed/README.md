# may-sed

The `sed` of sandboxed agent scripts. The `aither-sed` binary runs every
script on [uutils' `sed`](https://crates.io/crates/sed) in its `--sandbox`
mode, which refuses the `w`/`W`/`r`/`R`/`e` commands and the `s///w` and
`s///e` flags while the script compiles, before any file is opened; it also
refuses `-i`/`--in-place`. Every refusal names `apply_patch` as the way to
change files. Substitutions, regex addresses and ranges, `-E`, `d`, `q`, `y`,
`=` and the other commands that write nothing run the same on every platform.

`aither-sed --version` prints `aither-sed <version>`. The prebuilt binaries
(macOS arm64, Linux musl x86_64/arm64) are built by the `tool-artifacts`
workflow and attached to the `sed/v<version>` release.
