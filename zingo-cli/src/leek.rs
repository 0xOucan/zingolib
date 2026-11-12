//! LeekWallet hardware signing for zingo-cli (cargo feature `leekwallet`).
//!
//! Prototype by 0xOucan on top of zingolib#3147 (dorianvp), which provides
//! the generic external-signer wallet and the PCZT build, prove and store
//! steps used here. This module only drives the LeekWallet board through the
//! LeekWallet SDK (`leek-zcash`, `leek-device`):
//!
//! - `pczt sign_usb` / `pczt sign_ble`: the stored proposal's PCZT is signed
//!   on the board over USB serial or Bluetooth LE, proved, and stored as
//!   Calculated; `transmit` broadcasts it.
//! - `pczt sign_qr` / `pczt import_qr`: the same over the QR air gap, the
//!   signer view out as an animated `ur:leek-pczt`, the board's `.zsig` back
//!   as `ur:leek-zsig`.
//! - `pczt export` / `pczt import`: the same seam over files, for the SD
//!   card (`LEEKZ/unsigned.pczt` out, `LEEKZ/signed.zsig` back).
//! - `pczt abandon`: ends a signing session and releases its inputs.
//!
//! Every USB and Bluetooth session pairs afresh: the board and the terminal
//! show the same six digits and the user presses ALLOW on the board. No host
//! is remembered. QR and the SD card carry no pairing: nothing but the
//! signer view and the signatures crosses them.

mod qr;

use std::path::{Path, PathBuf};
use std::time::Duration;

use leek_zcash::leek_device::ble;
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
/// The signer view a QR session showed the board, beside [`PENDING_PCZT`]:
/// the board's `.zsig` must name its hash.
const PENDING_QR_VIEW: &str = "leek-pending-qr.view";
/// How long `--camera` looks for the board's reply.
#[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
const CAMERA_TIMEOUT: Duration = Duration::from_secs(180);

/// Where the board's QR reply comes from.
#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
pub(crate) struct QrReply {
    /// A file with the board's ur:leek-zsig parts, one per line in any order,
    /// or the .zsig itself. Without it the parts are pasted here.
    #[arg(long, value_name = "FILE")]
    reply: Option<PathBuf>,
    /// Read the board's reply off this V4L2 webcam.
    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
    #[arg(
        long,
        value_name = "DEVICE",
        num_args = 0..=1,
        default_missing_value = "/dev/video0",
        conflicts_with = "reply"
    )]
    camera: Option<PathBuf>,
}

fn parse_fragment(s: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .ok()
        .filter(|bytes| (leek_ur::MIN_FRAGMENT_LEN..=leek_ur::MAX_FRAGMENT_LEN).contains(bytes))
        .ok_or_else(|| {
            format!(
                "a fragment is {} to {} bytes",
                leek_ur::MIN_FRAGMENT_LEN,
                leek_ur::MAX_FRAGMENT_LEN
            )
        })
}

