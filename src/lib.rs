// Clippy warns that it's dangerous to derive `PartialEq` and explicitly implement `Hash`, but the
// `pairing::bls12_381` types don't implement `Hash`, so we can't derive it.
#![cfg_attr(feature = "cargo-clippy", allow(derive_hash_xor_eq))]

#[cfg(test)]
extern crate bincode;
extern crate byteorder;
extern crate errno;
#[macro_use]
extern crate failure;
extern crate init_with;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
extern crate memsec;
extern crate pairing;
extern crate rand;
#[macro_use]
extern crate rand_derive;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate tiny_keccak;

pub mod error;
mod into_fr;
pub mod poly;
pub mod serde_impl;

use std::env;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::size_of_val;
use std::ptr::{copy_nonoverlapping, write_volatile};

use byteorder::{BigEndian, ByteOrder};
use errno::errno;
use init_with::InitWith;
use memsec::{memzero, mlock, munlock};
use pairing::bls12_381::{Bls12, Fr, G1, G1Affine, G2, G2Affine};
use pairing::{CurveAffine, CurveProjective, Engine, Field};
use rand::{ChaChaRng, OsRng, Rand, Rng, SeedableRng};
use tiny_keccak::sha3_256;

use error::{Error, Result};
use into_fr::IntoFr;
use poly::{Commitment, Poly};

lazy_static! {
    // Sets whether or not `mlock`ing is enabled. Memory locking is enabled by default; it can be
    // disabled by setting the environment variable `MLOCK_SECRETS=false`. This is useful when you
    // are running on a system where you do not have the ability to increase the systems locked
    // memory limit (which can be found using the Unix command: `ulimit -l`). For example, we
    // disable `mlock`ing of secrets when testing crates that depend on `threshold_crypto` when
    // running in Travis CI because Travis has a locked memory limit of 64kb, which we may exceed
    // while running `cargo test`. Disabling `mlock`ing for secret values allows secret keys to be
    // swapped/core-dumped to disk, resulting in unmanaged copies of secrets to hang around in
    // memory; this is significantly less secure than enabling memory locking (the default). Only
    // set `MLOCK_SECRETS=false` in development/testing.
    pub(crate) static ref SHOULD_MLOCK_SECRETS: bool = match env::var("MLOCK_SECRETS") {
        Ok(s) => s.parse().unwrap_or(true),
        _ => true,
    };
}

/// Marks a type as containing one or more secret prime field elements.
pub(crate) trait ContainsSecret {
    /// Calls the `mlock` system call on the region of memory allocated for the secret prime field
    /// element or elements. This results in that region of memory not being being copied to disk,
    /// either in a swap to disk or core dump. This method is called on every created instance of
    /// a secret type.
    ///
    /// # Errors
    ///
    /// An `Error::MlockFailed` is returned if we failed to `mlock` the secret data.
    fn mlock_secret_memory(&self) -> Result<()>;

    /// Undoes the `mlock` on the secret region of memory via the `munlock` system call.
    ///
    /// # Errors
    ///
    /// An `Error::MunlockFailed` is returned if we failed to `munlock` the secret data; this
    /// method is called on each secret type when it  goes out of scope.
    fn munlock_secret_memory(&self) -> Result<()>;

    /// Overwrites the secret prime field element or elements with zeros; this method is called on
    /// each each secret type when it  goes out of scope.
    fn zero_secret_memory(&self);
}

/// Wrapper for a byte array, whose `Debug` implementation outputs shortened hexadecimal strings.
pub struct HexBytes<'a>(pub &'a [u8]);

impl<'a> fmt::Debug for HexBytes<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0.len() > 6 {
            for byte in &self.0[..3] {
                write!(f, "{:02x}", byte)?;
            }
            write!(f, "..")?;
            for byte in &self.0[(self.0.len() - 3)..] {
                write!(f, "{:02x}", byte)?;
            }
        } else {
            for byte in self.0 {
                write!(f, "{:02x}", byte)?;
            }
        }
        Ok(())
    }
}

/// The number of words (`u32`) in a ChaCha RNG seed.
const CHACHA_RNG_SEED_SIZE: usize = 8;

const ERR_OS_RNG: &str = "could not initialize the OS random number generator";

