## LeekWallet signer branch (this fork)

The `leekwallet-signer` branch is Dorian's
[zingolib PR #3147](https://github.com/zingolabs/zingolib/pull/3147) at its
head `abf25aff`, plus our commits. The PCZT signing (the external-signer
wallet, the PCZT build, prove, finish and abandon steps and their FFI) is
Zingo Labs' work, used unmodified.

Our commits touch zingo-cli only. They add the LeekWallet board
([zcash.leekwallet.xyz](https://zcash.leekwallet.xyz),
[0xOucan/leekwallet-zcash](https://github.com/0xOucan/leekwallet-zcash)) as a
signer, behind the cargo feature `leekwallet` (off by default). The LeekWallet
SDK crates (`leek-zcash`, `leek-device`, `leek-ur`, Apache-2.0) are git
dependencies on that repository, pinned to main `e58dc8d6`, so the branch
builds without any local checkout. Bluetooth uses btleplug, which on Linux
needs `libdbus-1-dev` to build.

The board is reached over four links, and every path ends in the same
prove-and-store step. `transmit` then broadcasts:

| Link | Create the wallet | Sign | On the board |
|---|---|---|---|
| USB | `--leek-port <path>` | `pczt sign_usb --port <path>` | tested 2026-11-11 |
| Bluetooth LE | `--leek-ble [--leek-ble-addr <addr>] [--leek-ble-scan <s>]` | `pczt sign_ble [--ble-addr <addr>] [--ble-scan <s>]` | built, not yet tested |
| QR | none (QR never pairs) | `pczt sign_qr`, `pczt import_qr` | built, not yet tested |
| SD card | none | `pczt export --card <dir>`, `pczt import --card <dir>` | built |

- **USB and Bluetooth.** Every connection pairs afresh with the six-digit
  code, which you compare on the board before pressing ALLOW. The wallet
  imports the viewing key only after the board's "Share view key?" prompt.
  No host is remembered.
  - Bluetooth scans for the LeekWallet service UUID. With two boards
    advertising, it refuses and lists them, so pick one by address.
  - Errors say what to do, for example "On the board: Settings → Link → Bluetooth".
  - The Bluetooth port is `leek-device`'s, the transport the LeekWallet app uses. The frames and the encrypted session are the same as over USB.
- **QR.** `pczt sign_qr` shows the compact signer view as an animated
  `ur:leek-pczt` (UR from `leek-ur`).
  - Display: half-block characters in the terminal, and/or `--html <file>` for a self-contained offline page.
  - Defaults: 50 B fragments and 300 ms frames, from leek-ur's laptop display hints. `--fragment` and `--interval` override them.
  - The board's reply, `ur:leek-zsig`, is pasted or read with `--reply <file>`. That file holds the UR parts or the `.zsig` itself.
  - With the feature `leekwallet-qr-camera`, `--camera` reads the reply off a V4L2 webcam instead.
  - Before anything is applied, the reply's hash must name the shown view and every signature is verified (`leek_zcash::finish_sd_sign`).
  - A reply not given at the prompt keeps the session for `pczt import_qr`.
- **Generic options.** `--signer-fingerprint` and `--signer-account` build a
  `WalletConfig::UfvkWithDerivation` wallet for any PCZT signer.
- **Ending a session.** A failure after the build abandons the session, which
  releases the inputs. `pczt abandon` does the same by hand.

Commits:

- `3e88d2f04` Add LeekWallet signer to zingo-cli: USB, SD card, the generic options.
- `f1add3152` Accept `--signer-account` with `--leek-port`.
- `7874f4faf` (feat(zingo-cli): sign with LeekWallet over Bluetooth LE and QR): Bluetooth and QR. It also switches the SDK to the pinned git dependency.

On 2026-11-11 zingo-cli signed a testnet transaction on the board over USB and
broadcast it:
`43e42f8cc2e68f332ad36183d6c0f0b412f37b2f35bc7ff7b21a533f59cb9378`.

Tests, run on 2026-11-12 with no network, board or radio:

| Command | Unit tests | Other test targets |
|---|---|---|
| `cargo test -p zingo-cli --features leekwallet` | 268 passed | 2 passed, 1 ignored |
| `cargo test -p zingo-cli --features leekwallet-qr-camera` | 270 passed | 2 passed, 1 ignored |
| `cargo test -p zingo-cli` (default features) | 250 passed | 2 passed, 1 ignored |

`cargo build -p zingo-cli --features leekwallet --release` is OK.

The `leekwallet` unit tests include:
- the SD-card and QR round trips with a software signer, from the PCZT through the UR, decoding, verification, proving and storing as Calculated within the NU7 expiry window;
- a refused reply for another view;
- the option parsing of `sign_ble`, `sign_qr`, `import_qr` and the `--leek-ble` session options;
- the QR drawing and HTML page.

The Android app that uses this branch is our zingo-mobile fork,
[0xOucan/zingo-mobile `leekwallet-qr`](https://github.com/0xOucan/zingo-mobile/tree/leekwallet-qr).
The branch is offered to the Zingo team for review.

Per `AI_POLICY.md`: our commits and this section were written with Claude Code
(Claude Opus 5.5), with 0xOucan directing the work. 0xOucan ran the board
test and answers for the code.

## Zingolib
[![license](https://img.shields.io/github/license/zingolabs/zingolib)](LICENSE) [![coverage](https://img.shields.io/endpoint?url=https://zingolabs.org/zingolib/coverage/badge.json)](https://zingolabs.org/zingolib/coverage/)
This repo provides both a library for zingo-mobile, as well as an included cli application to interact with the Zcash blockchain through a chain indexer.

# Security Vulnerability Disclosure

If you believe you have discovered a security issue, please contact us at:

zingodisclosure@proton.me

## Zingo CLI
`zingo-cli` is a command line indexer client. Its own README,
[zingo-cli/README.md](zingo-cli/README.md), holds everything specific to it:
the one procedure that builds it, which bundles the `nym-proxy` binary beside
it, and how to launch it, take it online, choose a network (regtest included),
create or restore a wallet, and set its session options. Releases are currently
only provisional, we will update the README as releases come out.

## Privacy
* While all the keys and transaction detection happens on the client, the server can learn what blocks contain your shielded transactions.
* The server also learns other metadata about you like your ip address etc...
* Also remember that t-addresses are publicly visible on the blockchain.
* Price information is retrieved from Gemini exchange.

### Note Management
Zingo-CLI does automatic note and utxo management, which means it doesn't allow you to manually select which address to send outgoing transactions from. It follows these principles:
* Defaults to sending shielded transactions, even if you're sending to a transparent address
* Can select funds from multiple shielded addresses in the same transaction
* Will automatically shield your sapling funds at the first opportunity
    * When sending an outgoing transaction to a shielded address, Zingo-CLI can decide to use the transaction to additionally shield your sapling funds (i.e., send your sapling funds to your own orchard address in the same transaction)
* Transparent funds are only spent via explicit shield operations

## Mobile releases

zingolib supports React Native mobile development. A mobile app links the
Binding Layer, the uniffi-exported wallet and proxy crates with their Kotlin
and Swift Binding Sets, and consumes it as Prebuilt Bundles published from
commits of `dev`. [bindings/README.md](bindings/README.md) explains how a
zingolib developer builds the Binding Layer locally and how a publication
runs, from the workflow dispatch to the Publication Record.

## Architecture decision records

Architecture decision records live in
[zingo-adrs](https://github.com/zingolabs/zingo-adrs), and zingolib's own
records sit under `docs/adr/zingolib/` once the submodule is initialised. The
[zingo-adrs README](https://github.com/zingolabs/zingo-adrs#pointing-a-code-repository-at-zingo-adrs) explains how to read them, advance the pointer,
and propose a record.

## Testing
`run_workspace_tests.sh` script may be used as a helper to run all tests in one invocation.
