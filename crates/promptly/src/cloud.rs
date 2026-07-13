//! The cloud seam — device pairing, ranked auto-submit, and grading status (`20`).
//!
//! [`HttpCloud`] is the authenticated client for the Promptly web app's device
//! routes. It speaks five contracts, all over the device-token bearer:
//!   - **pair** (`POST /api/devices/pair` + poll `/pair/token`): the OAuth-style
//!     device-authorization flow — the CLI proves it owns a fresh Ed25519 key,
//!     shows the player a short code to confirm in the browser, then polls for the
//!     90-day device token and stores it ([`crate::credentials`]).
//!   - **prepare_attempt** (`POST /api/cli/attempts`): claim the server-issued
//!     attempt nonce the daemon binds into its capture so it can reach `verified`.
//!   - **submit** (`POST /api/cli/submit`): redact-then-package the solution,
//!     sign the captured turn chain ([`crate::signing`]) binding the server-
//!     recomputed `final_code_hash`, and upload for ranked grading.
//!   - **submission_status** (`GET /api/cli/submissions/{id}`): poll the grade,
//!     then compare it to the local best-case projection ([`parity_report`]).
//!   - **run_public_tests** (`POST /api/cli/test`, the [`RemoteTests`] trait):
//!     run a level's public tests on the server's Judge0 backend — `promptly
//!     test`'s fallback when the local toolchain is missing. Feedback only,
//!     never a ranked attempt.
//!
//! The byte-exact cross-system pieces reuse already-cross-checked ports: the
//! `final_code_hash` is the daemon's `baseline` port (equal to the server's
//! `computeBaselineHash`), and the signed chain is `signing.rs` (pinned to the
//! shared vectors). The pure builders below are unit-tested directly; the trait
//! keeps `submit`/`pair`/`prepare_attempt` testable with in-memory fakes.

use std::collections::BTreeSet;
use std::io::Write;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use promptlyd::baseline::CanonicalFile;
use promptlyd::model::{Agreement, Confidence};

use crate::credentials::{CredentialStore, Credentials};
use crate::daemon_client::NormalizedTurn;
use crate::signing::{self, CaptureSummary, CrossSource, SignedChainWire, TokenCounts, TurnInput};
use crate::submission::SubmissionBundle;

/// Why a cloud operation didn't complete.
#[derive(Debug, Error)]
pub enum CloudError {
    #[error("not signed in — run `promptly pair` first")]
    NotPaired,
    #[error("device pairing and ranked upload ship with cloud pairing (subplan 20)")]
    Unavailable,
    #[error("couldn't reach Promptly at {0} — pass --api-url or set PROMPTLY_API_URL")]
    NotReachable(String),
    #[error("pairing wasn't approved in time — run `promptly pair` again")]
    PairingTimedOut,
    #[error("stored device credentials are unusable ({0}) — run `promptly pair` again")]
    BadCredentials(String),
    #[error("{0}")]
    Http(String),
    /// The server answered but doesn't serve this route at all (a bare 404) —
    /// it predates the endpoint. Distinct from [`CloudError::Http`] so the CLI
    /// can say "not available on this server yet" instead of a raw HTTP error.
    #[error("this server doesn't support this operation yet")]
    UnsupportedEndpoint,
    #[error("cloud error: {0}")]
    Other(String),
}

/// The outcome of a ranked submission (the async grade the server queues).
#[derive(Debug, Clone)]
pub struct SubmitReceipt {
    pub submission_id: String,
    pub status: String,
}

/// The score the server persisted once a submission is graded.
#[derive(Debug, Clone)]
pub struct GradedScore {
    /// The canonical persisted score (higher is better).
    pub score: f64,
    /// Fraction of hidden tests passed, 0–100.
    pub correctness_pct: f64,
    /// False when the model fell back to the baseline-floor tier server-side.
    pub recognized: bool,
}

/// A grading-status poll result: the job status, plus the score once graded.
#[derive(Debug, Clone)]
pub struct RemoteStatus {
    pub status: String,
    pub graded: Option<GradedScore>,
}

impl RemoteStatus {
    /// Whether the job has reached a terminal state (graded or failed).
    pub fn is_terminal(&self) -> bool {
        self.graded.is_some() || self.status == "graded" || self.status == "failed"
    }
}

/// The server's response to preparing an attempt (`20`): the attempt nonce the
/// daemon binds into its signed capture, plus the level's authoritative kit
/// `baseline_hash`. The daemon attests its local (player-editable) manifest against
/// that hash before a fresh start, so a stale or tampered manifest can't anchor a
/// session to a forged starter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedAttempt {
    pub nonce: String,
    /// `None` when the kit isn't authored/attestable yet — the daemon records the
    /// capture as unattested (which caps it at `unverified`) rather than blocking.
    pub baseline_hash: Option<String>,
}

/// The captured session the daemon path uploads: the turns to sign, the attempt
/// nonce they're bound to, and the daemon session id the telemetry came from.
pub struct CaptureUpload<'a> {
    pub turns: &'a [NormalizedTurn],
    /// The attempt's server-issued nonce (the chain binds it). `None` offline —
    /// the server then can't match it and the capture stays `suspect`.
    pub attempt_nonce: Option<&'a str>,
    pub telemetry_session_id: &'a str,
    /// The session-scoped capture summary the chain's terminal entry signs (`20`):
    /// pause accounting, paste/edit provenance, the baseline attestation, and the
    /// nonce origin. Assembled by the caller from the daemon's `/session` state, it
    /// is what the server's trust policy (`25`) reads to decide the verified tier —
    /// and signing it makes every field tamper-evident.
    pub capture_summary: CaptureSummary,
}