/// A public key.
#[derive(Deserialize, Serialize, Copy, Clone, PartialEq, Eq)]
pub struct PublicKey(#[serde(with = "serde_impl::projective")] G1);

impl Hash for PublicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.into_affine().into_compressed().as_ref().hash(state);
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let uncomp = self.0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        write!(f, "PublicKey({:?})", HexBytes(bytes))
    }
}

impl PublicKey {
    /// Returns `true` if the signature matches the element of `G2`.
    pub fn verify_g2<H: Into<G2Affine>>(&self, sig: &Signature, hash: H) -> bool {
        Bls12::pairing(self.0, hash) == Bls12::pairing(G1Affine::one(), sig.0)
    }

    /// Returns `true` if the signature matches the message.
    pub fn verify<M: AsRef<[u8]>>(&self, sig: &Signature, msg: M) -> bool {
        self.verify_g2(sig, hash_g2(msg))
    }

    /// Encrypts the message.
    pub fn encrypt<M: AsRef<[u8]>>(&self, msg: M) -> Ciphertext {
        let r: Fr = OsRng::new().expect(ERR_OS_RNG).gen();
        let u = G1Affine::one().mul(r);
        let v: Vec<u8> = {
            let g = self.0.into_affine().mul(r);
            xor_vec(&hash_bytes(g, msg.as_ref().len()), msg.as_ref())
        };
        let w = hash_g1_g2(u, &v).into_affine().mul(r);
        Ciphertext(u, v, w)
    }

    /// Returns a byte string representation of the public key.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.into_affine().into_compressed().as_ref().to_vec()
    }
}

/// A public key share.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, Hash)]
pub struct PublicKeyShare(PublicKey);

impl fmt::Debug for PublicKeyShare {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let uncomp = (self.0).0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        write!(f, "PublicKeyShare({:?})", HexBytes(bytes))
    }
}

impl PublicKeyShare {
    /// Returns `true` if the signature matches the element of `G2`.
    pub fn verify_g2<H: Into<G2Affine>>(&self, sig: &SignatureShare, hash: H) -> bool {
        self.0.verify_g2(&sig.0, hash)
    }

    /// Returns `true` if the signature matches the message.
    pub fn verify<M: AsRef<[u8]>>(&self, sig: &SignatureShare, msg: M) -> bool {
        self.verify_g2(sig, hash_g2(msg))
    }

    /// Returns `true` if the decryption share matches the ciphertext.
    pub fn verify_decryption_share(&self, share: &DecryptionShare, ct: &Ciphertext) -> bool {
        let Ciphertext(ref u, ref v, ref w) = *ct;
        let hash = hash_g1_g2(*u, v);
        Bls12::pairing(share.0, hash) == Bls12::pairing((self.0).0, *w)
    }

    /// Returns a byte string representation of the public key share.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.to_bytes()
    }
}

/// A signature.
// Note: Random signatures can be generated for testing.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, Rand)]
pub struct Signature(#[serde(with = "serde_impl::projective")] G2);

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let uncomp = self.0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        write!(f, "Signature({:?})", HexBytes(bytes))
    }
}

impl Hash for Signature {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.into_affine().into_compressed().as_ref().hash(state);
    }
}

impl Signature {
    pub fn parity(&self) -> bool {
        let uncomp = self.0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        let xor_bytes: u8 = bytes.iter().fold(0, |result, byte| result ^ byte);
        let parity = 0 != xor_bytes.count_ones() % 2;
        debug!("Signature: {:?}, output: {}", HexBytes(bytes), parity);
        parity
    }
}

/// A signature share.
// Note: Random signature shares can be generated for testing.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, Rand, Hash)]
pub struct SignatureShare(pub Signature);

impl fmt::Debug for SignatureShare {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let uncomp = (self.0).0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        write!(f, "SignatureShare({:?})", HexBytes(bytes))
    }
}

/// A secret key; wraps a single prime field element. The field element is
/// heap allocated to avoid any stack copying that result when passing
/// `SecretKey`s between stack frames.
#[derive(PartialEq, Eq)]
pub struct SecretKey(Box<Fr>);

