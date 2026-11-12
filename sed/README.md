# may-sed

The `sed` of sandboxed agent scripts. The `aither-sed` binary implements
exactly the GNU `sed -n` forms whose script is made of `p` commands addressed
by `N`, `N,M`, `N,$` or `$`, joined by `;`, over files, `-` or standard
input; any other option or script is refused with a message naming the tools
to use instead. `may_sed::supports` decides whether a call is one of those
forms, which May's command policy uses to let only them reach the host `sed`.

`aither-sed --version` prints `aither-sed <version>`. The prebuilt binaries
(macOS arm64, Linux musl x86_64/arm64) are built by the `tool-artifacts`
workflow and attached to the `sed/v<version>` release.
