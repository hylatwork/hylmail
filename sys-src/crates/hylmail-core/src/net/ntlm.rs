//! NTLM (`MS-NLMP`) authentication for the Exchange Web Services client.
//!
//! Exchange on-premises usually offers **only** NTLM (or `Negotiate`, which
//! starts with the same message) on its EWS endpoint, so a client that wants to
//! talk to it has to implement the challenge/response handshake.  Everything
//! needed for that lives in this module:
//!
//! | Message | Direction | What it carries |
//! |---------|-----------|-----------------|
//! | type 1, `NEGOTIATE` | client to server | the flags this client supports |
//! | type 2, `CHALLENGE` | server to client | an 8 byte server challenge and the target info |
//! | type 3, `AUTHENTICATE` | client to server | the LMv2 and NTLMv2 responses |
//!
//! Only **NTLMv2** is implemented: LMv1 and NTLMv1 are broken, and NTLMv2 is
//! what every server that still speaks NTLM accepts.  It needs exactly two
//! primitives, MD4 for the NT hash and HMAC-MD5 for the responses, which is why
//! the protocol is implemented here instead of through a third party crate:
//! `sspi` 0.23 requires a newer compiler than this workspace supports, drags in
//! Kerberos and PKI machinery and defaults to a *second* TLS crypto provider,
//! and `ntlmclient` binds a C library that would have to be compiled with cmake
//! and OpenSSL on every platform.  The dependency cost of doing it here is two
//! small pure Rust digest crates.
//!
//! The handshake is **connection bound**: the type 3 message must be sent on
//! the very connection the type 2 challenge arrived on, because the responses
//! are tied to the server challenge.  The HTTP transport reflects that by
//! switching to a single pooled connection while NTLM is in use, see
//! [`crate::soap::http`].
//!
//! References: [`MS-NLMP`](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-nlmp/),
//! sections 2.2.1.1 (type 1), 2.2.1.2 (type 2) and 2.2.1.3 (type 3).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use md4::{Digest, Md4};
use md5::Md5;
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, Result};

/// The signature every NTLM message starts with, including the terminating NUL.
const SIGNATURE: [u8; 8] = *b"NTLMSSP\0";

const MESSAGE_NEGOTIATE: u32 = 1;
const MESSAGE_CHALLENGE: u32 = 2;
const MESSAGE_AUTHENTICATE: u32 = 3;

const NEGOTIATE_UNICODE: u32 = 0x0000_0001;
const NEGOTIATE_OEM: u32 = 0x0000_0002;
const REQUEST_TARGET: u32 = 0x0000_0004;
const NEGOTIATE_NTLM: u32 = 0x0000_0200;
const NEGOTIATE_ALWAYS_SIGN: u32 = 0x0000_8000;
const NEGOTIATE_EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
const NEGOTIATE_128: u32 = 0x2000_0000;
const NEGOTIATE_56: u32 = 0x8000_0000;

/// Flags offered in the type 1 message.
///
/// No signing and no sealing: the payload travels inside TLS, so message
/// integrity and confidentiality are already provided by the transport.
const CLIENT_FLAGS: u32 = NEGOTIATE_UNICODE
    | NEGOTIATE_OEM
    | REQUEST_TARGET
    | NEGOTIATE_NTLM
    | NEGOTIATE_ALWAYS_SIGN
    | NEGOTIATE_EXTENDED_SESSIONSECURITY
    | NEGOTIATE_128
    | NEGOTIATE_56;

/// Size of the type 1 message.
const NEGOTIATE_SIZE: usize = 32;

/// Size of the fixed part of the type 3 message, without the optional version
/// field.  The payload follows directly after it.
const AUTHENTICATE_SIZE: usize = 64;

/// HMAC-MD5 with the derived NTLMv2 key.
type HmacMd5 = Hmac<Md5>;

/// Windows `FILETIME` ticks (100 ns) between 1601-01-01 and 1970-01-01.
const EPOCH_DIFFERENCE_TICKS: u64 = 11_644_473_600 * 10_000_000;

