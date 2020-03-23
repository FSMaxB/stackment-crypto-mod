// SPDX-License-Identifier: GPL-3.0-or-later
/*
 * Copyright (C) 2020 Daniel Vogelbacher
 * Written by: Daniel Vogelbacher <daniel@chaospixel.com>
 */

use std::ops::Deref;

use std::fmt;
use std::io::Read;
use std::io::Write;

use chrono::Utc;
use ring::{self, signature::UnparsedPublicKey};
use ring::{
    aead, digest, pbkdf2, signature,
    signature::{Ed25519KeyPair, KeyPair, Signature},
};
use snow;
use yasna::{self, models::GeneralizedTime, models::ObjectIdentifier, Tag};

use x25519_dalek as x25519;

use rand::Rng;
use rand::rngs::OsRng;

use crate::crypto::{CertVariant, PublicVariant, SecretVariant};

type Seed = [u8; SEED_LEN];
const SEED_LEN: usize = 32;

use crate::crypto::{
    validate_signature, Cert, DeviceCert, Encrypted, Fingerprint, IdentCert, Public, Secret,
    SignatureBytes, Trusted, Untrusted,
};

/// Public part of a Alpha keyring, consists of:
///  * ED25519 key for signing
///  * X25519 key for agreement and crypto
pub struct AlphaPublic {
    ed25519_pubkey: Vec<u8>,
    x25519_pubkey: x25519::PublicKey,
}

/// Secret part of a Alpha keyring, consists of:
///  * ED25519 key for signing
///  * X25519 key for agreement and crypto
pub struct AlphaSecret {
    /// Ring constructs a ED25519 keypair from a Seed which is usually
    /// a 32 byte random value. Ring checks for consistency when loaded
    /// with a public key. We store this here for serialization purpose
    /// because the seed cannot extracted from Ed25519KeyPair
    ed25519_seed: Seed,
    /// The ED25519 Keypair (private and public)
    ed25519_keypair: Ed25519KeyPair,
    /// A static secret generated by X25519
    x25519_secret: x25519::StaticSecret,
    /// The public keys for this secret
    pubkey: AlphaPublic,
}

impl AlphaSecret {
    /// Construct a ne AlphaSecret with an ED25519 and X25519 keypair
    pub fn new() -> Self {
        let mut rng = OsRng::default();
        let ed25519_seed: [u8; SEED_LEN] = rng.gen();
        let ed25519_keypair = Ed25519KeyPair::from_seed_unchecked(&ed25519_seed).unwrap();
        let x25519_secret = x25519::StaticSecret::new(&mut rng);
        let ed25519_pubkey = Vec::from(ed25519_keypair.public_key().as_ref());
        let x25519_pubkey = x25519::PublicKey::from(&x25519_secret);
        Self {
            ed25519_seed,
            ed25519_keypair,
            x25519_secret,
            pubkey: AlphaPublic {
                ed25519_pubkey,
                x25519_pubkey,
            },
        }
    }

    /// Returns the public key parts for this secret
    pub fn public_key(&self) -> &AlphaPublic {
        &self.pubkey
    }
}

impl Secret for AlphaSecret {
    type PublicKey = AlphaPublic;

    fn sign(&self, bytes: &[u8]) -> SignatureBytes {
        self.ed25519_keypair.sign(bytes).as_ref().into()
    }

    fn decrypt(&self, enc_bytes: &Encrypted, _sender_pubkey: &AlphaPublic) -> Vec<u8> {
        let mut in_out = enc_bytes.data.clone();
        let mut raw_ephemeral_pubkey = [0; 32];
        let bytes = &enc_bytes.ephemeral_pubkey[..raw_ephemeral_pubkey.len()];
        raw_ephemeral_pubkey.copy_from_slice(bytes);
        let ephemeral_pub = x25519::PublicKey::from(raw_ephemeral_pubkey);
        // DH
        let shared_secret = self.x25519_secret.diffie_hellman(&ephemeral_pub);

        let salt = [0];

        let mut kdf_input = Vec::new();
        kdf_input.extend(shared_secret.as_bytes());
        kdf_input.extend(ephemeral_pub.as_bytes());
        kdf_input.extend(self.public_key().encryption_public_key());
        let mut key = [0; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(1000).unwrap(),
            &salt,
            &kdf_input,
            &mut key,
        );

        let mut opening_key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key).expect("opening key"),
        );

        let nonce =
            aead::Nonce::assume_unique_for_key([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let decrypted_data = opening_key
            .open_in_place(nonce, aead::Aad::empty(), &mut in_out)
            .expect("opening failed");
        Vec::from(decrypted_data)
    }

    fn encrypt(&self, plain_bytes: &[u8], peer_public: &AlphaPublic) -> Encrypted {
        // Generate an ephemeral x25519 key
        let ephemeral_key = x25519::EphemeralSecret::new(&mut OsRng::default());
        let ephemeral_pub = x25519::PublicKey::from(&ephemeral_key);
        // DH
        let shared_secret = ephemeral_key.diffie_hellman(&peer_public.x25519_pubkey);
        // This is controversal:
        // The shared_secret is always used once because of the ephemeral key.
        // ring::derive needs a salt and in this case it should be save
        // to put in a static salt to prevent sending an additional salt value
        // to the receiver.
        let salt = [0];
        // for KDF, the RFC 7748 6.1 recommends to use the shared secret + P1 + P2
        // as input for a KDF.
        let mut kdf_input = Vec::new();
        kdf_input.extend(shared_secret.as_bytes());
        kdf_input.extend(ephemeral_pub.as_bytes());
        kdf_input.extend(peer_public.x25519_pubkey.as_bytes());
        let mut key = [0; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(1000).unwrap(),
            &salt,
            &kdf_input,
            &mut key,
        );
        // Encrypt data
        let mut in_out = Vec::from(plain_bytes);
        let mut sealing_key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key).expect("sealing key"),
        );
        // Because the key is used only once and this is one single encryption step,
        // we can work with a simple nonce.
        let nonce =
            aead::Nonce::assume_unique_for_key([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        sealing_key
            .seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut in_out)
            .expect("sealing failed");
        Encrypted {
            ephemeral_pubkey: Vec::from(&ephemeral_pub.as_bytes()[..]),
            data: in_out,
        }
    }

    /// Serialize the secret as ASN.1 date to `stream`.
    /// TODO: Insert ASN.1 schema here
    fn serialize(&self, stream: &mut dyn Write) {
        let raw_bytes = yasna::construct_der(|writer| {
            writer.write_sequence(|writer| {
                writer.next().write_i64(0xfe73ba2003); // Magic
                writer.next().write_u8(1); // Private key
                writer.next().write_i64(1); // Version
                writer.next().write_bytes(&self.ed25519_seed);
                writer
                    .next()
                    .write_bytes(self.ed25519_keypair.public_key().as_ref());
                writer.next().write_bytes(&self.x25519_secret.to_bytes());
                writer
                    .next()
                    .write_bytes(self.pubkey.x25519_pubkey.as_bytes());
            });
        });
        stream.write_all(&raw_bytes).unwrap();
    }
}

impl Public for AlphaPublic {
    fn signing_public_key(&self) -> &[u8] {
        &self.ed25519_pubkey
    }

    fn encryption_public_key(&self) -> &[u8] {
        self.x25519_pubkey.as_bytes()
    }

    fn verify(&self, bytes: &[u8], signature: &SignatureBytes) -> bool {
        let public_key = UnparsedPublicKey::new(&signature::ED25519, self.signing_public_key());
        public_key
            .verify(bytes, signature.as_bytes())
            .is_ok()
    }
}
