//! LeekWallet hardware signing for zingo-cli (cargo feature `leekwallet`).
//!
//! Prototype by 0xOucan on top of zingolib#3147 (dorianvp), which provides
//! the generic external-signer wallet and the PCZT build, prove and store
//! steps used here. This module only drives the LeekWallet board through the
//! LeekWallet SDK (`leek-zcash`, `leek-device`):
//!
//! - `pczt sign_usb`: the stored proposal's PCZT is signed on the board over
//!   USB serial, proved, and stored as Calculated; `transmit` broadcasts it.
//! - `pczt export` / `pczt import`: the same seam over files, for the SD
//!   card (`LEEKZ/unsigned.pczt` out, `LEEKZ/signed.zsig` back).
//! - `pczt abandon`: ends a signing session and releases its inputs.
//!
//! Every USB session pairs afresh: the board and the terminal show the same
//! six digits and the user presses ALLOW on the board. No host is remembered.

use std::path::{Path, PathBuf};

use leek_zcash::leek_device::device::Device;
use leek_zcash::{AccountMeta, LeekError, SignOptions, SignResult, SignerView};
use zingolib::config::ChainType;
use zingolib::lightclient::LightClient;
use zingolib::lightclient::error::LightClientError;
use zingolib::wallet::keys::unified::{UnifiedKeyStore, encode_ufvk};
use zingolib::wallet::pczt::{Pczt, PcztError};

/// The authoritative PCZT of an exported session, kept beside the wallet
/// file until `pczt import` or `pczt abandon`. It never goes to the card:
/// it carries the witnesses and wallet metadata the signer view drops.
const PENDING_PCZT: &str = "leek-pending.pczt";