/// How long a Bluetooth scan looks by default, as in the LeekWallet app.
pub(crate) const DEFAULT_BLE_SCAN: Duration = ble::DEFAULT_SCAN;

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
    #[command(about = "Sign the stored proposal on a LeekWallet board over Bluetooth LE")]
    SignBle {
        /// The board's Bluetooth address; needed only when more than one
        /// board is advertising (the error then lists them).
        #[arg(long = "ble-addr", value_name = "ADDR")]
        ble_addr: Option<String>,
        /// How long to scan for the board, in seconds (1 to 60).
        #[arg(long = "ble-scan", value_name = "SECS", default_value = "4", value_parser = crate::parse_scan_secs)]
        ble_scan: Duration,
        /// Send the compact signer view (about half the bytes, fewer chunks).
        #[arg(long)]
        compact: bool,
    },
    #[command(
        about = "Sign the stored proposal on a LeekWallet board over QR: show an animated \
                 ur:leek-pczt, read back its ur:leek-zsig"
    )]
    SignQr {
        /// Also write a self-contained HTML page that loops the code, for a
        /// browser (no network).
        #[arg(long, value_name = "FILE")]
        html: Option<PathBuf>,
        /// Show the code only in the HTML page, not in this terminal.
        #[arg(long = "no-terminal", requires = "html")]
        no_terminal: bool,
        /// Largest UR fragment in bytes: smaller is bigger modules and more
        /// frames.
        #[arg(long, value_name = "BYTES", default_value_t = qr::DEFAULT_FRAGMENT, value_parser = parse_fragment)]
        fragment: usize,
        /// Milliseconds each frame stays up.
        #[arg(
            long,
            value_name = "MS",
            default_value_t = qr::DEFAULT_INTERVAL_MS,
            value_parser = clap::value_parser!(u32).range(i64::from(*qr::INTERVAL_RANGE.start())..=i64::from(*qr::INTERVAL_RANGE.end()))
        )]
        interval: u32,
        #[command(flatten)]
        reply: QrReply,
    },
    #[command(about = "Apply the board's ur:leek-zsig to the pending QR session, prove, and store")]
    ImportQr {
        #[command(flatten)]
        reply: QrReply,
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
         --signer-fingerprint and --signer-account, or with --leek-port or --leek-ble"
    )]
    NotExternalSigner,
    #[error("LeekWallet signs on mainnet and testnet only")]
    UnsupportedChain,
    #[error("no exported signing session: run `pczt export` or `pczt sign_qr` first")]
    NoPendingSession,
    #[error("Bluetooth: {0}")]
    Ble(String),
    #[error(transparent)]
    Qr(#[from] qr::QrError),
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

/// How zingo-cli reaches the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    /// The board's USB serial port.
    Usb(String),
    /// Bluetooth LE: the board at `addr`, or the only one advertising.
    Ble {
        addr: Option<String>,
        scan: Duration,
    },
}

/// - Opens the serial port, or scans for and connects to the board over
///   Bluetooth.
fn open_link(link: &Link) -> Result<Device, LeekCommandError> {
    match link {
        Link::Usb(port) => Device::open(port).map_err(|e| leek_zcash::classify(&e).into()),
        Link::Ble { addr, scan } => {
            let id = match addr {
                Some(addr) => addr.trim().to_string(),
                None => {
                    println!(
                        "Scanning {} s for LeekWallet boards over Bluetooth ...",
                        scan.as_secs()
                    );
                    let found = ble::scan(*scan).map_err(ble_error)?;
                    let board = ble::pick(&found, None).map_err(ble_error)?;
                    println!("Found{}", ble::describe(board));
                    board.id.clone()
                }
            };
            println!("Connecting to {id} over Bluetooth ...");
            Device::open_ble(&id, *scan).map_err(ble_error)
        }
    }
}

// leek-device's Bluetooth errors already say what to do (the adapter, a
// permission, Settings → Link → Bluetooth on the board).
fn ble_error(error: impl std::fmt::Display) -> LeekCommandError {
    LeekCommandError::Ble(format!("{error:#}"))
}

/// Pairs with the board over `link` (the six digits are always compared)
/// and makes sure it is unlocked.
pub(crate) fn connect(link: &Link) -> Result<Device, LeekCommandError> {
    let mut device = open_link(link)?;
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

async fn sign_on_link(
    lightclient: &mut LightClient,
    link: Link,
    compact: bool,
) -> Result<String, LeekCommandError> {
    let (account, meta) = signer_account(lightclient).await?;
    let pczt = build_for_board(lightclient, &meta).await;
    let signed = async {
        let pczt = pczt?;
        // The board link blocks for as long as the user reviews.
        tokio::task::spawn_blocking(move || {
            let mut device = connect(&link)?;
            println!("Review the transaction on the board; accepting on its last page signs.");
            Ok::<_, LeekCommandError>(leek_zcash::sign_pczt(
                &mut device,
                &pczt,
                &meta,
                SignOptions {
                    view: view(compact),
                    verify_account: true,
                    ..SignOptions::default()
                },
            )?)
        })
        .await?
    }
    .await;
    let stored = match signed {
        Ok(signed) => prove_and_store(lightclient, signed).await,
        Err(failure) => Err(failure),
    };
    abandon_on_error(lightclient, account, stored).await
}

/// The files of an air-gapped session, beside the wallet file.
struct Pending {
    pczt: PathBuf,
    qr_view: PathBuf,
}

impl Pending {
    fn in_dir(dir: &Path) -> Self {
        Self {
            pczt: dir.join(PENDING_PCZT),
            qr_view: dir.join(PENDING_QR_VIEW),
        }
    }

    /// - Deletes both files; an absent file is no error.
    fn remove(&self) -> Result<(), LeekCommandError> {
        [&self.pczt, &self.qr_view]
            .into_iter()
            .try_for_each(|path| match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(file_error(path)(e)),
                _ => Ok(()),
            })
    }
}