/// The cloud operations the CLI drives. `HttpCloud` is the authenticated
/// implementation; the trait keeps the commands testable with in-memory fakes.
pub trait Cloud {
    /// Pair this device: run the device-authorization flow and store the token.
    fn pair(&self) -> Result<(), CloudError>;
    /// Ensure a server-side attempt for `slug` and return its server-issued nonce
    /// plus the level's authoritative kit baseline ([`PreparedAttempt`]) — the nonce
    /// the daemon binds so the capture can reach `verified`, and the baseline it
    /// attests the local manifest against. Returns `Ok(None)` when the device isn't
    /// paired — offline play is first-class and simply caps the attempt at
    /// `unverified`; an `Err` is reserved for a paired device that couldn't reach the
    /// server.
    fn prepare_attempt(&self, slug: &str) -> Result<Option<PreparedAttempt>, CloudError>;
    /// Sign the captured turn chain and upload the packaged submission for ranked
    /// grading; returns the async grading receipt.
    fn submit(
        &self,
        slug: &str,
        bundle: &SubmissionBundle,
        capture: &CaptureUpload,
    ) -> Result<SubmitReceipt, CloudError>;
    /// Poll a submission's grading status.
    fn submission_status(&self, submission_id: &str) -> Result<RemoteStatus, CloudError>;
}

/// The remote public-test seam (`19`): run a level's public tests on the
/// server's Judge0 backend when local execution isn't possible. A separate
/// trait from [`Cloud`] — `test` is the only consumer, and the existing
/// submit/pair fakes shouldn't have to grow a method they never use.
pub trait RemoteTests {
    /// Run `slug`'s public tests against the (already-redacted) bundle and
    /// return the server's per-case verdicts. [`CloudError::NotPaired`] when no
    /// device credentials are stored; [`CloudError::UnsupportedEndpoint`] when
    /// the server predates `POST /api/cli/test`.
    fn run_public_tests(
        &self,
        slug: &str,
        bundle: &SubmissionBundle,
    ) -> Result<RemoteTestReport, CloudError>;
}

/// The server's remote public-test report (`POST /api/cli/test`, 200 body).
/// Only the fields the CLI renders are read; the wire carries more.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteTestReport {
    /// The server's overall verdict: every case ran and passed.
    #[serde(default)]
    pub passed: bool,
    /// The suite crashed before producing per-case verdicts (compile error).
    #[serde(default)]
    pub crashed: bool,
    #[serde(default)]
    pub cases: Vec<RemoteCase>,
    /// Trimmed compiler/setup output, when the run produced any.
    #[serde(default, rename = "compileOutput")]
    pub compile_output: Option<String>,
}

/// One remote case verdict: `status` is the server's string
/// (`passed`/`failed`/`errored`/`missing`), left unparsed so an unknown future
/// status degrades to an error line instead of failing the whole response.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCase {
    pub name: String,
    pub status: String,
    /// A short non-spoiler note (wrong-answer summary, error head, …).
    #[serde(default)]
    pub message: Option<String>,
}

/// The offline cloud: no web app configured / never paired. `prepare_attempt`
/// succeeds with no nonce (offline capture is allowed, capped at `unverified`);
/// the authenticated operations report that pairing is required. Used as the
/// "unpaired" stand-in in tests.
pub struct UnpairedCloud;

impl Cloud for UnpairedCloud {
    fn pair(&self) -> Result<(), CloudError> {
        Err(CloudError::Unavailable)
    }
    fn prepare_attempt(&self, _slug: &str) -> Result<Option<PreparedAttempt>, CloudError> {
        Ok(None)
    }
    fn submit(
        &self,
        _slug: &str,
        _bundle: &SubmissionBundle,
        _capture: &CaptureUpload,
    ) -> Result<SubmitReceipt, CloudError> {
        Err(CloudError::NotPaired)
    }
    fn submission_status(&self, _submission_id: &str) -> Result<RemoteStatus, CloudError> {
        Err(CloudError::NotPaired)
    }
}

// --- Pure builders (unit-tested directly, no network) -----------------------

/// The message a device signs at pairing to prove it owns the registered key.
/// Must match `keyOwnershipMessage` in `lib/devices/turn-chain.ts` byte-for-byte.
fn key_ownership_message(public_key_b64: &str) -> String {
    format!("promptly-device-pairing:v1:{public_key_b64}")
}

/// Base64 Ed25519 proof-of-possession over [`key_ownership_message`].
fn sign_proof(key: &SigningKey, public_key_b64: &str) -> String {
    let message = key_ownership_message(public_key_b64);
    STANDARD.encode(key.sign(message.as_bytes()).to_bytes())
}

/// Generate a fresh device keypair from the OS CSPRNG, returning the signing key
/// and the base64 seed to persist (the public half is uploaded at pairing).
fn generate_device_keypair() -> Result<(SigningKey, String), CloudError> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .map_err(|err| CloudError::Other(format!("secure RNG unavailable: {err}")))?;
    let key = signing::signing_key_from_seed(&seed);
    Ok((key, STANDARD.encode(seed)))
}