/// A parsed `pczt` command.
#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
#[command(rename_all = "snake_case")]
pub(crate) enum PcztSubCommand {
    #[command(about = "Sign the stored proposal on a LeekWallet board over USB serial")]
    SignUsb {
        /// The board's serial port, e.g. /dev/ttyACM0.
        #[arg(long)]
        port: String,
        /// Send the compact signer view (about half the bytes).
        #[arg(long)]
        compact: bool,
    },
    #[command(about = "Write the stored proposal's signer view to an SD card for the board")]
    Export {
        /// The card's root directory; the file goes to LEEKZ/unsigned.pczt.
        #[arg(long)]
        card: PathBuf,
        /// Write the compact signer view (about half the bytes).
        #[arg(long)]
        compact: bool,
    },
    #[command(about = "Apply the board's LEEKZ/signed.zsig from the card, prove, and store")]
    Import {
        /// The card's root directory.
        #[arg(long)]
        card: PathBuf,
    },
    #[command(about = "End the signing session and release its reserved inputs")]
    Abandon,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LeekCommandError {
    #[error(
        "this wallet is not an external-signer wallet: create it with --viewkey, \
         --signer-fingerprint and --signer-account, or with --leek-port"
    )]
    NotExternalSigner,
    #[error("LeekWallet signs on mainnet and testnet only")]
    UnsupportedChain,
    #[error("no exported signing session: run `pczt export` first")]
    NoPendingSession,
    #[error(transparent)]
    Client(#[from] LightClientError),
    #[error(transparent)]
    Pczt(#[from] PcztError),
    #[error("LeekWallet: {0} (code {code})", code = .0.code())]
    Leek(LeekError),
    #[error("file {path}: {source}")]
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the signing task stopped: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl From<LeekError> for LeekCommandError {
    fn from(error: LeekError) -> Self {
        Self::Leek(error)
    }
}

fn file_error(path: &Path) -> impl FnOnce(std::io::Error) -> LeekCommandError + '_ {
    move |source| LeekCommandError::File {
        path: path.to_path_buf(),
        source,
    }
}

fn leek_network(chain: ChainType) -> Result<leek_zcash::Network, LeekCommandError> {
    match chain {
        ChainType::Mainnet => Ok(leek_zcash::Network::Main),
        ChainType::Testnet => Ok(leek_zcash::Network::Test),
        ChainType::Regtest(_) => Err(LeekCommandError::UnsupportedChain),
    }
}

/// The signer account: the wallet's first account, which must carry the
/// ZIP 32 derivation #3147 stores for an external signer.
async fn signer_account(
    lightclient: &LightClient,
) -> Result<(zip32::AccountId, AccountMeta), LeekCommandError> {
    let wallet = lightclient.wallet().read().await;
    let (&account, keys) = wallet
        .unified_key_store
        .iter()
        .next()
        .ok_or(LeekCommandError::NotExternalSigner)?;
    let (UnifiedKeyStore::View(ufvk), Some(derivation)) =
        (keys, wallet.account_derivation(account))
    else {
        return Err(LeekCommandError::NotExternalSigner);
    };
    let meta = AccountMeta {
        seed_fingerprint: derivation.seed_fingerprint().to_bytes(),
        index: u32::from(account),
        network: leek_network(wallet.chain_type())?,
        ufvk: Some(encode_ufvk(ufvk, &wallet.chain_type())),
    };
    Ok((account, meta))
}

/// Builds the authoritative PCZT of the stored proposal and fills in any
/// derivation the board needs that the builder left out.
async fn build_for_board(
    lightclient: &mut LightClient,
    meta: &AccountMeta,
) -> Result<Pczt, LeekCommandError> {
    let pczt = lightclient.pczt_from_stored_proposal().await?;
    let ufvk = meta.ufvk.as_deref().unwrap_or_default();
    Ok(leek_zcash::prepare_for_signer(
        pczt,
        ufvk,
        &meta.seed_fingerprint,
        meta.index,
    )?)
}

/// Proves the board-signed PCZT and stores its transaction as Calculated.
/// The SD and USB paths both end here, and so will QR.
async fn prove_and_store(
    lightclient: &mut LightClient,
    signed: SignResult,
) -> Result<String, LeekCommandError> {
    println!(
        "The board signed {} spend(s); txid {}",
        signed.signatures.len(),
        txid_hex(&signed.txid)
    );
    eprintln!("Proving ...");
    let proven =
        tokio::task::spawn_blocking(move || zingolib::wallet::pczt::prove(signed.signed_pczt))
            .await??;
    let txids = lightclient.wallet().write().await.finish_pczt(proven)?;
    Ok(json::object! {
        "txids" => txids.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "next" => "run `transmit` to broadcast",
    }
    .pretty(2))
}

fn txid_hex(txid: &[u8; 32]) -> String {
    txid.iter()
        .rev()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Ends the session on any failure after the PCZT was built, so the
/// proposal's inputs are not left reserved.
async fn abandon_on_error<T>(
    lightclient: &mut LightClient,
    account: zip32::AccountId,
    result: Result<T, LeekCommandError>,
) -> Result<T, LeekCommandError> {
    if result.is_err() {
        let _ = lightclient.wallet().write().await.abandon_pczt(account);
    }
    result
}

/// Pairs with the board on `port` (the six digits are always compared) and
/// makes sure it is unlocked.
pub(crate) fn connect(port: &str) -> Result<Device, LeekError> {
    let mut device = Device::open(port).map_err(|e| leek_zcash::classify(&e))?;
    device
        .pair(|code| {
            println!("Pairing code: {code}");
            println!("Check that the board shows the same six digits, then press ALLOW on it.");
        })
        .map_err(|e| leek_zcash::classify(&e))?;
    device
        .ensure_unlocked()
        .map_err(|e| leek_zcash::classify(&e))?;
    if leek_zcash::get_features(&mut device).is_ok_and(|f| f.firmware_outdated()) {
        println!("{}", leek_zcash::BOARD_UPDATE_TEXT);
    }
    Ok(device)
}

fn view(compact: bool) -> SignerView {
    if compact {
        SignerView::Compact
    } else {
        SignerView::Full
    }
}

async fn sign_usb(
    lightclient: &mut LightClient,
    port: String,
    compact: bool,
) -> Result<String, LeekCommandError> {
    let (account, meta) = signer_account(lightclient).await?;
    let pczt = build_for_board(lightclient, &meta).await;
    let signed = async {
        let pczt = pczt?;
        // The board link blocks for as long as the user reviews.
        Ok(tokio::task::spawn_blocking(move || {
            let mut device = connect(&port)?;
            println!("Review the transaction on the board; accepting on its last page signs.");
            leek_zcash::sign_pczt(
                &mut device,
                &pczt,
                &meta,
                SignOptions {
                    view: view(compact),
                    verify_account: true,
                    ..SignOptions::default()
                },
            )
        })
        .await??)
    }
    .await;
    let stored = match signed {
        Ok(signed) => prove_and_store(lightclient, signed).await,
        Err(failure) => Err(failure),
    };
    abandon_on_error(lightclient, account, stored).await
}

fn pending_path(lightclient: &LightClient) -> Result<PathBuf, LeekCommandError> {
    Ok(lightclient.wallet_dir()?.join(PENDING_PCZT))
}

/// Writes the board's file to the card, and the authoritative PCZT to
/// `pending` on the host.
fn write_card(
    pczt: Pczt,
    meta: &AccountMeta,
    card: &Path,
    pending: &Path,
    compact: bool,
) -> Result<String, LeekCommandError> {
    let request = leek_zcash::prepare_sd_sign(&pczt, meta, view(compact))?;
    // QR hook: a UR encoder takes `request.bytes` here instead of the card,
    // and its decoder hands the `.zsig` bytes to `finish_sd_sign`
    // (`read_card`), so both paths end in `prove_and_store`.
    let unsigned = card.join(request.file_name);
    let signed = card.join(leek_zcash::SD_SIGNED_ZSIG);
    std::fs::create_dir_all(card.join(leek_zcash::SD_DIR)).map_err(file_error(card))?;
    // A stale answer from an earlier session must not be taken for this one.
    match std::fs::remove_file(&signed) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(file_error(&signed)(e)),
        _ => Ok(()),
    }?;
    std::fs::write(&unsigned, &request.bytes).map_err(file_error(&unsigned))?;
    let full = zingolib::wallet::pczt::encode(pczt)?;
    std::fs::write(pending, full).map_err(file_error(pending))?;
    Ok(json::object! {
        "unsigned" => unsigned.display().to_string(),
        "sha256" => hex::encode(request.sent_hash),
        "account" => request.account,
        "network" => request.network.as_str(),
        "next" => "on the board: SD Card > Sign transaction, then `pczt import`",
    }
    .pretty(2))
}

/// Applies the board's `.zsig` on the card to the authoritative PCZT in
/// `pending`, every signature verified against the bytes the board read.
fn read_card(card: &Path, pending: &Path) -> Result<SignResult, LeekCommandError> {
    let full = match std::fs::read(pending) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LeekCommandError::NoPendingSession);
        }
        read => read.map_err(file_error(pending))?,
    };
    let unsigned = card.join(leek_zcash::SD_UNSIGNED_PCZT);
    let sent = std::fs::read(&unsigned).map_err(file_error(&unsigned))?;
    let zsig_path = card.join(leek_zcash::SD_SIGNED_ZSIG);
    let zsig = std::fs::read(&zsig_path).map_err(file_error(&zsig_path))?;
    let pczt = zingolib::wallet::pczt::decode(&full)?;
    Ok(leek_zcash::finish_sd_sign(&pczt, &sent, &zsig)?)
}