fn pending_paths(lightclient: &LightClient) -> Result<Pending, LeekCommandError> {
    Ok(Pending::in_dir(&lightclient.wallet_dir()?))
}

/// - Reads `path`; a missing file is [`LeekCommandError::NoPendingSession`].
fn read_pending(path: &Path) -> Result<Vec<u8>, LeekCommandError> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(LeekCommandError::NoPendingSession)
        }
        read => read.map_err(file_error(path)),
    }
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
    let full = read_pending(pending)?;
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
    let pending = pending_paths(lightclient)?;
    let written = match build_for_board(lightclient, &meta).await {
        Ok(pczt) => write_card(pczt, &meta, &card, &pending.pczt, compact),
        Err(failure) => Err(failure),
    };
    abandon_on_error(lightclient, account, written).await
}

async fn import(lightclient: &mut LightClient, card: PathBuf) -> Result<String, LeekCommandError> {
    let pending = pending_paths(lightclient)?;
    let signed = read_card(&card, &pending.pczt)?;
    let stored = prove_and_store(lightclient, signed).await?;
    pending.remove()?;
    Ok(stored)
}

/// The options of the code `pczt sign_qr` shows.
struct QrShow {
    html: Option<PathBuf>,
    terminal: bool,
    fragment: usize,
    interval: u32,
}

/// - Writes the authoritative PCZT and the compact signer view the board is
///   shown to `pending`.
fn stage_qr(
    pczt: Pczt,
    meta: &AccountMeta,
    pending: &Pending,
    fragment: usize,
) -> Result<leek_ur::Encoder, LeekCommandError> {
    // The board reads the same signer view over QR as over a link; the
    // compact one is about half the frames.
    let request = leek_zcash::prepare_sd_sign(&pczt, meta, SignerView::Compact)?;
    let encoder =
        leek_ur::Encoder::leek_pczt(&request.bytes, fragment).map_err(qr::QrError::from)?;
    std::fs::write(&pending.qr_view, &request.bytes).map_err(file_error(&pending.qr_view))?;
    let full = zingolib::wallet::pczt::encode(pczt)?;
    std::fs::write(&pending.pczt, full).map_err(file_error(&pending.pczt))?;
    println!(
        "QR signing for account {} on {}: ur:leek-pczt, {} B in {} fragment(s) of {} B, sha256 {}.",
        request.account,
        request.network.as_str(),
        request.bytes.len(),
        encoder.seq_len(),
        encoder.fragment_len(),
        hex::encode(request.sent_hash),
    );
    Ok(encoder)
}

/// - Writes the HTML page, and draws the code in the terminal until Enter.
fn show_qr(encoder: &leek_ur::Encoder, show: &QrShow) -> Result<(), LeekCommandError> {
    let frames = qr::frames(encoder)?;
    if let Some(path) = &show.html {
        qr::write_html(&frames, encoder, show.interval, path).map_err(file_error(path))?;
        println!(
            "QR page -> {}: open it in a browser, full screen.",
            path.display()
        );
    }
    if show.terminal {
        qr::show_terminal(&frames, show.interval)?;
    }
    Ok(())
}

/// - Reads the reply from the camera, the `--reply` file, or stdin.
fn read_qr_reply(reply: &QrReply) -> Result<Option<Vec<u8>>, LeekCommandError> {
    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
    if let Some(device) = &reply.camera {
        return Ok(Some(qr::camera::scan(device, CAMERA_TIMEOUT)?));
    }
    match &reply.reply {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(file_error(path))?;
            Ok(Some(qr::zsig_from_reply(&bytes)?))
        }
        None => Ok(qr::zsig_from_paste()?),
    }
}

/// - Reads the pending PCZT and shown view.
fn apply_qr_reply(pending: &Pending, zsig: &[u8]) -> Result<SignResult, LeekCommandError> {
    let full = read_pending(&pending.pczt)?;
    let shown = read_pending(&pending.qr_view)?;
    let pczt = zingolib::wallet::pczt::decode(&full)?;
    Ok(leek_zcash::finish_sd_sign(&pczt, &shown, zsig)?)
}