/// Rebuild the device signing key from its stored base64 seed.
fn decode_signing_key(seed_b64: &str) -> Result<SigningKey, CloudError> {
    let bytes = STANDARD
        .decode(seed_b64)
        .map_err(|err| CloudError::BadCredentials(err.to_string()))?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| CloudError::BadCredentials("signing seed must be 32 bytes".to_string()))?;
    Ok(signing::signing_key_from_seed(&seed))
}

/// The content hash of the packaged solution — identical to the server's
/// `finalCodeHash` (the same canonical baseline hash, `lib/kits/baseline-hash.ts`),
/// computed via the daemon's bit-for-bit port so the signed chain binds the exact
/// value the server recomputes.
fn final_code_hash(bundle: &SubmissionBundle) -> String {
    let files: Vec<CanonicalFile> = bundle
        .files
        .iter()
        .map(|file| CanonicalFile {
            path: file.path.clone(),
            content: file.bytes.clone(),
        })
        .collect();
    promptlyd::baseline::compute_baseline_hash(&files)
}

/// Summarize the OTEL↔JSONL corroboration across the captured turns for the v2
/// signed terminal entry (`17`/`25`): how many turns the two telemetry sources
/// disagreed on, and the sorted, de-duplicated union of the fields they disagreed
/// about. Signing this into the chain makes the disagreement count tamper-evident
/// — a forked daemon can't quietly zero it without breaking the terminal signature
/// (the server then grades the run `suspect`). Turns observed by a single source
/// (`Agreement::Single`) or in agreement contribute nothing.
fn cross_source_summary(turns: &[NormalizedTurn]) -> CrossSource {
    let mut disagree_turns = 0u32;
    let mut disagree_fields: BTreeSet<String> = BTreeSet::new();
    for turn in turns {
        if let Agreement::Disagree { fields } = &turn.agreement {
            disagree_turns += 1;
            for field in fields {
                disagree_fields.insert(field.clone());
            }
        }
    }
    CrossSource {
        disagree_turns,
        disagree_fields: disagree_fields.into_iter().collect(),
    }
}

/// The signed per-turn confidence tier (`otel`/`jsonl`/`estimated`) — the lowercase
/// name the server's trust policy (`25`) reads to require an OTEL-backed capture.
fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Otel => "otel",
        Confidence::Jsonl => "jsonl",
        Confidence::Estimated => "estimated",
    }
}

/// Sign the captured turns into the wire chain (v3), binding `final_code_hash`, the
/// `cross_source` corroboration summary, and the session `capture_summary`. Each
/// turn also signs its confidence, source set, and timestamp, so the server can
/// decide the trust tier from signed evidence alone. Turn indices are assigned
/// sequentially from 0 — the order the server verifies.
fn signed_chain_for(
    key: &SigningKey,
    nonce: &str,
    turns: &[NormalizedTurn],
    cross_source: &CrossSource,
    capture_summary: &CaptureSummary,
    final_code_hash: &str,
) -> SignedChainWire {
    let inputs: Vec<TurnInput> = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| TurnInput {
            turn_index: index as u32,
            model: turn.model.clone(),
            token_counts: TokenCounts {
                input: turn.tokens_input,
                output: turn.tokens_output,
                thinking: turn.tokens_thinking,
                cache: turn.tokens_cache,
            },
            confidence: confidence_name(turn.confidence).to_string(),
            sources: turn
                .sources
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            timestamp_ms: turn.timestamp_ms,
        })
        .collect();
    signing::sign_chain(
        key,
        nonce,
        &inputs,
        cross_source,
        capture_summary,
        final_code_hash,
    )
}

/// The submission's overall telemetry confidence, taken as the weakest across the
/// captured turns (an `estimated` turn downgrades the whole run; the server scores
/// it against the baseline-floor tier). An empty capture is `estimated`.
fn derive_confidence(turns: &[NormalizedTurn]) -> &'static str {
    let mut seen_jsonl = false;
    for turn in turns {
        match turn.confidence {
            Confidence::Estimated => return "estimated",
            Confidence::Jsonl => seen_jsonl = true,
            Confidence::Otel => {}
        }
    }
    if turns.is_empty() {
        "estimated"
    } else if seen_jsonl {
        "jsonl"
    } else {
        "otel"
    }
}

/// Package the bundle into a STORE-only zip the server's `readUploadedZip` reads.
fn zip_bundle(bundle: &SubmissionBundle) -> Result<Vec<u8>, CloudError> {
    use zip::write::SimpleFileOptions;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for file in &bundle.files {
        zip.start_file(file.path.as_str(), options)
            .map_err(|err| CloudError::Other(err.to_string()))?;
        zip.write_all(&file.bytes)
            .map_err(|err| CloudError::Other(err.to_string()))?;
    }
    Ok(zip
        .finish()
        .map_err(|err| CloudError::Other(err.to_string()))?
        .into_inner())
}

/// How the locally projected score relates to the server's graded score.
#[derive(Debug, Clone)]
pub struct ParityReport {
    /// The local best-case projection (assumes a clear, floored run time).
    pub projected: f64,
    /// The server's persisted score.
    pub graded: f64,
    pub correctness_pct: f64,
    pub recognized: bool,
    /// Set when the graded score exceeds the local best-case projection — a real
    /// scoring-parity violation (the two ports disagree), worth surfacing.
    pub warning: Option<String>,
}