/// Ticks per second, the unit of a `FILETIME`.
const TICKS_PER_SECOND: u64 = 10_000_000;

/// One NTLM identity: user, domain and password.
///
/// The password is held as a [`SecretString`] so that it cannot leak through a
/// `Debug` or `Display` implementation; [`Ntlm`]'s own `Debug` redacts it too.
pub struct Ntlm {
    user: String,
    domain: String,
    password: SecretString,
}

impl std::fmt::Debug for Ntlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ntlm")
            .field("user", &self.user)
            .field("domain", &self.domain)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// The parts of a type 2 message this client needs.
#[derive(Debug, Clone)]
struct Challenge {
    flags: u32,
    server_challenge: [u8; 8],
    target_info: Vec<u8>,
}

impl Ntlm {
    /// Builds an identity from a login name and a password.
    ///
    /// The login name may be written `DOMAIN\user` (NetBIOS style) or
    /// `user@domain.example` (UPN style); a bare `user` leaves the domain
    /// empty, which some proxies accept for local accounts.
    #[must_use]
    pub fn new(login: &str, password: &SecretString) -> Self {
        let (domain, user) = split_login(login);
        Self {
            user,
            domain,
            password: password.clone(),
        }
    }

    /// The identity as it appears in error messages.  Never the password.
    fn account(&self) -> String {
        if self.domain.is_empty() {
            self.user.clone()
        } else {
            format!("{}\\{}", self.domain, self.user)
        }
    }

    /// An authentication failure caused by this client or by a hostile server
    /// answer - as opposed to one the server itself reported.
    fn rejected(&self, message: &str) -> Error {
        Error::Auth {
            account: self.account(),
            message: message.to_string(),
        }
    }

    /// The type 1 message, base64 encoded, for `Authorization: NTLM <value>`.
    ///
    /// The domain and workstation fields are deliberately empty: the server
    /// does not need them, and leaving them out keeps the local host name off
    /// the wire.
    #[must_use]
    pub fn negotiate(&self) -> String {
        let mut message = Vec::with_capacity(NEGOTIATE_SIZE);
        message.extend_from_slice(&SIGNATURE);
        message.extend_from_slice(&MESSAGE_NEGOTIATE.to_le_bytes());
        message.extend_from_slice(&CLIENT_FLAGS.to_le_bytes());
        for _ in 0..2 {
            // length, maximum length, offset (the payload is empty)
            message.extend_from_slice(&0_u16.to_le_bytes());
            message.extend_from_slice(&0_u16.to_le_bytes());
            message.extend_from_slice(&(NEGOTIATE_SIZE as u32).to_le_bytes());
        }
        BASE64.encode(message)
    }

    /// Answers a base64 encoded type 2 message with a type 3 message, also
    /// base64 encoded.
    ///
    /// A fresh client challenge and the current time are used, so two runs
    /// produce different messages even against the same server challenge.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] when the challenge is not a well formed type 2
    /// message, or when the server does not offer what NTLMv2 requires.
    pub fn authenticate(&self, challenge: &str) -> Result<String> {
        let raw = BASE64
            .decode(challenge.trim())
            .map_err(|_| self.rejected("the NTLM challenge is not valid base64"))?;
        let challenge = self.parse_challenge(&raw)?;
        let client_challenge = client_challenge();
        // Only what both sides support, as `MS-NLMP` requires; the server
        // challenge is bound to this connection, and so is the answer.
        let flags = CLIENT_FLAGS & challenge.flags;
        self.authenticate_with(
            challenge.server_challenge,
            &challenge.target_info,
            client_challenge,
            filetime_now(),
            flags,
        )
    }