async fn export(
    lightclient: &mut LightClient,
    card: PathBuf,
    compact: bool,
) -> Result<String, LeekCommandError> {
    let (account, meta) = signer_account(lightclient).await?;
    let pending = pending_path(lightclient)?;
    let written = match build_for_board(lightclient, &meta).await {
        Ok(pczt) => write_card(pczt, &meta, &card, &pending, compact),
        Err(failure) => Err(failure),
    };
    abandon_on_error(lightclient, account, written).await
}

async fn import(lightclient: &mut LightClient, card: PathBuf) -> Result<String, LeekCommandError> {
    let pending = pending_path(lightclient)?;
    let signed = read_card(&card, &pending)?;
    let stored = prove_and_store(lightclient, signed).await?;
    std::fs::remove_file(&pending).map_err(file_error(&pending))?;
    Ok(stored)
}

async fn abandon(lightclient: &mut LightClient) -> Result<String, LeekCommandError> {
    let (account, _) = signer_account(lightclient).await?;
    let released = lightclient
        .wallet()
        .write()
        .await
        .abandon_pczt(account)
        .map_err(|e| LeekCommandError::Client(LightClientError::from(e)))?;
    let pending = pending_path(lightclient)?;
    match std::fs::remove_file(&pending) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(file_error(&pending)(e)),
        _ => Ok(json::object! { "released_locks" => released }.pretty(2)),
    }
}