/// - Reads the board's reply (blocking, off the runtime), then proves and
///   stores, deleting the pending files.
/// - A reply that is not given keeps the session for `pczt import_qr`.
async fn finish_qr(
    lightclient: &mut LightClient,
    reply: QrReply,
) -> Result<String, LeekCommandError> {
    let pending = pending_paths(lightclient)?;
    if !pending.qr_view.exists() {
        return Err(LeekCommandError::NoPendingSession);
    }
    let Some(zsig) = tokio::task::spawn_blocking(move || read_qr_reply(&reply)).await?? else {
        return Ok(json::object! {
            "pending" => pending.qr_view.display().to_string(),
            "next" => "`pczt import_qr [--reply <file>]` with the board's reply, or `pczt abandon`",
        }
        .pretty(2));
    };
    let signed = apply_qr_reply(&pending, &zsig)?;
    let stored = prove_and_store(lightclient, signed).await?;
    pending.remove()?;
    Ok(stored)
}

async fn sign_qr(
    lightclient: &mut LightClient,
    show: QrShow,
    reply: QrReply,
) -> Result<String, LeekCommandError> {
    let (account, meta) = signer_account(lightclient).await?;
    let pending = pending_paths(lightclient)?;
    let staged = match build_for_board(lightclient, &meta).await {
        Ok(pczt) => stage_qr(pczt, &meta, &pending, show.fragment),
        Err(failure) => Err(failure),
    };
    let encoder = abandon_on_error(lightclient, account, staged).await?;
    // From here the session stays on failure: the board may already hold
    // the view, and `pczt import_qr` or `pczt abandon` ends it.
    tokio::task::spawn_blocking(move || show_qr(&encoder, &show)).await??;
    finish_qr(lightclient, reply).await
}

async fn abandon(lightclient: &mut LightClient) -> Result<String, LeekCommandError> {
    let (account, _) = signer_account(lightclient).await?;
    let released = lightclient
        .wallet()
        .write()
        .await
        .abandon_pczt(account)
        .map_err(|e| LeekCommandError::Client(LightClientError::from(e)))?;
    pending_paths(lightclient)?.remove()?;
    Ok(json::object! { "released_locks" => released }.pretty(2))
}