/// Creates a `SecretKey` containing the zero prime field element.
///
/// # Panics
///
/// Panics if we have hit the system's locked memory limit when `mlock`ing the new instance of
/// `SecretKey`.`
impl Default for SecretKey {
    fn default() -> Self {
        let mut fr = Fr::zero();
        match SecretKey::from_mut_ptr(&mut fr as *mut Fr) {
            Ok(sk) => sk,
            Err(e) => panic!("Failed to create default `SecretKey`: {}", e),
        }
    }
}

/// Creates a random `SecretKey`.
///
/// # Panics
///
/// Panics if we have hit the system's locked memory limit when `mlock`ing the new instance of
/// `SecretKey`.
impl Rand for SecretKey {
    fn rand<R: Rng>(rng: &mut R) -> Self {
        let mut fr = Fr::rand(rng);
        match SecretKey::from_mut_ptr(&mut fr as *mut Fr) {
            Ok(sk) => sk,
            Err(e) => panic!("Failed to create random `SecretKey`: {}", e),
        }
    }
}

/// Creates a new `SecretKey` by cloning another key's prime field element.
///
/// # Panics
///
/// Panics if we have hit the system's locked memory limit when `mlock`ing the new instance of
/// `SecretKey`.
impl Clone for SecretKey {
    fn clone(&self) -> Self {
        let mut fr = *self.0;
        match SecretKey::from_mut_ptr(&mut fr as *mut Fr) {
            Ok(sk) => sk,
            Err(e) => panic!("Failed to clone a new `SecretKey`: {}", e),
        }
    }
}

// A volatile overwrite of the prime field element's memory.
//
// # Panics
//
// Panics if we were unable to `munlock` the prime field element memory after it has been cleared.
impl Drop for SecretKey {
    fn drop(&mut self) {
        self.zero_secret_memory();
        if let Err(e) = self.munlock_secret_memory() {
            panic!("Failed to drop `SecretKey`: {}", e);
        }
    }
}

/// A debug statement where the secret prime field element is redacted.
impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SecretKey(...)")
    }
}

impl ContainsSecret for SecretKey {
    fn mlock_secret_memory(&self) -> Result<()> {
        if !*SHOULD_MLOCK_SECRETS {
            return Ok(());
        }
        let ptr = &*self.0 as *const Fr as *mut u8;
        let n_bytes = size_of_val(&*self.0);
        let mlock_succeeded = unsafe { mlock(ptr, n_bytes) };
        if mlock_succeeded {
            Ok(())
        } else {
            let e = Error::MlockFailed {
                errno: errno(),
                addr: format!("{:?}", ptr),
                n_bytes,
            };
            Err(e)
        }
    }

    fn munlock_secret_memory(&self) -> Result<()> {
        if !*SHOULD_MLOCK_SECRETS {
            return Ok(());
        }
        let ptr = &*self.0 as *const Fr as *mut u8;
        let n_bytes = size_of_val(&*self.0);
        let munlock_succeeded = unsafe { munlock(ptr, n_bytes) };
        if munlock_succeeded {
            Ok(())
        } else {
            let e = Error::MunlockFailed {
                errno: errno(),
                addr: format!("{:?}", ptr),
                n_bytes,
            };
            Err(e)
        }
    }

    fn zero_secret_memory(&self) {
        let ptr = &*self.0 as *const Fr as *mut u8;
        let n_bytes = size_of_val(&*self.0);
        unsafe {
            memzero(ptr, n_bytes);
        }
    }
}

impl SecretKey {
    /// Creates a new `SecretKey` given a mutable raw pointer to a prime
    /// field element. This constructor takes a pointer to avoid any
    /// unnecessary stack copying/moving of secrets. The field element will
    /// be copied bytewise onto the heap, the resulting `Box` is then
    /// stored in the `SecretKey`.
    ///
    /// *WARNING* this constructor will overwrite the pointed to `Fr` element
    /// with zeros after it has been copied onto the heap.
    ///
    /// # Errors
    ///
    /// Returns an `Error::MlockFailed` if we have reached the systems's
    /// locked memory limit.
    #[cfg_attr(feature = "cargo-clippy", allow(not_unsafe_ptr_arg_deref))]
    pub fn from_mut_ptr(fr_ptr: *mut Fr) -> Result<Self> {
        let mut boxed_fr = Box::new(Fr::zero());
        unsafe {
            copy_nonoverlapping(fr_ptr, &mut *boxed_fr as *mut Fr, 1);
            write_volatile(fr_ptr, Fr::zero());
        }
        let sk = SecretKey(boxed_fr);
        sk.mlock_secret_memory()?;
        Ok(sk)
    }