    /// Deterministic core of [`Self::authenticate`]: same inputs, same bytes.
    ///
    /// Used by the tests, which check the result against the published
    /// `MS-NLMP` vectors, and by callers that want to replay a captured
    /// challenge.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] when a field does not fit into the protocol's
    /// 16 bit length fields, which cannot happen for realistic names.
    pub fn authenticate_with(
        &self,
        server_challenge: [u8; 8],
        target_info: &[u8],
        client_challenge: [u8; 8],
        timestamp: u64,
        flags: u32,
    ) -> Result<String> {
        let key = self.ntlmv2_key()?;
        let lm = self.lm_response(&key, server_challenge, client_challenge)?;
        let nt = self.nt_response(
            &key,
            server_challenge,
            target_info,
            client_challenge,
            timestamp,
        )?;

        let domain = utf16le(&self.domain);
        let user = utf16le(&self.user);

        // The payload follows the fixed header, in the order in which the
        // security buffers below point at it.
        let mut payload = Vec::with_capacity(lm.len() + nt.len() + domain.len() + user.len());
        let lm_offset = AUTHENTICATE_SIZE;
        payload.extend_from_slice(&lm);
        let nt_offset = lm_offset + lm.len();
        payload.extend_from_slice(&nt);
        let domain_offset = nt_offset + nt.len();
        payload.extend_from_slice(&domain);
        let user_offset = domain_offset + domain.len();
        payload.extend_from_slice(&user);
        // No workstation, no session key: nothing is signed or sealed.

        let flags = flags & CLIENT_FLAGS;
        let mut message = Vec::with_capacity(AUTHENTICATE_SIZE + payload.len());
        message.extend_from_slice(&SIGNATURE);
        message.extend_from_slice(&MESSAGE_AUTHENTICATE.to_le_bytes());
        self.security_buffer(&mut message, lm.len(), lm_offset)?;
        self.security_buffer(&mut message, nt.len(), nt_offset)?;
        self.security_buffer(&mut message, domain.len(), domain_offset)?;
        self.security_buffer(&mut message, user.len(), user_offset)?;
        self.security_buffer(&mut message, 0, AUTHENTICATE_SIZE)?; // workstation
        self.security_buffer(&mut message, 0, AUTHENTICATE_SIZE)?; // session key
        message.extend_from_slice(&flags.to_le_bytes());
        message.extend_from_slice(&payload);

        Ok(BASE64.encode(message))
    }

    /// Writes one 8 byte `SecBuffer`: length, maximum length, offset.
    fn security_buffer(&self, message: &mut Vec<u8>, length: usize, offset: usize) -> Result<()> {
        let length = u16::try_from(length)
            .map_err(|_| self.rejected("an NTLM field is longer than 65535 bytes"))?;
        let offset = u32::try_from(offset)
            .map_err(|_| self.rejected("an NTLM field starts beyond 4 GiB"))?;
        message.extend_from_slice(&length.to_le_bytes());
        message.extend_from_slice(&length.to_le_bytes());
        message.extend_from_slice(&offset.to_le_bytes());
        Ok(())
    }

    /// `NTOWFv2`: `HMAC-MD5(MD4(UTF16LE(password)), UTF16LE(UPPER(user) || domain))`.
    fn ntlmv2_key(&self) -> Result<[u8; 16]> {
        let nt_hash = Md4::digest(utf16le(self.password.expose_secret()));
        let account = utf16le(&format!("{}{}", self.user.to_uppercase(), self.domain));
        hmac_md5(nt_hash.as_slice(), &account, &self.account())
    }