/// Compare the server's graded score to the local best-case projection. The
/// projection assumes a perfect clear with floored run time, so it should
/// **upper-bound** the graded score; the server scoring higher means the Rust and
/// TS scoring ports have drifted out of parity (`13`).
pub fn parity_report(projected: f64, graded: &GradedScore) -> ParityReport {
    // A 1% relative slack absorbs float formatting; a real divergence dwarfs it.
    let warning = (graded.score > projected * 1.01).then(|| {
        format!(
            "server score {:.0} exceeds the local best-case projection {:.0} — scoring may be out of parity",
            graded.score, projected
        )
    });
    ParityReport {
        projected,
        graded: graded.score,
        correctness_pct: graded.correctness_pct,
        recognized: graded.recognized,
        warning,
    }
}

// --- Wire bodies / responses ------------------------------------------------

#[derive(Serialize)]
struct PairStartBody<'a> {
    public_key: &'a str,
    proof: &'a str,
    name: &'a str,
}

#[derive(Deserialize)]
struct PairStartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Serialize)]
struct DeviceCodeBody<'a> {
    device_code: &'a str,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum PairTokenResponse {
    Pending,
    Complete { device_token: String },
}

#[derive(Serialize)]
struct SlugBody<'a> {
    slug: &'a str,
}

#[derive(Deserialize)]
struct AttemptResponse {
    attempt_nonce: String,
    /// The level's authoritative kit `baseline_hash` (`07`), the daemon-side
    /// attestation seam. `null`/absent when the kit isn't attestable yet.
    #[serde(default)]
    baseline_hash: Option<String>,
}

#[derive(Serialize)]
struct Intake {
    mode: &'static str,
    #[serde(rename = "archiveBase64")]
    archive_base64: String,
}

#[derive(Serialize)]
struct SubmitBody<'a> {
    slug: &'a str,
    intake: Intake,
    telemetry_confidence: &'a str,
    telemetry_session_id: &'a str,
    signed_chain: SignedChainWire,
}

#[derive(Serialize)]
struct RemoteTestBody<'a> {
    slug: &'a str,
    intake: Intake,
}

#[derive(Deserialize)]
struct SubmitResponse {
    #[serde(rename = "submissionId")]
    submission_id: String,
    status: String,
}

#[derive(Deserialize)]
struct StatusResponse {
    status: String,
    #[serde(default)]
    graded: Option<GradedResponse>,
}

#[derive(Deserialize)]
struct GradedResponse {
    score: f64,
    #[serde(rename = "correctnessPct")]
    correctness_pct: f64,
    recognized: bool,
}

// --- The authenticated HTTP client ------------------------------------------

/// Hard cap on how long pairing polls for the player's approval before giving up,
/// independent of the server's `expires_in` (a safety bound on the loop).
const MAX_PAIRING_POLLS: u64 = 180;

/// The authenticated client for the web app's device routes.
pub struct HttpCloud {
    agent: ureq::Agent,
    base: String,
    store: Box<dyn CredentialStore>,
}

impl HttpCloud {
    /// Build a client for `api_url`, reading/writing credentials through `store`.
    pub fn new(api_url: &str, store: Box<dyn CredentialStore>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(30))
            .build();
        Self {
            agent,
            base: api_url.trim_end_matches('/').to_string(),
            store,
        }
    }

    fn credentials(&self) -> Result<Option<Credentials>, CloudError> {
        self.store
            .load()
            .map_err(|err| CloudError::BadCredentials(err.to_string()))
    }

    fn require_credentials(&self) -> Result<Credentials, CloudError> {
        self.credentials()?.ok_or(CloudError::NotPaired)
    }

    fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        token: Option<&str>,
    ) -> Result<R, CloudError> {
        let url = format!("{}{path}", self.base);
        let mut request = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        if let Some(token) = token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        // `ureq` is built without its `json` feature, so serialize the body here and
        // send it as a string (the daemon client does the same).
        let payload =
            serde_json::to_string(body).map_err(|err| CloudError::Other(err.to_string()))?;
        match request.send_string(&payload) {
            Ok(resp) => parse_json(resp),
            Err(err) => Err(self.transport_error(err)),
        }
    }

    fn get_json<R: DeserializeOwned>(&self, path: &str, token: &str) -> Result<R, CloudError> {
        let url = format!("{}{path}", self.base);
        match self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(resp) => parse_json(resp),
            Err(err) => Err(self.transport_error(err)),
        }
    }

    /// Map a ureq error to a `CloudError`: a 401 means the device token was
    /// rejected (revoked/expired) so re-pairing is needed; other statuses surface
    /// the server's `{ "error" }` message; a transport failure is unreachable.
    fn transport_error(&self, err: ureq::Error) -> CloudError {
        match err {
            ureq::Error::Status(401, _) => CloudError::NotPaired,
            ureq::Error::Status(code, resp) => CloudError::Http(http_message(code, resp)),
            ureq::Error::Transport(_) => CloudError::NotReachable(self.base.clone()),
        }
    }

    /// Poll `/pair/token` until the player approves (or the bound elapses).
    fn poll_for_token(
        &self,
        device_code: &str,
        interval: u64,
        expires_in: u64,
    ) -> Result<String, CloudError> {
        let interval = interval.clamp(1, 30);
        let budget = (expires_in / interval).clamp(1, MAX_PAIRING_POLLS);
        for poll in 0..budget {
            let body = DeviceCodeBody { device_code };
            match self.post_json::<_, PairTokenResponse>("/api/devices/pair/token", &body, None)? {
                PairTokenResponse::Complete { device_token } => return Ok(device_token),
                PairTokenResponse::Pending => {
                    if poll + 1 < budget {
                        std::thread::sleep(Duration::from_secs(interval));
                    }
                }
            }
        }
        Err(CloudError::PairingTimedOut)
    }
}