    /// Creates a new random instance of `SecretKey`. This is used
    /// as a  wrapper around: `let sk: SecretKey = rand::random();`.
    ///
    /// # Panics
    ///
    /// Panics if we have hit the system's locked memory limit when
    /// `mlock`ing the new instance of `SecretKey`.
    pub fn random() -> Self {
        use rand::thread_rng;
        let mut rng = thread_rng();
        SecretKey::rand(&mut rng)
    }

    /// Returns the matching public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(G1Affine::one().mul(*self.0))
    }

    /// Signs the given element of `G2`.
    pub fn sign_g2<H: Into<G2Affine>>(&self, hash: H) -> Signature {
        Signature(hash.into().mul(*self.0))
    }

    /// Signs the given message.
    pub fn sign<M: AsRef<[u8]>>(&self, msg: M) -> Signature {
        self.sign_g2(hash_g2(msg))
    }

    /// Returns the decrypted text, or `None`, if the ciphertext isn't valid.
    pub fn decrypt(&self, ct: &Ciphertext) -> Option<Vec<u8>> {
        if !ct.verify() {
            return None;
        }
        let Ciphertext(ref u, ref v, _) = *ct;
        let g = u.into_affine().mul(*self.0);
        Some(xor_vec(&hash_bytes(g, v.len()), v))
    }

    /// Generates a non-redacted debug string. This method differs from
    /// the `Debug` implementation in that it *does* leak the secret prime
    /// field element.
    pub fn reveal(&self) -> String {
        let uncomp = self.public_key().0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        format!("SecretKey({:?})", HexBytes(bytes))
    }
}

/// A secret key share.
#[derive(Clone, PartialEq, Eq, Rand, Default)]
pub struct SecretKeyShare(SecretKey);

impl fmt::Debug for SecretKeyShare {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let uncomp = self.0.public_key().0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        write!(f, "SecretKeyShare({:?})", HexBytes(bytes))
    }
}

impl SecretKeyShare {
    /// Creates a secret key share from an existing value. This constructor
    /// takes a pointer to avoid any unnecessary stack copying/moving of
    /// secrets. The field element will be copied bytewise onto the heap,
    /// the resulting `Box` is then stored in the `SecretKey` which is then
    /// wrapped in a `SecretKeyShare`.
    ///
    /// *WARNING* this constructor will overwrite the pointed to `Fr` element
    /// with zeros once it has been copied into a new `SecretKeyShare`.
    ///
    /// # Errors
    ///
    /// Returns an `Error::MlockFailed` if we have reached the systems's
    /// locked memory limit.
    pub fn from_mut_ptr(fr_ptr: *mut Fr) -> Result<Self> {
        SecretKey::from_mut_ptr(fr_ptr).map(SecretKeyShare)
    }

    /// Returns the matching public key share.
    pub fn public_key_share(&self) -> PublicKeyShare {
        PublicKeyShare(self.0.public_key())
    }

    /// Signs the given element of `G2`.
    pub fn sign_g2<H: Into<G2Affine>>(&self, hash: H) -> SignatureShare {
        SignatureShare(self.0.sign_g2(hash))
    }

    /// Signs the given message.
    pub fn sign<M: AsRef<[u8]>>(&self, msg: M) -> SignatureShare {
        SignatureShare(self.0.sign(msg))
    }

    /// Returns a decryption share, or `None`, if the ciphertext isn't valid.
    pub fn decrypt_share(&self, ct: &Ciphertext) -> Option<DecryptionShare> {
        if !ct.verify() {
            return None;
        }
        Some(self.decrypt_share_no_verify(ct))
    }

    /// Returns a decryption share, without validating the ciphertext.
    pub fn decrypt_share_no_verify(&self, ct: &Ciphertext) -> DecryptionShare {
        DecryptionShare(ct.0.into_affine().mul(*(self.0).0))
    }