    /// `LMv2`: `HMAC-MD5(key, server challenge || client challenge)` plus the
    /// client challenge.
    fn lm_response(
        &self,
        key: &[u8; 16],
        server_challenge: [u8; 8],
        client_challenge: [u8; 8],
    ) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&server_challenge);
        data.extend_from_slice(&client_challenge);
        let mut response = hmac_md5(key, &data, &self.account())?.to_vec();
        response.extend_from_slice(&client_challenge);
        Ok(response)
    }

    /// `NTv2`: `HMAC-MD5(key, server challenge || blob)` plus the blob, where
    /// the blob carries the response type, the timestamp, the client challenge
    /// and the target info of the server (empty here when it sent none).
    fn nt_response(
        &self,
        key: &[u8; 16],
        server_challenge: [u8; 8],
        target_info: &[u8],
        client_challenge: [u8; 8],
        timestamp: u64,
    ) -> Result<Vec<u8>> {
        let mut blob = Vec::with_capacity(32 + target_info.len());
        blob.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]); // response type 1
        blob.extend_from_slice(&[0x00; 4]); // reserved
        blob.extend_from_slice(&timestamp.to_le_bytes());
        blob.extend_from_slice(&client_challenge);
        if target_info.is_empty() {
            // The AV pair list of the blob ends with a four byte terminator;
            // an empty list is just that terminator.
            blob.extend_from_slice(&[0x00; 4]);
        } else {
            // Copied verbatim from the type 2 message, including its own
            // terminator.
            blob.extend_from_slice(target_info);
        }

        let mut data = Vec::with_capacity(8 + blob.len());
        data.extend_from_slice(&server_challenge);
        data.extend_from_slice(&blob);

        let mut response = hmac_md5(key, &data, &self.account())?.to_vec();
        response.extend_from_slice(&blob);
        Ok(response)
    }

    /// Reads the flags, the server challenge and the target info of a type 2
    /// message.
    fn parse_challenge(&self, raw: &[u8]) -> Result<Challenge> {
        if raw.len() < 48 {
            return Err(self.rejected("the NTLM challenge is shorter than 48 bytes"));
        }
        if raw[..8] != SIGNATURE {
            return Err(self.rejected("the NTLM challenge does not start with NTLMSSP"));
        }
        let message_type = u32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4]));
        if message_type != MESSAGE_CHALLENGE {
            return Err(self.rejected(&format!(
                "expected an NTLM challenge (type 2), got message type {message_type}"
            )));
        }

        let flags = u32::from_le_bytes(raw[20..24].try_into().unwrap_or([0; 4]));
        if flags & NEGOTIATE_NTLM == 0 {
            return Err(self.rejected("the server does not offer NTLM"));
        }
        if flags & NEGOTIATE_UNICODE == 0 {
            return Err(self.rejected("the server does not negotiate unicode"));
        }
        if flags & NEGOTIATE_EXTENDED_SESSIONSECURITY == 0 {
            return Err(self.rejected(
                "the server does not negotiate extended session security, which NTLMv2 needs",
            ));
        }

        let mut server_challenge = [0_u8; 8];
        server_challenge.copy_from_slice(&raw[24..32]);

        // The target info is a security buffer whose content follows the fixed
        // part of the message; a server may also leave it out entirely.
        let (length, offset) = security_buffer(raw, 40)
            .ok_or_else(|| self.rejected("the NTLM challenge has no target info field"))?;
        let target_info = if length == 0 {
            Vec::new()
        } else {
            let end = offset + length;
            raw.get(offset..end)
                .ok_or_else(|| self.rejected("the target info of the challenge is truncated"))?
                .to_vec()
        };

        Ok(Challenge {
            flags,
            server_challenge,
            target_info,
        })
    }
}

/// Reads a security buffer at `at`: length, maximum length and offset.
fn security_buffer(raw: &[u8], at: usize) -> Option<(usize, usize)> {
    let length = u16::from_le_bytes(raw.get(at..at + 2)?.try_into().ok()?) as usize;
    let offset = u32::from_le_bytes(raw.get(at + 4..at + 8)?.try_into().ok()?) as usize;
    Some((length, offset))
}

/// Splits `DOMAIN\user`, `user@domain` or a bare `user` into domain and user.
fn split_login(login: &str) -> (String, String) {
    if let Some((domain, user)) = login.split_once('\\') {
        (domain.to_string(), user.to_string())
    } else if let Some((user, domain)) = login.split_once('@') {
        (domain.to_string(), user.to_string())
    } else {
        (String::new(), login.to_string())
    }
}