pub(crate) async fn pczt(
    sub: PcztSubCommand,
    lightclient: &mut LightClient,
) -> Result<String, LeekCommandError> {
    match sub {
        PcztSubCommand::SignUsb { port, compact } => {
            sign_on_link(lightclient, Link::Usb(port), compact).await
        }
        PcztSubCommand::SignBle {
            ble_addr,
            ble_scan,
            compact,
        } => {
            let link = Link::Ble {
                addr: ble_addr,
                scan: ble_scan,
            };
            sign_on_link(lightclient, link, compact).await
        }
        PcztSubCommand::SignQr {
            html,
            no_terminal,
            fragment,
            interval,
            reply,
        } => {
            let show = QrShow {
                html,
                terminal: !no_terminal,
                fragment,
                interval,
            };
            sign_qr(lightclient, show, reply).await
        }
        PcztSubCommand::ImportQr { reply } => finish_qr(lightclient, reply).await,
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
    link: &Link,
    chain: ChainType,
    account: u32,
) -> Result<BoardAccount, LeekCommandError> {
    let network = leek_network(chain)?;
    let mut device = connect(link)?;
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

    /// A synthetic wallet with an Orchard and an Ironwood note, given an
    /// external-signer derivation, with a stored proposal to its own Orchard
    /// address; the account as the board would describe it, and the key the
    /// software signer signs with. The synthetic wallet is regtest, whose coin
    /// type is testnet's, so the account is described as a testnet one; the
    /// commands themselves refuse regtest.
    fn signer_wallet_with_proposal()
    -> (LightClient, AccountMeta, orchard::keys::SpendAuthorizingKey) {
        use zcash_keys::keys::UnifiedSpendingKey;

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
        let address = json::parse(&address).expect("new_address prints JSON")["encoded_address"]
            .as_str()
            .expect("new_address prints the encoded address")
            .to_string();
        run(&mut client, &format!("send {address} 10000"));
        let ask = orchard::keys::SpendAuthorizingKey::from(usk.orchard());
        (client, meta, ask)
    }

    /// Proves and stores `signed`, then checks the stored transaction is
    /// Calculated and expires in the ordinary window after NU7, not at the
    /// ZIP 203 limit.
    fn assert_stored_calculated(client: &mut LightClient, signed: SignResult) {
        let txid = signed.txid;
        let stored = RT
            .block_on(prove_and_store(client, signed))
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

    /// The SD-card round trip through zingolib#3147's PCZT and the SDK's
    /// checks: build, preflight and write the card, sign, apply the `.zsig`
    /// (every signature verified against `rk`), prove, and store as
    /// Calculated.
    #[test]
    fn the_sd_card_round_trip_stores_a_calculated_transaction() {
        let (mut client, meta, ask) = signer_wallet_with_proposal();
        let pczt = RT
            .block_on(build_for_board(&mut client, &meta))
            .expect("the stored proposal builds a PCZT the board would take");
        let card = tempfile::tempdir().expect("a temporary directory");
        let pending = card.path().join(PENDING_PCZT);
        write_card(pczt, &meta, card.path(), &pending, false).expect("the card is written");

        let sent = std::fs::read(card.path().join(leek_zcash::SD_UNSIGNED_PCZT))
            .expect("the card holds the signer view");
        std::fs::write(
            card.path().join(leek_zcash::SD_SIGNED_ZSIG),
            soft_sign(&sent, &ask),
        )
        .expect("the card takes the .zsig");

        let signed = read_card(card.path(), &pending).expect("the .zsig verifies and applies");
        assert_stored_calculated(&mut client, signed);
    }

    /// The QR round trip: the compact signer view staged and encoded as an
    /// animated ur:leek-pczt, decoded as the board's camera would, signed,
    /// sent back as ur:leek-zsig parts (out of order, with our own frames
    /// reflected in), decoded, verified against the shown view, proved and
    /// stored as Calculated.
    #[test]
    fn the_qr_round_trip_stores_a_calculated_transaction() {
        let (mut client, meta, ask) = signer_wallet_with_proposal();
        let pczt = RT
            .block_on(build_for_board(&mut client, &meta))
            .expect("the stored proposal builds a PCZT the board would take");
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pending = Pending::in_dir(dir.path());
        let encoder =
            stage_qr(pczt, &meta, &pending, qr::DEFAULT_FRAGMENT).expect("the view is staged");
        assert!(encoder.seq_len() > 1, "a signer view takes several frames");

        let shown: Vec<String> = (0..u64::from(encoder.seq_len()) * 2)
            .map(|i| encoder.frame(i))
            .collect();
        let view = leek_ur::decode_bytes(leek_ur::UR_PCZT, shown.iter().map(String::as_str))
            .expect("the board's camera assembles the view");
        assert_eq!(
            view,
            std::fs::read(&pending.qr_view).expect("the shown view is kept")
        );

        let zsig = soft_sign(&view, &ask);
        let reply = leek_ur::Encoder::leek_zsig(&zsig, 40).expect("the .zsig encodes");
        let mut lines: Vec<String> = (0..u64::from(reply.seq_len()) + 2)
            .map(|i| reply.frame(i))
            .collect();
        lines.reverse();
        lines.insert(1, shown[0].clone());
        let read =
            qr::zsig_from_parts(lines.iter().map(String::as_str)).expect("the reply assembles");
        assert_eq!(read, zsig);

        let signed = apply_qr_reply(&pending, &read).expect("the .zsig verifies and applies");
        assert_stored_calculated(&mut client, signed);
        pending.remove().expect("the pending files go");
        assert!(!pending.pczt.exists() && !pending.qr_view.exists());
    }

    /// A reply signed over other bytes than the shown view is refused before
    /// anything is applied.
    #[test]
    fn a_qr_reply_for_another_view_is_refused() {
        let (mut client, meta, ask) = signer_wallet_with_proposal();
        let pczt = RT
            .block_on(build_for_board(&mut client, &meta))
            .expect("the stored proposal builds a PCZT the board would take");
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pending = Pending::in_dir(dir.path());
        stage_qr(pczt, &meta, &pending, qr::DEFAULT_FRAGMENT).expect("the view is staged");
        let mut view = std::fs::read(&pending.qr_view).expect("the shown view is kept");
        let zsig = soft_sign(&view, &ask);
        view.push(0);
        std::fs::write(&pending.qr_view, &view).expect("the view is rewritten");
        assert!(matches!(
            apply_qr_reply(&pending, &zsig),
            Err(LeekCommandError::Leek(LeekError::ZsigHashMismatch))
        ));
    }

    #[test]
    fn no_pending_qr_session_is_reported() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(
            apply_qr_reply(&Pending::in_dir(dir.path()), &[]),
            Err(LeekCommandError::NoPendingSession)
        ));
    }

    fn parse_pczt(line: &str) -> Result<PcztSubCommand, String> {
        let tokens: Vec<String> = line.split_whitespace().map(String::from).collect();
        match crate::commands::parse_command_tokens(&tokens)? {
            crate::commands::CliCommand::Pczt { sub } => Ok(sub),
            other => Err(format!("not a pczt command: {}", other.name())),
        }
    }

    #[test]
    fn sign_ble_takes_an_address_and_a_scan_window() {
        assert_eq!(
            parse_pczt("pczt sign_ble"),
            Ok(PcztSubCommand::SignBle {
                ble_addr: None,
                ble_scan: ble::DEFAULT_SCAN,
                compact: false,
            })
        );
        assert_eq!(
            parse_pczt("pczt sign_ble --ble-addr 28:84:85:9F:D2:0E --ble-scan 10 --compact"),
            Ok(PcztSubCommand::SignBle {
                ble_addr: Some("28:84:85:9F:D2:0E".to_string()),
                ble_scan: Duration::from_secs(10),
                compact: true,
            })
        );
        assert!(parse_pczt("pczt sign_ble --ble-scan 0").is_err());
        assert!(parse_pczt("pczt sign_ble --ble-scan 61").is_err());
    }

    #[test]
    fn sign_qr_defaults_to_the_laptop_hints() {
        assert_eq!(
            parse_pczt("pczt sign_qr"),
            Ok(PcztSubCommand::SignQr {
                html: None,
                no_terminal: false,
                fragment: qr::DEFAULT_FRAGMENT,
                interval: qr::DEFAULT_INTERVAL_MS,
                reply: QrReply {
                    reply: None,
                    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
                    camera: None,
                },
            })
        );
    }

    #[test]
    fn sign_qr_takes_its_options() {
        assert_eq!(
            parse_pczt(
                "pczt sign_qr --html /tmp/q.html --no-terminal --fragment 30 --interval 500 \
                 --reply /tmp/parts.txt"
            ),
            Ok(PcztSubCommand::SignQr {
                html: Some(PathBuf::from("/tmp/q.html")),
                no_terminal: true,
                fragment: 30,
                interval: 500,
                reply: QrReply {
                    reply: Some(PathBuf::from("/tmp/parts.txt")),
                    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
                    camera: None,
                },
            })
        );
        assert_eq!(
            parse_pczt("pczt import_qr --reply /tmp/parts.txt"),
            Ok(PcztSubCommand::ImportQr {
                reply: QrReply {
                    reply: Some(PathBuf::from("/tmp/parts.txt")),
                    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
                    camera: None,
                },
            })
        );
    }

    #[test]
    fn sign_qr_refuses_out_of_range_options() {
        assert!(parse_pczt("pczt sign_qr --no-terminal").is_err());
        assert!(parse_pczt("pczt sign_qr --fragment 9").is_err());
        assert!(parse_pczt("pczt sign_qr --fragment 513").is_err());
        assert!(parse_pczt("pczt sign_qr --interval 99").is_err());
        assert!(parse_pczt("pczt sign_qr --interval 5001").is_err());
    }

    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
    #[test]
    fn the_camera_defaults_to_video0_and_excludes_a_reply_file() {
        let Ok(PcztSubCommand::ImportQr { reply }) = parse_pczt("pczt import_qr --camera") else {
            panic!("`import_qr --camera` parses");
        };
        assert_eq!(reply.camera, Some(PathBuf::from("/dev/video0")));
        assert!(parse_pczt("pczt import_qr --camera --reply /tmp/p.txt").is_err());
    }
}