    /// Generates a non-redacted debug string. This method differs from
    /// the `Debug` implementation in that it *does* leak the secret prime
    /// field element.
    pub fn reveal(&self) -> String {
        let uncomp = self.0.public_key().0.into_affine().into_uncompressed();
        let bytes = uncomp.as_ref();
        format!("SecretKeyShare({:?})", HexBytes(bytes))
    }
}

/// An encrypted message.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Ciphertext(
    #[serde(with = "serde_impl::projective")] G1,
    Vec<u8>,
    #[serde(with = "serde_impl::projective")] G2,
);

impl Hash for Ciphertext {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Ciphertext(ref u, ref v, ref w) = *self;
        u.into_affine().into_compressed().as_ref().hash(state);
        v.hash(state);
        w.into_affine().into_compressed().as_ref().hash(state);
    }
}

impl Ciphertext {
    /// Returns `true` if this is a valid ciphertext. This check is necessary to prevent
    /// chosen-ciphertext attacks.
    pub fn verify(&self) -> bool {
        let Ciphertext(ref u, ref v, ref w) = *self;
        let hash = hash_g1_g2(*u, v);
        Bls12::pairing(G1Affine::one(), *w) == Bls12::pairing(*u, hash)
    }
}

/// A decryption share. A threshold of decryption shares can be used to decrypt a message.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq, Rand)]
pub struct DecryptionShare(#[serde(with = "serde_impl::projective")] G1);

impl Hash for DecryptionShare {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.into_affine().into_compressed().as_ref().hash(state);
    }
}

/// A public key and an associated set of public key shares.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PublicKeySet {
    /// The coefficients of a polynomial whose value at `0` is the "master key", and value at
    /// `i + 1` is key share number `i`.
    commit: Commitment,
}

impl Hash for PublicKeySet {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.commit.hash(state);
    }
}

impl From<Commitment> for PublicKeySet {
    fn from(commit: Commitment) -> PublicKeySet {
        PublicKeySet { commit }
    }
}

impl PublicKeySet {
    /// Returns the threshold `t`: any set of `t + 1` signature shares can be combined into a full
    /// signature.
    pub fn threshold(&self) -> usize {
        self.commit.degree()
    }

    /// Returns the public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.commit.coeff[0])
    }

    /// Returns the `i`-th public key share.
    pub fn public_key_share<T: IntoFr>(&self, i: T) -> PublicKeyShare {
        let value = self.commit.evaluate(into_fr_plus_1(i));
        PublicKeyShare(PublicKey(value))
    }

    /// Combines the shares into a signature that can be verified with the main public key.
    pub fn combine_signatures<'a, T, I>(&self, shares: I) -> Result<Signature>
    where
        I: IntoIterator<Item = (T, &'a SignatureShare)>,
        T: IntoFr,
    {
        let samples = shares.into_iter().map(|(i, share)| (i, &(share.0).0));
        Ok(Signature(interpolate(self.commit.degree() + 1, samples)?))
    }

    /// Combines the shares to decrypt the ciphertext.
    pub fn decrypt<'a, T, I>(&self, shares: I, ct: &Ciphertext) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = (T, &'a DecryptionShare)>,
        T: IntoFr,
    {
        let samples = shares.into_iter().map(|(i, share)| (i, &share.0));
        let g = interpolate(self.commit.degree() + 1, samples)?;
        Ok(xor_vec(&hash_bytes(g, ct.1.len()), &ct.1))
    }
}

/// A secret key and an associated set of secret key shares.
pub struct SecretKeySet {
    /// The coefficients of a polynomial whose value at `0` is the "master key", and value at
    /// `i + 1` is key share number `i`.
    poly: Poly,
}

impl From<Poly> for SecretKeySet {
    fn from(poly: Poly) -> SecretKeySet {
        SecretKeySet { poly }
    }
}

impl SecretKeySet {
    /// Creates a set of secret key shares, where any `threshold + 1` of them can collaboratively
    /// sign and decrypt.
    pub fn random<R: Rng>(threshold: usize, rng: &mut R) -> Result<Self> {
        let poly = Poly::random(threshold, rng)?;
        Ok(SecretKeySet { poly })
    }

