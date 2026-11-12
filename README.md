## LeekWallet signer branch (this fork)

The `leekwallet-signer` branch is Dorian's
[zingolib PR #3147](https://github.com/zingolabs/zingolib/pull/3147) at its
head `abf25aff`, plus two of our commits. The PCZT signing (the
external-signer wallet, the PCZT build, prove, finish and abandon steps and
their FFI) is Zingo Labs' work, used unmodified.

Our commits touch zingo-cli only. They add the LeekWallet board
([zcash.leekwallet.xyz](https://zcash.leekwallet.xyz),
[0xOucan/leekwallet-zcash](https://github.com/0xOucan/leekwallet-zcash)) as a
signer, behind the cargo feature `leekwallet` (off by default). The LeekWallet
SDK crates are path dependencies on `../../../leekwallet-zcash/host`.

- `3e88d2f04` (Add LeekWallet signer to zingo-cli) adds `--leek-port`, which
  creates a wallet from the board over USB after the six-digit pairing code
  and the board's "Share view key?" prompt. It adds `pczt sign_usb`, which
  signs the stored proposal on the board over USB, verifies every signature,
  proves and stores the transaction for `transmit`. `pczt export`, `import`
  and `abandon` cover the SD card. The `--signer-fingerprint` and
  `--signer-account` options build a `WalletConfig::UfvkWithDerivation` wallet
  for any PCZT signer.
- `f1add3152` (Accept --signer-account with --leek-port) lets
  `--signer-account` go with `--leek-port`. Before, it required
  `--signer-fingerprint`, and the board import was refused.

On 2026-11-11 zingo-cli signed a testnet transaction on the board over USB and
broadcast it:
`43e42f8cc2e68f332ad36183d6c0f0b412f37b2f35bc7ff7b21a533f59cb9378`.

Tests on `f1add3152`, re-run on 2026-11-12, with no network:
`cargo test -p zingo-cli --features leekwallet` passed 250 unit tests,
including the LeekWallet SD-card round trip with a software signer, and the
other test targets passed 2 with 1 ignored. `cargo test -p zingo-cli` with the
default features passed 246 unit tests and the same 2 with 1 ignored.

The Android app that uses this branch is our zingo-mobile fork,
[0xOucan/zingo-mobile `leekwallet-qr`](https://github.com/0xOucan/zingo-mobile/tree/leekwallet-qr).
The branch is offered to the Zingo team for review. Per `AI_POLICY.md`: our
two commits and this section were written with Claude Code (Claude Opus 5.5),
with 0xOucan directing the work. 0xOucan ran the board test and answers for
the code.

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