pub(crate) async fn pczt(
    sub: PcztSubCommand,
    lightclient: &mut LightClient,
) -> Result<String, LeekCommandError> {
    match sub {
        PcztSubCommand::SignUsb { port, compact } => sign_usb(lightclient, port, compact).await,
        PcztSubCommand::Export { card, compact } => export(lightclient, card, compact).await,
        PcztSubCommand::Import { card } => import(lightclient, card).await,
        PcztSubCommand::Abandon => abandon(lightclient).await,
    }
}

/// The account a new wallet imports from the board: its UFVK, the ZIP 32
/// derivation, and the birthday the board holds (or `birthday`).
pub(crate) struct BoardAccount {
    pub(crate) ufvk: String,
    pub(crate) seed_fingerprint: [u8; 32],
    pub(crate) account: u32,
    pub(crate) birthday: Option<u32>,
}

/// Pairs with the board and reads `account`'s viewing key, which the board
/// shares only after the user confirms there.
pub(crate) fn board_account(
    port: &str,
    chain: ChainType,
    account: u32,
) -> Result<BoardAccount, LeekCommandError> {
    let network = leek_network(chain)?;
    let mut device = connect(port)?;
    println!("Confirm on the board to share account {account}'s viewing key.");
    let exported = leek_zcash::get_account(&mut device, account, network)?;
    println!("Board account {account}: {}", exported.ua);
    Ok(BoardAccount {
        ufvk: exported.ufvk,
        seed_fingerprint: exported.seed_fingerprint,
        account,
        birthday: exported.birthday,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::commands::RT;
    use zingolib::testutils::synthetic_wallet::SyntheticWalletBuilder;

    fn run(client: &mut LightClient, line: &str) -> String {
        let tokens: Vec<String> = line.split_whitespace().map(String::from).collect();
        let command = crate::commands::parse_command_tokens(&tokens).expect("the line parses");
        RT.block_on(crate::commands::dispatch_parsed(command, client))
            .unwrap_or_else(|e| panic!("`{line}` fails: {e}"))
    }

    #[test]
    fn regtest_has_no_board_network() {
        assert!(matches!(
            leek_network(ChainType::Regtest(Default::default())),
            Err(LeekCommandError::UnsupportedChain)
        ));
        assert_eq!(
            leek_network(ChainType::Testnet).unwrap(),
            leek_zcash::Network::Test
        );
    }

    #[test]
    fn txids_print_in_display_order() {
        let mut txid = [0u8; 32];
        txid[0] = 0xab;
        assert!(txid_hex(&txid).ends_with("ab"));
    }

    #[test]
    fn a_seed_wallet_is_not_a_signer_wallet() {
        let client = RT.block_on(LightClient::new_for_test(
            SyntheticWalletBuilder::new(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED).build(),
        ));
        assert!(matches!(
            RT.block_on(signer_account(&client)),
            Err(LeekCommandError::NotExternalSigner)
        ));
    }

    /// A software signer standing in for the board, returning what the board
    /// returns: a `.zsig` of the Orchard and Ironwood spend signatures over
    /// the exact bytes it read. Dev only; it checks nothing it signs.
    fn soft_sign(sent: &[u8], ask: &orchard::keys::SpendAuthorizingKey) -> Vec<u8> {
        use leek_zcash::leek_device::zsig::{Entry, Pool, Zsig};
        use pczt::roles::signer::{Signer, extract_orchard_spend_auth_signatures};

        let view = Pczt::parse(sent).expect("the card file parses");
        let unsigned = |actions: &[pczt::orchard::Action]| {
            actions
                .iter()
                .enumerate()
                .filter(|(_, action)| action.spend().spend_auth_sig().is_none())
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        let orchard_todo = unsigned(view.orchard().actions());
        let ironwood_todo = unsigned(view.ironwood().actions());
        let mut signer = Signer::new(view).expect("the signer view signs");
        for &index in &orchard_todo {
            signer
                .sign_orchard(zingolib::utils::system_rng(), index, ask)
                .expect("the account key signs its own spend");
        }
        for &index in &ironwood_todo {
            signer
                .sign_ironwood(zingolib::utils::system_rng(), index, ask)
                .expect("the account key signs its own spend");
        }
        let entries = extract_orchard_spend_auth_signatures(&signer.finish())
            .into_iter()
            .filter_map(|signature| {
                let (pool, todo) = match signature.value_pool() {
                    orchard::ValuePool::Orchard => (Pool::Orchard, &orchard_todo),
                    orchard::ValuePool::Ironwood => (Pool::Ironwood, &ironwood_todo),
                };
                todo.contains(&signature.action_index()).then(|| {
                    Entry::shielded(
                        pool,
                        signature.action_index() as u32,
                        *signature.signature(),
                    )
                })
            })
            .collect();
        Zsig::new(sent, entries)
            .to_bytes()
            .expect("the .zsig encodes")
    }

    /// The SD-card round trip through zingolib#3147's PCZT and the SDK's
    /// checks: build, preflight and write the card, sign, apply the `.zsig`
    /// (every signature verified against `rk`), prove, and store as
    /// Calculated. The synthetic wallet is regtest, whose coin type is
    /// testnet's, so the account is described as a testnet one here; the
    /// command itself refuses regtest. The transaction must also expire in
    /// the ordinary window after NU7, not at the ZIP 203 limit.
    #[test]
    fn the_sd_card_round_trip_stores_a_calculated_transaction() {
        use zcash_keys::keys::UnifiedSpendingKey;
        use zingolib::wallet::keys::unified::encode_ufvk;

        let mut client = RT.block_on(LightClient::new_for_test(
            SyntheticWalletBuilder::new(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
                .tip(20)
                .orchard_note(1_000_000)
                .ironwood_note(70_000)
                .build(),
        ));
        let chain = client.chain_type();
        let seed = bip0039::Mnemonic::<bip0039::English>::from_phrase(
            zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED,
        )
        .expect("the fixture phrase parses")
        .to_seed("");
        let fingerprint = zip32::fingerprint::SeedFingerprint::from_seed(&seed)
            .expect("a 64-byte seed fingerprints");
        let usk = UnifiedSpendingKey::from_seed(&chain, &seed, zip32::AccountId::ZERO)
            .expect("the fixture seed derives a spending key");
        RT.block_on(client.wallet().write())
            .account_derivations
            .insert(
                zip32::AccountId::ZERO,
                zcash_client_backend::data_api::Zip32Derivation::new(
                    fingerprint,
                    zip32::AccountId::ZERO,
                ),
            );
        let meta = AccountMeta {
            seed_fingerprint: fingerprint.to_bytes(),
            index: 0,
            network: leek_zcash::Network::Test,
            // Encoded for testnet, which shares regtest's coin type and keys.
            ufvk: Some(encode_ufvk(
                &usk.to_unified_full_viewing_key(),
                &ChainType::Testnet,
            )),
        };

        let address = run(&mut client, "new_address o");
        let address = json::parse(&address).unwrap()["encoded_address"]
            .as_str()
            .unwrap()
            .to_string();
        run(&mut client, &format!("send {address} 10000"));

        let pczt = RT
            .block_on(build_for_board(&mut client, &meta))
            .expect("the stored proposal builds a PCZT the board would take");
        let card = tempfile::tempdir().unwrap();
        let pending = card.path().join(PENDING_PCZT);
        write_card(pczt, &meta, card.path(), &pending, false).expect("the card is written");

        let sent = std::fs::read(card.path().join(leek_zcash::SD_UNSIGNED_PCZT)).unwrap();
        let ask = orchard::keys::SpendAuthorizingKey::from(usk.orchard());
        std::fs::write(
            card.path().join(leek_zcash::SD_SIGNED_ZSIG),
            soft_sign(&sent, &ask),
        )
        .unwrap();

        let signed = read_card(card.path(), &pending).expect("the .zsig verifies and applies");
        let txid = signed.txid;
        let stored = RT
            .block_on(prove_and_store(&mut client, signed))
            .expect("it proves and stores");
        assert!(stored.contains(&txid_hex(&txid)), "{stored}");

        let wallet = RT.block_on(client.wallet().read());
        let transaction = wallet
            .wallet_transactions
            .get(&zcash_protocol::TxId::from_bytes(txid))
            .expect("the transaction is stored");
        assert!(matches!(
            transaction.status(),
            zingo_status::confirmation_status::ConfirmationStatus::Calculated(_)
        ));
        let expiry = u32::from(transaction.transaction().expiry_height());
        assert!(
            expiry <= 21 + zingolib::wallet::expiry::NU7_TX_EXPIRY_DELTA,
            "expiry {expiry} must be the ordinary window, not the ZIP 203 limit"
        );
    }
}