    /// Returns the threshold `t`: any set of `t + 1` signature shares can be combined into a full
    /// signature.
    pub fn threshold(&self) -> usize {
        self.poly.degree()
    }

    /// Returns the `i`-th secret key share.
    pub fn secret_key_share<T: IntoFr>(&self, i: T) -> Result<SecretKeyShare> {
        let mut fr = self.poly.evaluate(into_fr_plus_1(i));
        SecretKeyShare::from_mut_ptr(&mut fr as *mut Fr)
    }

    /// Returns the corresponding public key set. That information can be shared publicly.
    pub fn public_keys(&self) -> PublicKeySet {
        PublicKeySet {
            commit: self.poly.commitment(),
        }
    }

    /// Returns the secret master key.
    #[cfg(test)]
    fn secret_key(&self) -> Result<SecretKey> {
        let mut fr = self.poly.evaluate(0);
        SecretKey::from_mut_ptr(&mut fr as *mut Fr)
    }
}

/// Returns a hash of the given message in `G2`.
fn hash_g2<M: AsRef<[u8]>>(msg: M) -> G2 {
    let digest = sha3_256(msg.as_ref());
    let seed = <[u32; CHACHA_RNG_SEED_SIZE]>::init_with_indices(|i| {
        BigEndian::read_u32(&digest.as_ref()[(4 * i)..(4 * i + 4)])
    });
    let mut rng = ChaChaRng::from_seed(&seed);
    rng.gen()
}

/// Returns a hash of the group element and message, in the second group.
fn hash_g1_g2<M: AsRef<[u8]>>(g1: G1, msg: M) -> G2 {
    // If the message is large, hash it, otherwise copy it.
    // TODO: Benchmark and optimize the threshold.
    let mut msg = if msg.as_ref().len() > 64 {
        sha3_256(msg.as_ref()).to_vec()
    } else {
        msg.as_ref().to_vec()
    };
    msg.extend(g1.into_affine().into_compressed().as_ref());
    hash_g2(&msg)
}

/// Returns a hash of the group element with the specified length in bytes.
fn hash_bytes(g1: G1, len: usize) -> Vec<u8> {
    let digest = sha3_256(g1.into_affine().into_compressed().as_ref());
    let seed = <[u32; CHACHA_RNG_SEED_SIZE]>::init_with_indices(|i| {
        BigEndian::read_u32(&digest.as_ref()[(4 * i)..(4 * i + 4)])
    });
    let mut rng = ChaChaRng::from_seed(&seed);
    rng.gen_iter().take(len).collect()
}

/// Returns the bitwise xor.
fn xor_vec(x: &[u8], y: &[u8]) -> Vec<u8> {
    x.iter().zip(y).map(|(a, b)| a ^ b).collect()
}