/// Encodes a string the way both NTLMv2 strings and NTLM passwords are encoded:
/// UTF-16 little endian, without a byte order mark.
fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// `HMAC-MD5(key, data)`.
///
/// HMAC accepts keys of any length, so the error branch is unreachable; it
/// exists because the constructor returns a `Result`.
fn hmac_md5(key: &[u8], data: &[u8], account: &str) -> Result<[u8; 16]> {
    let mut mac = <HmacMd5 as Mac>::new_from_slice(key).map_err(|_| Error::Auth {
        account: account.to_string(),
        message: "HMAC-MD5 rejected the key length".to_string(),
    })?;
    mac.update(data);
    let mut digest = [0_u8; 16];
    digest.copy_from_slice(mac.finalize().into_bytes().as_slice());
    Ok(digest)
}

/// A fresh 8 byte client challenge.
///
/// The bytes come from the operating system's random number generator through
/// [`uuid::Uuid::new_v4`]; folding the two halves together makes every bit
/// random even though a version 4 UUID has a fixed version nibble.
fn client_challenge() -> [u8; 8] {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut challenge = [0_u8; 8];
    for (index, byte) in challenge.iter_mut().enumerate() {
        *byte = bytes[index] ^ bytes[index + 8];
    }
    challenge
}

/// The current time as a Windows `FILETIME`, the timestamp the blob carries.
fn filetime_now() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => {
            (elapsed.as_secs() + EPOCH_DIFFERENCE_TICKS / TICKS_PER_SECOND) * TICKS_PER_SECOND
                + u64::from(elapsed.subsec_nanos()) / 100
        }
        // The clock is set before 1970; a timestamp of zero is what a client
        // with an unknown clock sends, and the server decides about it.
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The user, domain and password of the published `MS-NLMP` example, with
    /// every value below reproduced by an independent implementation while
    /// this module was written (MD4 checked against the five RFC 1320 test
    /// vectors, the NT hash against the published NTLM hash of `Password`).
    const LOGIN: &str = "Domain\\User";
    const PASSWORD: &str = "Password";
    const NT_HASH: &str = "a4f49c406510bdcab6824ee7c30fd852";
    const NTLMV2_KEY: &str = "0c868a403bfd7a93a3001ef22ef02e3f";
    const SERVER_CHALLENGE: [u8; 8] = [0xa4, 0xf1, 0xba, 0xa6, 0x7c, 0xdc, 0x1a, 0x12];
    const CLIENT_CHALLENGE: [u8; 8] = [0x20, 0xc0, 0x2b, 0x3d, 0xc0, 0x61, 0xa7, 0x73];
    const TIMESTAMP: u64 = 130_475_779_380_041_523;
    const LM_V2: &str = "5ec3c52ee75a23457372d82b43eac42620c02b3dc061a773";
    const NT_V2: &str = "a838989edcbecf8db75c148526a02af9\
                         01010000000000003357bdb1078bcf0120c02b3dc061a77300000000";
    const SESSION_BASE_KEY: &str = "72e53e4b8918c98fdafba0740661bc9f";

    /// The target info of a real type 2 message: `MsvAvNbDomainName` = "WIN7"
    /// plus the terminating end-of-list marker.
    const TARGET_INFO: [u8; 16] = [
        0x02, 0x00, 0x08, 0x00, 0x57, 0x00, 0x49, 0x00, 0x4e, 0x00, 0x37, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    const NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;

    fn identity() -> Ntlm {
        Ntlm::new(LOGIN, &SecretString::from(PASSWORD))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn little_u32(raw: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(raw[at..at + 4].try_into().unwrap())
    }

    fn little_u16(raw: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(raw[at..at + 2].try_into().unwrap())
    }

    /// Reads the security buffer at `at` out of a message.
    fn field(raw: &[u8], at: usize) -> &[u8] {
        let length = little_u16(raw, at) as usize;
        let offset = little_u32(raw, at + 4) as usize;
        &raw[offset..offset + length]
    }

    /// A type 2 message as a server sends it, including the optional target
    /// name and version fields, so that the parser is exercised on the full
    /// layout rather than on a minimal one.
    fn challenge(server_challenge: [u8; 8], target_info: &[u8], flags: u32) -> String {
        let target_name = utf16le("WIN7");
        let target_info_offset = 56 + target_name.len();
        let mut message = Vec::new();
        message.extend_from_slice(&SIGNATURE);
        message.extend_from_slice(&MESSAGE_CHALLENGE.to_le_bytes());
        message.extend_from_slice(&(target_name.len() as u16).to_le_bytes());
        message.extend_from_slice(&(target_name.len() as u16).to_le_bytes());
        message.extend_from_slice(&56_u32.to_le_bytes());
        message.extend_from_slice(&(flags | NEGOTIATE_TARGET_INFO).to_le_bytes());
        message.extend_from_slice(&server_challenge);
        message.extend_from_slice(&[0; 8]); // reserved
        message.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        message.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        message.extend_from_slice(&(target_info_offset as u32).to_le_bytes());
        message.extend_from_slice(&[0x06, 0x01, 0xb1, 0x1d, 0x00, 0x00, 0x00, 0x0f]);
        message.extend_from_slice(&target_name);
        message.extend_from_slice(target_info);
        BASE64.encode(message)
    }

    #[test]
    fn the_nt_hash_and_the_ntlmv2_key_match_the_ms_nlmp_vectors() {
        assert_eq!(hex(Md4::digest(utf16le(PASSWORD)).as_slice()), NT_HASH);
        assert_eq!(hex(identity().ntlmv2_key().unwrap().as_slice()), NTLMV2_KEY);
    }

    #[test]
    fn the_responses_match_the_ms_nlmp_vectors() {
        let identity = identity();
        let key = identity.ntlmv2_key().unwrap();

        let lm = identity
            .lm_response(&key, SERVER_CHALLENGE, CLIENT_CHALLENGE)
            .unwrap();
        assert_eq!(hex(&lm), LM_V2);

        let nt = identity
            .nt_response(&key, SERVER_CHALLENGE, &[], CLIENT_CHALLENGE, TIMESTAMP)
            .unwrap();
        assert_eq!(hex(&nt), NT_V2);

        // `MS-NLMP` 3.3.2: the session base key is the NT proof, so this also
        // pins the first sixteen bytes of the NTLMv2 response.
        let session_key = hmac_md5(&key, &nt[..16], "test").unwrap();
        assert_eq!(hex(&session_key), SESSION_BASE_KEY);
    }

    #[test]
    fn the_negotiate_message_has_the_documented_layout() {
        let message = BASE64.decode(identity().negotiate()).unwrap();
        assert_eq!(message.len(), NEGOTIATE_SIZE);
        assert_eq!(&message[..8], &SIGNATURE);
        assert_eq!(little_u32(&message, 8), MESSAGE_NEGOTIATE);
        assert_eq!(little_u32(&message, 12), CLIENT_FLAGS);
        // Empty domain and workstation security buffers that point behind the
        // message, which is what a client without a NetBIOS name sends.
        for at in [16, 24] {
            assert_eq!(little_u16(&message, at), 0);
            assert_eq!(little_u32(&message, at + 4), 32);
        }
    }

    #[test]
    fn the_authenticate_message_carries_the_verified_responses() {
        let raw = BASE64
            .decode(
                identity()
                    .authenticate_with(
                        SERVER_CHALLENGE,
                        &[],
                        CLIENT_CHALLENGE,
                        TIMESTAMP,
                        CLIENT_FLAGS,
                    )
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(&raw[..8], &SIGNATURE);
        assert_eq!(little_u32(&raw, 8), MESSAGE_AUTHENTICATE);
        assert_eq!(little_u32(&raw, 60), CLIENT_FLAGS);
        assert_eq!(hex(field(&raw, 12)), LM_V2);
        assert_eq!(hex(field(&raw, 20)), NT_V2);
        assert_eq!(field(&raw, 28), utf16le("Domain").as_slice());
        assert_eq!(field(&raw, 36), utf16le("User").as_slice());
        assert_eq!(field(&raw, 44), b""); // workstation
        assert_eq!(field(&raw, 52), b""); // session key
                                          // The payload ends with the last field, so nothing follows it.
        let payload = field(&raw, 12).len()
            + field(&raw, 20).len()
            + utf16le("Domain").len()
            + utf16le("User").len();
        assert_eq!(raw.len(), 64 + payload);
    }

    #[test]
    fn a_full_challenge_is_parsed_and_its_target_info_is_copied_into_the_blob() {
        let identity = identity();
        let server = challenge(SERVER_CHALLENGE, &TARGET_INFO, CLIENT_FLAGS);
        let parsed = identity
            .parse_challenge(&BASE64.decode(server.clone()).unwrap())
            .unwrap();
        assert_eq!(parsed.server_challenge, SERVER_CHALLENGE);
        assert_eq!(parsed.target_info, TARGET_INFO);

        let answered = BASE64
            .decode(identity.authenticate(&server).unwrap())
            .unwrap();
        assert_eq!(little_u32(&answered, 8), MESSAGE_AUTHENTICATE);
        // The blob of the NTLMv2 response ends with the AV pairs verbatim.
        assert!(field(&answered, 20).ends_with(&TARGET_INFO));
    }

    #[test]
    fn a_challenge_without_extended_session_security_is_refused() {
        let server = challenge(
            SERVER_CHALLENGE,
            &[],
            CLIENT_FLAGS & !NEGOTIATE_EXTENDED_SESSIONSECURITY,
        );
        let error = identity().authenticate(&server).unwrap_err();
        match error {
            Error::Auth { account, message } => {
                assert_eq!(account, LOGIN);
                assert!(message.contains("extended session security"), "{message}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_message_that_is_not_a_challenge_is_refused() {
        // A valid type 1 message where the server challenge belongs.
        let error = identity()
            .authenticate(&identity().negotiate())
            .unwrap_err();
        assert!(matches!(error, Error::Auth { .. }), "{error}");
        let error = identity().authenticate("not base64").unwrap_err();
        assert!(matches!(error, Error::Auth { .. }), "{error}");
        let error = identity().authenticate("").unwrap_err();
        assert!(matches!(error, Error::Auth { .. }), "{error}");
    }

    #[test]
    fn the_login_name_is_split_into_domain_and_user() {
        let password = SecretString::from("geheim");
        assert_eq!(
            Ntlm::new("Domain\\User", &password).account(),
            "Domain\\User"
        );
        assert_eq!(
            Ntlm::new("user@example.com", &password).account(),
            "example.com\\user"
        );
        assert_eq!(Ntlm::new("user", &password).account(), "user");
        // A login without a domain still produces a usable identity, and the
        // empty domain is simply left out of the name.
        assert_eq!(Ntlm::new("\\user", &password).account(), "user");
    }

    #[test]
    fn the_debug_output_never_contains_the_password() {
        let debug = format!("{:?}", Ntlm::new(LOGIN, &SecretString::from("geheim")));
        assert!(debug.contains("Domain"), "{debug}");
        assert!(!debug.contains("geheim"), "{debug}");
        assert!(debug.contains("REDACTED"), "{debug}");
    }

    #[test]
    fn a_random_client_challenge_and_a_plausible_timestamp_are_produced() {
        assert_ne!(client_challenge(), client_challenge());
        // 2020-01-01 as a Windows FILETIME; anything the server sees must be
        // more recent than that.
        const YEAR_2020: u64 = 132_223_104_000_000_000;
        assert!(filetime_now() > YEAR_2020);
        assert_eq!(EPOCH_DIFFERENCE_TICKS / TICKS_PER_SECOND, 11_644_473_600);
    }
}