/// A human label for this device, so the player recognizes it in `/devices`.
fn device_name() -> String {
    std::env::var("PROMPTLY_DEVICE_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(hostname)
        .unwrap_or_else(|| "promptly cli".to_string())
}

fn hostname() -> Option<String> {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

fn parse_json<R: DeserializeOwned>(resp: ureq::Response) -> Result<R, CloudError> {
    let body = resp
        .into_string()
        .map_err(|err| CloudError::Other(err.to_string()))?;
    serde_json::from_str(&body)
        .map_err(|err| CloudError::Http(format!("unexpected response: {err}")))
}

fn http_message(code: u16, resp: ureq::Response) -> String {
    let body = resp.into_string().unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {code}"))
}

impl Cloud for HttpCloud {
    fn pair(&self) -> Result<(), CloudError> {
        let (key, seed_b64) = generate_device_keypair()?;
        let public_key = signing::public_key_base64(&key);
        let proof = sign_proof(&key, &public_key);
        let name = device_name();

        let start: PairStartResponse = self.post_json(
            "/api/devices/pair",
            &PairStartBody {
                public_key: &public_key,
                proof: &proof,
                name: &name,
            },
            None,
        )?;

        println!(
            "To pair this device, open {} and enter code: {}",
            start.verification_uri, start.user_code
        );
        println!("Waiting for approval…");

        let device_token =
            self.poll_for_token(&start.device_code, start.interval, start.expires_in)?;
        self.store
            .save(&Credentials {
                device_token,
                signing_seed_b64: seed_b64,
            })
            .map_err(|err| CloudError::Other(err.to_string()))?;
        Ok(())
    }

    fn prepare_attempt(&self, slug: &str) -> Result<Option<PreparedAttempt>, CloudError> {
        // Unpaired is offline, not an error: the daemon seeds a local nonce and
        // the capture caps at `unverified`.
        let Some(creds) = self.credentials()? else {
            return Ok(None);
        };
        let response: AttemptResponse = self.post_json(
            "/api/cli/attempts",
            &SlugBody { slug },
            Some(&creds.device_token),
        )?;
        Ok(Some(PreparedAttempt {
            nonce: response.attempt_nonce,
            baseline_hash: response.baseline_hash,
        }))
    }

    fn submit(
        &self,
        slug: &str,
        bundle: &SubmissionBundle,
        capture: &CaptureUpload,
    ) -> Result<SubmitReceipt, CloudError> {
        let creds = self.require_credentials()?;
        let key = decode_signing_key(&creds.signing_seed_b64)?;

        // Hash the (already-redacted) bundle, sign the chain binding that hash and
        // the cross-source corroboration summary, and upload the same bytes — so the
        // server's recomputed hash matches and the capture can verify.
        let final_hash = final_code_hash(bundle);
        let cross_source = cross_source_summary(capture.turns);
        let chain = signed_chain_for(
            &key,
            capture.attempt_nonce.unwrap_or(""),
            capture.turns,
            &cross_source,
            &capture.capture_summary,
            &final_hash,
        );
        let archive = zip_bundle(bundle)?;

        let body = SubmitBody {
            slug,
            intake: Intake {
                mode: "zip",
                archive_base64: STANDARD.encode(&archive),
            },
            telemetry_confidence: derive_confidence(capture.turns),
            telemetry_session_id: capture.telemetry_session_id,
            signed_chain: chain,
        };
        let response: SubmitResponse =
            self.post_json("/api/cli/submit", &body, Some(&creds.device_token))?;
        Ok(SubmitReceipt {
            submission_id: response.submission_id,
            status: response.status,
        })
    }

    fn submission_status(&self, submission_id: &str) -> Result<RemoteStatus, CloudError> {
        let creds = self.require_credentials()?;
        let response: StatusResponse = self.get_json(
            &format!("/api/cli/submissions/{submission_id}"),
            &creds.device_token,
        )?;
        Ok(RemoteStatus {
            status: response.status,
            graded: response.graded.map(|g| GradedScore {
                score: g.score,
                correctness_pct: g.correctness_pct,
                recognized: g.recognized,
            }),
        })
    }
}

impl RemoteTests for HttpCloud {
    fn run_public_tests(
        &self,
        slug: &str,
        bundle: &SubmissionBundle,
    ) -> Result<RemoteTestReport, CloudError> {
        let creds = self.require_credentials()?;
        let archive = zip_bundle(bundle)?;
        let body = RemoteTestBody {
            slug,
            intake: Intake {
                mode: "zip",
                archive_base64: STANDARD.encode(&archive),
            },
        };
        let payload =
            serde_json::to_string(&body).map_err(|err| CloudError::Other(err.to_string()))?;
        let url = format!("{}/api/cli/test", self.base);
        let request = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {}", creds.device_token));
        match request.send_string(&payload) {
            Ok(resp) => parse_json(resp),
            // The live endpoint never returns a 404 (an unknown level is a 422
            // there, precisely so this stays unambiguous): a 404 means the
            // server predates `POST /api/cli/test` entirely.
            Err(ureq::Error::Status(404, _)) => Err(CloudError::UnsupportedEndpoint),
            Err(err) => Err(self.transport_error(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryCredentialStore;
    use crate::submission::SubmissionFile;
    use ed25519_dalek::{Signature, Verifier};
    use promptlyd::model::{Agreement, Plausibility, Source};

    fn turn(model: &str, confidence: Confidence, input: u64, output: u64) -> NormalizedTurn {
        NormalizedTurn {
            schema_version: 1,
            turn_id: format!("{model}-{input}-{output}"),
            model: model.to_string(),
            harness: "claude_code_cli".to_string(),
            tokens_input: input,
            tokens_output: output,
            tokens_thinking: 0,
            tokens_cache: 0,
            prompt_id: None,
            timestamp_ms: 1,
            confidence,
            cost_usd: None,
            duration_ms: None,
            sources: vec![Source::Otel],
            session_id: Some("sess-1".into()),
            attempt_nonce: Some("n".into()),
            workspace: None,
            agreement: Agreement::Single,
            plausibility: Plausibility::Plausible,
        }
    }

    fn bundle(files: &[(&str, &str)]) -> SubmissionBundle {
        let files: Vec<SubmissionFile> = files
            .iter()
            .map(|(path, content)| SubmissionFile {
                path: (*path).to_string(),
                bytes: content.as_bytes().to_vec(),
            })
            .collect();
        let total_bytes = files.iter().map(|f| f.bytes.len() as u64).sum();
        SubmissionBundle { files, total_bytes }
    }

    /// A representative capture summary (server-nonce, attested baseline) for the
    /// signing tests.
    fn sample_summary() -> CaptureSummary {
        CaptureSummary {
            baseline_attested: true,
            baseline_reset_count: 0,
            bulk_paste_events: 0,
            ignore_changed: false,
            nonce_origin: "server".to_string(),
            pause_count: 0,
            paused_ms_total: 0,
            prompt_count: 2,
            signed_at_ms: 1_000,
            started_at_ms: 0,
            untracked_edit_windows: 0,
        }
    }

    #[test]
    fn key_ownership_message_matches_the_server_contract() {
        assert_eq!(
            key_ownership_message("PUBKEY"),
            "promptly-device-pairing:v1:PUBKEY"
        );
    }

    #[test]
    fn a_generated_keypair_round_trips_through_its_seed() {
        let (key, seed_b64) = generate_device_keypair().unwrap();
        let restored = decode_signing_key(&seed_b64).unwrap();
        assert_eq!(
            signing::public_key_base64(&key),
            signing::public_key_base64(&restored),
            "the seed rebuilds the same key"
        );
    }

    #[test]
    fn a_bad_seed_is_a_credentials_error_not_a_panic() {
        assert!(matches!(
            decode_signing_key("not-base64!!"),
            Err(CloudError::BadCredentials(_))
        ));
        assert!(
            matches!(
                decode_signing_key(&STANDARD.encode([0u8; 16])),
                Err(CloudError::BadCredentials(_))
            ),
            "a 16-byte seed is rejected"
        );
    }

    #[test]
    fn the_pairing_proof_verifies_under_the_generated_key() {
        let (key, _) = generate_device_keypair().unwrap();
        let public_key = signing::public_key_base64(&key);
        let proof = sign_proof(&key, &public_key);
        // The server verifies exactly this: the proof over the ownership message.
        let signature = Signature::from_slice(&STANDARD.decode(&proof).unwrap()).unwrap();
        let message = key_ownership_message(&public_key);
        assert!(key
            .verifying_key()
            .verify(message.as_bytes(), &signature)
            .is_ok());
    }

    #[test]
    fn final_code_hash_equals_the_baseline_port() {
        let b = bundle(&[("b.go", "package b\n"), ("a.go", "package a\n")]);
        let expected = promptlyd::baseline::compute_baseline_hash(&[
            CanonicalFile {
                path: "b.go".into(),
                content: b"package b\n".to_vec(),
            },
            CanonicalFile {
                path: "a.go".into(),
                content: b"package a\n".to_vec(),
            },
        ]);
        assert_eq!(final_code_hash(&b), expected);
        assert_eq!(expected.len(), 64, "full sha-256 hex");
    }

    #[test]
    fn the_signed_chain_binds_indices_the_hash_and_verifies() {
        let (key, _) = generate_device_keypair().unwrap();
        let mut turns = [
            turn("claude-opus-4-8", Confidence::Otel, 100, 50),
            turn("claude-opus-4-8", Confidence::Jsonl, 60, 40),
        ];
        // The second turn's sources disagree — the summary the chain must bind.
        turns[1].agreement = Agreement::Disagree {
            fields: vec!["tokens_output".into()],
        };
        let cross = cross_source_summary(&turns);
        let summary = sample_summary();
        let chain = signed_chain_for(&key, "nonce-1", &turns, &cross, &summary, "deadbeef");

        // Indices are sequential and the final entry binds the hash + the summaries.
        assert_eq!(chain.turns[0].turn_index, 0);
        assert_eq!(chain.turns[1].turn_index, 1);
        assert_eq!(chain.final_entry.final_code_hash, "deadbeef");
        assert_eq!(chain.final_entry.cross_source.disagree_turns, 1);
        assert_eq!(chain.final_entry.capture_summary.nonce_origin, "server");
        assert!(chain.final_entry.capture_summary.baseline_attested);

        // Each turn signature verifies over its canonical message at the current
        // chain version (the anti-replay anchor), chained to the previous signature.
        let vk = key.verifying_key();
        let mut prev: Option<String> = None;
        for (i, signed) in chain.turns.iter().enumerate() {
            let message = signing::canonical_turn_message(
                signing::CHAIN_VERSION,
                "nonce-1",
                i as u32,
                &signed.model,
                &TokenCounts {
                    input: signed.token_counts.input,
                    output: signed.token_counts.output,
                    thinking: signed.token_counts.thinking,
                    cache: signed.token_counts.cache,
                },
                prev.as_deref(),
                Some(signing::TurnV3 {
                    confidence: &signed.confidence,
                    sources: &signed.sources,
                    timestamp_ms: signed.timestamp_ms,
                }),
            );
            let sig = Signature::from_slice(&STANDARD.decode(&signed.signature).unwrap()).unwrap();
            assert!(
                vk.verify(message.as_bytes(), &sig).is_ok(),
                "turn {i} verifies"
            );
            prev = Some(signed.signature.clone());
        }

        // The terminal entry verifies over the signed cross_source + capture summary
        // — so the server rejects a stripped or zeroed summary as a broken chain.
        let final_message = signing::canonical_final_message(
            signing::CHAIN_VERSION,
            "nonce-1",
            "deadbeef",
            &chain.final_entry.cross_source,
            Some(&chain.final_entry.capture_summary),
            prev.as_deref(),
            chain.turns.len(),
        );
        let final_sig =
            Signature::from_slice(&STANDARD.decode(&chain.final_entry.signature).unwrap()).unwrap();
        assert!(
            vk.verify(final_message.as_bytes(), &final_sig).is_ok(),
            "terminal entry verifies"
        );
    }

    #[test]
    fn the_submit_body_has_the_fields_the_server_parses() {
        let (key, _) = generate_device_keypair().unwrap();
        let b = bundle(&[("lru.go", "package main\n")]);
        let turns = [turn("claude-opus-4-8", Confidence::Otel, 10, 5)];
        let chain = signed_chain_for(
            &key,
            "n",
            &turns,
            &cross_source_summary(&turns),
            &sample_summary(),
            &final_code_hash(&b),
        );
        let body = SubmitBody {
            slug: "stage-1-01",
            intake: Intake {
                mode: "zip",
                archive_base64: STANDARD.encode(zip_bundle(&b).unwrap()),
            },
            telemetry_confidence: derive_confidence(&turns),
            telemetry_session_id: "sess-1",
            signed_chain: chain,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["slug"], "stage-1-01");
        assert_eq!(value["intake"]["mode"], "zip");
        assert!(!value["intake"]["archiveBase64"]
            .as_str()
            .unwrap()
            .is_empty());
        assert_eq!(value["telemetry_confidence"], "otel");
        assert_eq!(value["telemetry_session_id"], "sess-1");
        // v4: the chain advertises version 4 and carries the signed cross_source,
        // the per-turn provenance, and the capture summary (incl. the prompt count).
        assert_eq!(value["signed_chain"]["chain_version"], 4);
        assert!(value["signed_chain"]["final"]["final_code_hash"].is_string());
        assert!(value["signed_chain"]["final"]["cross_source"].is_object());
        assert_eq!(
            value["signed_chain"]["final"]["capture_summary"]["nonce_origin"],
            "server"
        );
        assert!(value["signed_chain"]["final"]["capture_summary"]["prompt_count"].is_number());
        assert_eq!(value["signed_chain"]["turns"][0]["confidence"], "otel");
        assert_eq!(value["signed_chain"]["turns"][0]["sources"][0], "otel");
    }

    #[test]
    fn cross_source_summary_unions_disagreeing_fields_sorted() {
        let mut t0 = turn("m", Confidence::Otel, 1, 1);
        let mut t1 = turn("m", Confidence::Jsonl, 1, 1);
        let t2 = turn("m", Confidence::Otel, 1, 1); // Agreement::Single -> ignored
                                                    // Out-of-order fields across two turns: the summary unions + sorts them.
        t0.agreement = Agreement::Disagree {
            fields: vec!["tokens_output".into(), "model".into()],
        };
        t1.agreement = Agreement::Disagree {
            fields: vec!["model".into()],
        };
        let summary = cross_source_summary(&[t0, t1, t2]);
        assert_eq!(summary.disagree_turns, 2);
        assert_eq!(summary.disagree_fields, vec!["model", "tokens_output"]);
    }

    #[test]
    fn cross_source_summary_is_empty_when_sources_agree() {
        // The `turn` helper builds `Agreement::Single` turns — nothing disagrees.
        let summary = cross_source_summary(&[turn("m", Confidence::Otel, 1, 1)]);
        assert_eq!(summary.disagree_turns, 0);
        assert!(summary.disagree_fields.is_empty());
    }

    #[test]
    fn derive_confidence_takes_the_weakest_signal() {
        assert_eq!(derive_confidence(&[]), "estimated", "no turns -> estimated");
        assert_eq!(
            derive_confidence(&[
                turn("m", Confidence::Otel, 1, 1),
                turn("m", Confidence::Otel, 1, 1)
            ]),
            "otel"
        );
        assert_eq!(
            derive_confidence(&[
                turn("m", Confidence::Otel, 1, 1),
                turn("m", Confidence::Jsonl, 1, 1)
            ]),
            "jsonl"
        );
        assert_eq!(
            derive_confidence(&[
                turn("m", Confidence::Otel, 1, 1),
                turn("m", Confidence::Estimated, 1, 1)
            ]),
            "estimated"
        );
    }

    #[test]
    fn zip_bundle_round_trips_through_the_zip_reader() {
        let b = bundle(&[("a.go", "package a\n"), ("dir/b.go", "package b\n")]);
        let archive = zip_bundle(&b).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
        assert_eq!(zip.len(), 2);
        let mut found = std::collections::BTreeMap::new();
        for i in 0..zip.len() {
            use std::io::Read;
            let mut entry = zip.by_index(i).unwrap();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            found.insert(entry.name().to_string(), content);
        }
        assert_eq!(found.get("a.go").map(String::as_str), Some("package a\n"));
        assert_eq!(
            found.get("dir/b.go").map(String::as_str),
            Some("package b\n")
        );
    }

    #[test]
    fn parity_report_warns_only_when_the_server_outscores_the_projection() {
        let graded = GradedScore {
            score: 100.0,
            correctness_pct: 100.0,
            recognized: true,
        };
        // Projection >= graded: a clean clear, no warning.
        assert!(parity_report(100.0, &graded).warning.is_none());
        assert!(parity_report(120.0, &graded).warning.is_none());
        // Server outscores our best case by >1%: a parity violation.
        let report = parity_report(90.0, &graded);
        assert!(report.warning.is_some());
        assert_eq!(report.correctness_pct, 100.0);
        assert!(report.recognized);
    }

    #[test]
    fn the_remote_test_body_matches_the_server_contract() {
        let b = bundle(&[("lru.go", "package main\n")]);
        let body = RemoteTestBody {
            slug: "stage-1-01",
            intake: Intake {
                mode: "zip",
                archive_base64: STANDARD.encode(zip_bundle(&b).unwrap()),
            },
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["slug"], "stage-1-01");
        assert_eq!(value["intake"]["mode"], "zip");
        assert!(!value["intake"]["archiveBase64"]
            .as_str()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_remote_test_report_parses_the_server_response() {
        // The exact 200 shape `POST /api/cli/test` returns (extra fields like
        // `passedCount`/`timeSeconds` are carried on the wire but not read).
        let report: RemoteTestReport = serde_json::from_str(
            r#"{"ok":true,"slug":"stage-1-01","passed":false,"passedCount":1,
                "totalCount":3,"crashed":false,
                "cases":[
                  {"name":"a","status":"passed","timeSeconds":0.02,"message":null},
                  {"name":"b","status":"failed","timeSeconds":0.01,"message":"expected 3"},
                  {"name":"c","status":"missing","timeSeconds":null,"message":null}],
                "compileOutput":"warning: unused import"}"#,
        )
        .unwrap();
        assert!(!report.passed);
        assert!(!report.crashed);
        assert_eq!(report.cases.len(), 3);
        assert_eq!(report.cases[0].status, "passed");
        assert_eq!(report.cases[1].message.as_deref(), Some("expected 3"));
        assert_eq!(report.cases[2].status, "missing");
        assert_eq!(
            report.compile_output.as_deref(),
            Some("warning: unused import")
        );
    }

    #[test]
    fn run_public_tests_requires_pairing_before_any_network() {
        // Empty credential store -> NotPaired, and no request is attempted
        // (port 1 would refuse the connection with a different error).
        let cloud = HttpCloud::new("http://127.0.0.1:1", Box::new(MemoryCredentialStore::new()));
        assert!(matches!(
            cloud.run_public_tests("stage-1-01", &bundle(&[("a.go", "package a\n")])),
            Err(CloudError::NotPaired)
        ));
    }

    #[test]
    fn run_public_tests_paired_but_unreachable_is_not_unsupported() {
        // A transport failure must stay NotReachable — only a served 404 means
        // "this server predates the endpoint".
        let creds = Credentials {
            device_token: "tok".into(),
            signing_seed_b64: STANDARD.encode([7u8; 32]),
        };
        let cloud = HttpCloud::new(
            "http://127.0.0.1:1",
            Box::new(MemoryCredentialStore::with(creds)),
        );
        assert!(matches!(
            cloud.run_public_tests("stage-1-01", &bundle(&[("a.go", "package a\n")])),
            Err(CloudError::NotReachable(_))
        ));
    }

    #[test]
    fn prepare_attempt_is_offline_ok_when_unpaired() {
        // No credentials -> Ok(None), and no network touched (port 1 would refuse).
        let cloud = HttpCloud::new("http://127.0.0.1:1", Box::new(MemoryCredentialStore::new()));
        assert_eq!(cloud.prepare_attempt("stage-1-01").unwrap(), None);
        assert!(cloud.prepare_attempt("stage-1-01").unwrap().is_none());
    }

    #[test]
    fn paired_but_unreachable_is_an_error_not_offline() {
        let creds = Credentials {
            device_token: "tok".into(),
            signing_seed_b64: STANDARD.encode([7u8; 32]),
        };
        let cloud = HttpCloud::new(
            "http://127.0.0.1:1",
            Box::new(MemoryCredentialStore::with(creds)),
        );
        // Paired: prepare_attempt now makes a request, which can't connect.
        assert!(matches!(
            cloud.prepare_attempt("stage-1-01"),
            Err(CloudError::NotReachable(_))
        ));
        // Pairing posts before printing, so an unreachable server fails cleanly.
        assert!(matches!(cloud.pair(), Err(CloudError::NotReachable(_))));
    }
}