/// Given a list of `t` samples `(i - 1, f(i) * g)` for a polynomial `f` of degree `t - 1`, and a
/// group generator `g`, returns `f(0) * g`.
fn interpolate<'a, C, T, I>(t: usize, items: I) -> Result<C>
where
    C: CurveProjective<Scalar = Fr>,
    I: IntoIterator<Item = (T, &'a C)>,
    T: IntoFr,
{
    let samples: Vec<_> = items
        .into_iter()
        .map(|(i, sample)| (into_fr_plus_1(i), sample))
        .collect();
    if samples.len() < t {
        return Err(Error::NotEnoughShares);
    }
    let mut result = C::zero();
    let mut indexes = Vec::new();
    for (x, sample) in samples.iter().take(t) {
        if indexes.contains(x) {
            return Err(Error::DuplicateEntry);
        }
        indexes.push(x.clone());
        // Compute the value at 0 of the Lagrange polynomial that is `0` at the other data
        // points but `1` at `x`.
        let mut l0 = C::Scalar::one();
        for (x0, _) in samples.iter().take(t).filter(|(x0, _)| x0 != x) {
            let mut denom = *x0;
            denom.sub_assign(x);
            l0.mul_assign(x0);
            l0.mul_assign(&denom.inverse().expect("indices are different"));
        }
        result.add_assign(&sample.into_affine().mul(l0));
    }
    Ok(result)
}

fn into_fr_plus_1<I: IntoFr>(x: I) -> Fr {
    let mut result = Fr::one();
    result.add_assign(&x.into_fr());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use rand::{self, random};

    #[test]
    fn test_simple_sig() {
        let sk0: SecretKey = random();
        let sk1: SecretKey = random();
        let pk0 = sk0.public_key();
        let msg0 = b"Real news";
        let msg1 = b"Fake news";
        assert!(pk0.verify(&sk0.sign(msg0), msg0));
        assert!(!pk0.verify(&sk1.sign(msg0), msg0)); // Wrong key.
        assert!(!pk0.verify(&sk0.sign(msg1), msg0)); // Wrong message.
    }

    #[test]
    fn test_threshold_sig() {
        let mut rng = rand::thread_rng();
        let sk_set = SecretKeySet::random(3, &mut rng).expect("Failed to create `SecretKeySet`");
        let pk_set = sk_set.public_keys();
        let pk_master = pk_set.public_key();

        // Make sure the keys are different, and the first coefficient is the main key.
        assert_ne!(pk_master, pk_set.public_key_share(0).0);
        assert_ne!(pk_master, pk_set.public_key_share(1).0);
        assert_ne!(pk_master, pk_set.public_key_share(2).0);

        // Make sure we don't hand out the main secret key to anyone.
        let sk_master = sk_set
            .secret_key()
            .expect("Failed to create master `SecretKey`");
        let sk_share_0 = sk_set
            .secret_key_share(0)
            .expect("Failed to create first `SecretKeyShare`")
            .0;
        let sk_share_1 = sk_set
            .secret_key_share(1)
            .expect("Failed to create second `SecretKeyShare`")
            .0;
        let sk_share_2 = sk_set
            .secret_key_share(2)
            .expect("Failed to create third `SecretKeyShare`")
            .0;
        assert_ne!(sk_master, sk_share_0);
        assert_ne!(sk_master, sk_share_1);
        assert_ne!(sk_master, sk_share_2);

        let msg = "Totally real news";

        // The threshold is 3, so 4 signature shares will suffice to recreate the share.
        let sigs: BTreeMap<_, _> = [5, 8, 7, 10]
            .iter()
            .map(|&i| {
                let sig = sk_set
                    .secret_key_share(i)
                    .unwrap_or_else(|_| panic!("Failed to create `SecretKeyShare` #{}", i))
                    .sign(msg);
                (i, sig)
            })
            .collect();

        // Each of the shares is a valid signature matching its public key share.
        for (i, sig) in &sigs {
            assert!(pk_set.public_key_share(*i).verify(sig, msg));
        }

        // Combined, they produce a signature matching the main public key.
        let sig = pk_set.combine_signatures(&sigs).expect("signatures match");
        assert!(pk_set.public_key().verify(&sig, msg));

        // A different set of signatories produces the same signature.
        let sigs2: BTreeMap<_, _> = [42, 43, 44, 45]
            .iter()
            .map(|&i| {
                let sig = sk_set
                    .secret_key_share(i)
                    .unwrap_or_else(|_| panic!("Failed to create `SecretKeyShare` #{}", i))
                    .sign(msg);
                (i, sig)
            })
            .collect();
        let sig2 = pk_set.combine_signatures(&sigs2).expect("signatures match");
        assert_eq!(sig, sig2);
    }

    #[test]
    fn test_simple_enc() {
        let sk_bob: SecretKey = random();
        let sk_eve: SecretKey = random();
        let pk_bob = sk_bob.public_key();
        let msg = b"Muffins in the canteen today! Don't tell Eve!";
        let ciphertext = pk_bob.encrypt(&msg[..]);
        assert!(ciphertext.verify());

        // Bob can decrypt the message.
        let decrypted = sk_bob.decrypt(&ciphertext).expect("valid ciphertext");
        assert_eq!(msg[..], decrypted[..]);

        // Eve can't.
        let decrypted_eve = sk_eve.decrypt(&ciphertext).expect("valid ciphertext");
        assert_ne!(msg[..], decrypted_eve[..]);

        // Eve tries to trick Bob into decrypting `msg` xor `v`, but it doesn't validate.
        let Ciphertext(u, v, w) = ciphertext;
        let fake_ciphertext = Ciphertext(u, vec![0; v.len()], w);
        assert!(!fake_ciphertext.verify());
        assert_eq!(None, sk_bob.decrypt(&fake_ciphertext));
    }

    #[test]
    fn test_threshold_enc() {
        let mut rng = rand::thread_rng();
        let sk_set = SecretKeySet::random(3, &mut rng).expect("Failed to create to `SecretKeySet`");
        let pk_set = sk_set.public_keys();
        let msg = b"Totally real news";
        let ciphertext = pk_set.public_key().encrypt(&msg[..]);

        // The threshold is 3, so 4 signature shares will suffice to decrypt.
        let shares: BTreeMap<_, _> = [5, 8, 7, 10]
            .iter()
            .map(|&i| {
                let dec_share = sk_set
                    .secret_key_share(i)
                    .unwrap_or_else(|_| panic!("Failed to create `SecretKeyShare` #{}", i))
                    .decrypt_share(&ciphertext)
                    .expect("ciphertext is valid");
                (i, dec_share)
            })
            .collect();

        // Each of the shares is valid matching its public key share.
        for (i, share) in &shares {
            pk_set
                .public_key_share(*i)
                .verify_decryption_share(share, &ciphertext);
        }

        // Combined, they can decrypt the message.
        let decrypted = pk_set
            .decrypt(&shares, &ciphertext)
            .expect("decryption shares match");
        assert_eq!(msg[..], decrypted[..]);
    }

    /// Some basic sanity checks for the `hash_g2` function.
    #[test]
    fn test_hash_g2() {
        let mut rng = rand::thread_rng();
        let msg: Vec<u8> = (0..1000).map(|_| rng.gen()).collect();
        let msg_end0: Vec<u8> = msg.iter().chain(b"end0").cloned().collect();
        let msg_end1: Vec<u8> = msg.iter().chain(b"end1").cloned().collect();

        assert_eq!(hash_g2(&msg), hash_g2(&msg));
        assert_ne!(hash_g2(&msg), hash_g2(&msg_end0));
        assert_ne!(hash_g2(&msg_end0), hash_g2(&msg_end1));
    }

    /// Some basic sanity checks for the `hash_g1_g2` function.
    #[test]
    fn test_hash_g1_g2() {
        let mut rng = rand::thread_rng();
        let msg: Vec<u8> = (0..1000).map(|_| rng.gen()).collect();
        let msg_end0: Vec<u8> = msg.iter().chain(b"end0").cloned().collect();
        let msg_end1: Vec<u8> = msg.iter().chain(b"end1").cloned().collect();
        let g0 = rng.gen();
        let g1 = rng.gen();

        assert_eq!(hash_g1_g2(g0, &msg), hash_g1_g2(g0, &msg));
        assert_ne!(hash_g1_g2(g0, &msg), hash_g1_g2(g0, &msg_end0));
        assert_ne!(hash_g1_g2(g0, &msg_end0), hash_g1_g2(g0, &msg_end1));
        assert_ne!(hash_g1_g2(g0, &msg), hash_g1_g2(g1, &msg));
    }

    /// Some basic sanity checks for the `hash_bytes` function.
    #[test]
    fn test_hash_bytes() {
        let mut rng = rand::thread_rng();
        let g0 = rng.gen();
        let g1 = rng.gen();
        let hash = hash_bytes;
        assert_eq!(hash(g0, 5), hash(g0, 5));
        assert_ne!(hash(g0, 5), hash(g1, 5));
        assert_eq!(5, hash(g0, 5).len());
        assert_eq!(6, hash(g0, 6).len());
        assert_eq!(20, hash(g0, 20).len());
    }

    #[test]
    fn test_serde() {
        use bincode;

        let sk: SecretKey = random();
        let sig = sk.sign("Please sign here: ______");
        let pk = sk.public_key();
        let ser_pk = bincode::serialize(&pk).expect("serialize public key");
        let deser_pk = bincode::deserialize(&ser_pk).expect("deserialize public key");
        assert_eq!(pk, deser_pk);
        let ser_sig = bincode::serialize(&sig).expect("serialize signature");
        let deser_sig = bincode::deserialize(&ser_sig).expect("deserialize signature");
        assert_eq!(sig, deser_sig);
    }
}
