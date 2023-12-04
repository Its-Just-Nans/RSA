use alloc::vec::Vec;
use core::cmp::Ordering;
use core::hash::{Hash, Hasher};
use crypto_bigint::modular::{BoxedResidue, BoxedResidueParams};
use crypto_bigint::{BoxedUint, Limb, NonZero};
use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::{FromPrimitive, ToPrimitive};
use rand_core::CryptoRngCore;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::algorithms::generate::generate_multi_prime_key_with_exp;
use crate::algorithms::rsa::{
    compute_modulus, compute_private_exponent_carmicheal, compute_private_exponent_euler_totient,
    recover_primes,
};

use crate::dummy_rng::DummyRng;
use crate::errors::{Error, Result};
use crate::traits::keys::{CrtValueNew, PrivateKeyPartsNew, PublicKeyPartsNew};
use crate::traits::{PaddingScheme, PublicKeyParts, SignatureScheme};

/// Represents the public part of an RSA key.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RsaPublicKey {
    /// Modulus: product of prime numbers `p` and `q`
    n: NonZero<BoxedUint>,
    /// Public exponent: power to which a plaintext message is raised in
    /// order to encrypt it.
    ///
    /// Typically 0x10001 (65537)
    e: u64,

    #[cfg_attr(feature = "serde", serde(skip))]
    n_params: BoxedResidueParams,
}

impl Eq for RsaPublicKey {}
impl PartialEq for RsaPublicKey {
    #[inline]
    fn eq(&self, other: &RsaPublicKey) -> bool {
        self.n == other.n && self.e == other.e
    }
}

impl Hash for RsaPublicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Domain separator for RSA private keys
        state.write(b"RsaPublicKey");
        Hash::hash(&self.n, state);
        Hash::hash(&self.e, state);
    }
}

/// Represents a whole RSA key, public and private parts.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RsaPrivateKey {
    /// Public components of the private key.
    pubkey_components: RsaPublicKey,
    /// Private exponent
    pub(crate) d: BoxedUint,
    /// Prime factors of N, contains >= 2 elements.
    pub(crate) primes: Vec<BoxedUint>,
    /// precomputed values to speed up private operations
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) precomputed: Option<PrecomputedValues>,
}

impl Eq for RsaPrivateKey {}
impl PartialEq for RsaPrivateKey {
    #[inline]
    fn eq(&self, other: &RsaPrivateKey) -> bool {
        self.pubkey_components == other.pubkey_components
            && self.d == other.d
            && self.primes == other.primes
    }
}

impl AsRef<RsaPublicKey> for RsaPrivateKey {
    fn as_ref(&self) -> &RsaPublicKey {
        &self.pubkey_components
    }
}

impl Hash for RsaPrivateKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Domain separator for RSA private keys
        state.write(b"RsaPrivateKey");
        Hash::hash(&self.pubkey_components, state);
    }
}

impl Drop for RsaPrivateKey {
    fn drop(&mut self) {
        self.d.zeroize();
        self.primes.zeroize();
        self.precomputed.zeroize();
    }
}

impl ZeroizeOnDrop for RsaPrivateKey {}

#[derive(Debug, Clone)]
pub(crate) struct PrecomputedValues {
    /// D mod (P-1)
    pub(crate) dp: BoxedUint,
    /// D mod (Q-1)
    pub(crate) dq: BoxedUint,
    /// Q^-1 mod P
    pub(crate) qinv: BoxedResidue,

    pub(crate) p_params: BoxedResidueParams,
    pub(crate) q_params: BoxedResidueParams,
}

impl Zeroize for PrecomputedValues {
    fn zeroize(&mut self) {
        self.dp.zeroize();
        self.dq.zeroize();
    }
}

impl Drop for PrecomputedValues {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl From<RsaPrivateKey> for RsaPublicKey {
    fn from(private_key: RsaPrivateKey) -> Self {
        (&private_key).into()
    }
}

impl From<&RsaPrivateKey> for RsaPublicKey {
    fn from(private_key: &RsaPrivateKey) -> Self {
        let n = PublicKeyPartsNew::n(private_key);
        let e = PublicKeyPartsNew::e(private_key);
        let n_params = PublicKeyPartsNew::n_params(private_key);
        RsaPublicKey {
            n: n.clone(),
            e: e.clone(),
            n_params,
        }
    }
}

impl PublicKeyPartsNew for RsaPublicKey {
    fn n(&self) -> &NonZero<BoxedUint> {
        &self.n
    }

    fn e(&self) -> u64 {
        self.e
    }

    fn n_params(&self) -> BoxedResidueParams {
        self.n_params.clone()
    }
}

impl RsaPublicKey {
    /// Encrypt the given message.
    pub fn encrypt<R: CryptoRngCore, P: PaddingScheme>(
        &self,
        rng: &mut R,
        padding: P,
        msg: &[u8],
    ) -> Result<Vec<u8>> {
        padding.encrypt(rng, self, msg)
    }

    /// Verify a signed message.
    ///
    /// `hashed` must be the result of hashing the input using the hashing function
    /// passed in through `hash`.
    ///
    /// If the message is valid `Ok(())` is returned, otherwise an `Err` indicating failure.
    pub fn verify<S: SignatureScheme>(&self, scheme: S, hashed: &[u8], sig: &[u8]) -> Result<()> {
        scheme.verify(self, hashed, sig)
    }
}

impl RsaPublicKey {
    /// Minimum value of the public exponent `e`.
    pub const MIN_PUB_EXPONENT: u64 = 2;

    /// Maximum value of the public exponent `e`.
    pub const MAX_PUB_EXPONENT: u64 = (1 << 33) - 1;

    /// Maximum size of the modulus `n` in bits.
    pub const MAX_SIZE: usize = 4096;

    /// Create a new public key from its components.
    ///
    /// This function accepts public keys with a modulus size up to 4096-bits,
    /// i.e. [`RsaPublicKey::MAX_SIZE`].
    pub fn new(n: BigUint, e: BigUint) -> Result<Self> {
        Self::new_with_max_size(n, e, Self::MAX_SIZE)
    }

    /// Create a new public key from its components.
    pub fn new_with_max_size(n: BigUint, e: BigUint, max_size: usize) -> Result<Self> {
        check_public_with_max_size(&n, &e, max_size)?;
        let e = e.to_u64().unwrap();

        let raw_n = to_uint(n);
        let n_params = BoxedResidueParams::new(raw_n.clone()).unwrap();
        let n = NonZero::new(raw_n).unwrap();

        let k = Self { n, e, n_params };

        Ok(k)
    }

    /// Create a new public key, bypassing checks around the modulus and public
    /// exponent size.
    ///
    /// This method is not recommended, and only intended for unusual use cases.
    /// Most applications should use [`RsaPublicKey::new`] or
    /// [`RsaPublicKey::new_with_max_size`] instead.
    pub fn new_unchecked(n: BigUint, e: BigUint) -> Self {
        let raw_n = to_uint(n);
        let n_params = BoxedResidueParams::new(raw_n.clone()).unwrap();
        let n = NonZero::new(raw_n).unwrap();
        let e = e.to_u64().unwrap();
        Self { n, e, n_params }
    }
}

fn needed_bits(n: &BigUint) -> u32 {
    // widen to the max size bits
    let n_bits = n.bits();

    // TODO: better algorithm/more sizes
    if n_bits <= 64 {
        64
    } else if n_bits <= 128 {
        128
    } else if n_bits <= 256 {
        256
    } else if n_bits <= 512 {
        512
    } else if n_bits <= 1024 {
        1024
    } else if n_bits <= 2048 {
        2048
    } else if n_bits <= 4096 {
        4096
    } else if n_bits <= 8192 {
        8192
    } else {
        16384
    }
}

impl PublicKeyPartsNew for RsaPrivateKey {
    fn n(&self) -> &NonZero<BoxedUint> {
        &self.pubkey_components.n
    }

    fn e(&self) -> u64 {
        self.pubkey_components.e
    }

    fn n_params(&self) -> BoxedResidueParams {
        self.pubkey_components.n_params.clone()
    }
}

impl RsaPrivateKey {
    /// Default exponent for RSA keys.
    const EXP: u64 = 65537;

    /// Generate a new Rsa key pair of the given bit size using the passed in `rng`.
    pub fn new<R: CryptoRngCore + ?Sized>(rng: &mut R, bit_size: usize) -> Result<RsaPrivateKey> {
        let exp = BigUint::from_u64(Self::EXP).expect("invalid static exponent");
        Self::new_with_exp(rng, bit_size, &exp)
    }

    /// Generate a new RSA key pair of the given bit size and the public exponent
    /// using the passed in `rng`.
    ///
    /// Unless you have specific needs, you should use `RsaPrivateKey::new` instead.
    pub fn new_with_exp<R: CryptoRngCore + ?Sized>(
        rng: &mut R,
        bit_size: usize,
        exp: &BigUint,
    ) -> Result<RsaPrivateKey> {
        let components = generate_multi_prime_key_with_exp(rng, 2, bit_size, exp)?;
        RsaPrivateKey::from_components(components.n, components.e, components.d, components.primes)
    }

    /// Constructs an RSA key pair from individual components:
    ///
    /// - `n`: RSA modulus
    /// - `e`: public exponent (i.e. encrypting exponent)
    /// - `d`: private exponent (i.e. decrypting exponent)
    /// - `primes`: prime factors of `n`: typically two primes `p` and `q`. More than two primes can
    ///   be provided for multiprime RSA, however this is generally not recommended. If no `primes`
    ///   are provided, a prime factor recovery algorithm will be employed to attempt to recover the
    ///   factors (as described in [NIST SP 800-56B Revision 2] Appendix C.2). This algorithm only
    ///   works if there are just two prime factors `p` and `q` (as opposed to multiprime), and `e`
    ///   is between 2^16 and 2^256.
    ///
    ///  [NIST SP 800-56B Revision 2]: https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf
    pub fn from_components(
        n: BigUint,
        e: BigUint,
        d: BigUint,
        primes: Vec<BigUint>,
    ) -> Result<RsaPrivateKey> {
        let n = to_uint(n.clone());
        let e = e.to_u64().ok_or_else(|| Error::InvalidExponent)?;
        let nbits = n.bits_precision();
        let d = to_uint_exact(d, nbits);
        let primes = primes
            .into_iter()
            .map(|p| to_uint_exact(p, nbits))
            .collect();
        Self::from_components_new(n, e, d, primes)
    }

    pub(crate) fn from_components_new(
        n: BoxedUint,
        e: u64,
        d: BoxedUint,
        mut primes: Vec<BoxedUint>,
    ) -> Result<RsaPrivateKey> {
        let n_params = BoxedResidueParams::new(n.clone()).unwrap();
        let n_c = NonZero::new(n.clone()).unwrap();
        let nbits = n_c.bits_precision();

        let mut should_validate = false;

        if primes.len() < 2 {
            if !primes.is_empty() {
                return Err(Error::NprimesTooSmall);
            }
            // Recover `p` and `q` from `d`.
            // See method in Appendix C.2: https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf
            let (p, q) = recover_primes(
                &to_biguint(&n),
                &BigUint::from_u64(e).unwrap(),
                &to_biguint(&d),
            )?;
            primes.push(to_uint_exact(p, nbits));
            primes.push(to_uint_exact(q, nbits));
            should_validate = true;
        }

        let mut k = RsaPrivateKey {
            pubkey_components: RsaPublicKey {
                n: n_c,
                e,
                n_params,
            },
            d,
            primes,
            precomputed: None,
        };

        // Validate the key if we had to recover the primes.
        if should_validate {
            k.validate()?;
        }

        // precompute when possible, ignore error otherwise.
        let _ = k.precompute();

        Ok(k)
    }

    /// Constructs an RSA key pair from its two primes p and q.
    ///
    /// This will rebuild the private exponent and the modulus.
    ///
    /// Private exponent will be rebuilt using the method defined in
    /// [NIST 800-56B Section 6.2.1](https://nvlpubs.nist.gov/nistpubs/SpecialPublications/NIST.SP.800-56Br2.pdf#page=47).
    pub fn from_p_q(p: BigUint, q: BigUint, public_exponent: BigUint) -> Result<RsaPrivateKey> {
        if p == q {
            return Err(Error::InvalidPrime);
        }

        let n = compute_modulus(&[p.clone(), q.clone()]);
        let d = compute_private_exponent_carmicheal(&p, &q, &public_exponent)?;

        Self::from_components(n, public_exponent, d, vec![p, q])
    }

    /// Constructs an RSA key pair from its primes.
    ///
    /// This will rebuild the private exponent and the modulus.
    pub fn from_primes(primes: Vec<BigUint>, public_exponent: BigUint) -> Result<RsaPrivateKey> {
        if primes.len() < 2 {
            return Err(Error::NprimesTooSmall);
        }

        // Makes sure that primes is pairwise unequal.
        for (i, prime1) in primes.iter().enumerate() {
            for prime2 in primes.iter().take(i) {
                if prime1 == prime2 {
                    return Err(Error::InvalidPrime);
                }
            }
        }

        let n = compute_modulus(&primes);
        let d = compute_private_exponent_euler_totient(&primes, &public_exponent)?;

        Self::from_components(n, public_exponent, d, primes)
    }

    /// Get the public key from the private key, cloning `n` and `e`.
    ///
    /// Generally this is not needed since `RsaPrivateKey` implements the `PublicKey` trait,
    /// but it can occasionally be useful to discard the private information entirely.
    pub fn to_public_key(&self) -> RsaPublicKey {
        self.pubkey_components.clone()
    }

    /// Performs some calculations to speed up private key operations.
    pub fn precompute(&mut self) -> Result<()> {
        if self.precomputed.is_some() {
            return Ok(());
        }

        let d = &self.d;
        let p = &self.primes[0];
        let q = &self.primes[1];

        // TODO: error handling

        let p_params = BoxedResidueParams::new(p.clone()).unwrap();
        let q_params = BoxedResidueParams::new(q.clone()).unwrap();

        let x = NonZero::new(p.wrapping_sub(&BoxedUint::one())).unwrap();
        let dp = d.rem_vartime(&x);
        let x = NonZero::new(q.wrapping_sub(&BoxedUint::one())).unwrap();
        let dq = d.rem_vartime(&x);
        let qinv = BoxedResidue::new(q, p_params.clone());
        let qinv = qinv.invert();
        if qinv.is_none().into() {
            return Err(Error::InvalidPrime);
        }
        let qinv = qinv.unwrap();

        self.precomputed = Some(PrecomputedValues {
            dp,
            dq,
            qinv,
            p_params,
            q_params,
        });

        Ok(())
    }

    /// Clears precomputed values by setting to None
    pub fn clear_precomputed(&mut self) {
        self.precomputed = None;
    }

    /// Compute CRT coefficient: `(1/q) mod p`.
    pub fn crt_coefficient(&self) -> Option<BigUint> {
        let p = &self.primes[0];
        let q = &self.primes[1];

        Option::from(q.inv_mod(p)).map(|x| to_biguint(&x))
    }

    /// Performs basic sanity checks on the key.
    /// Returns `Ok(())` if everything is good, otherwise an appropriate error.
    pub fn validate(&self) -> Result<()> {
        check_public(self)?;

        // Check that Πprimes == n.
        let mut m = BoxedUint::one_with_precision(self.pubkey_components.n.bits_precision());
        for prime in &self.primes {
            // Any primes ≤ 1 will cause divide-by-zero panics later.
            if prime < &BoxedUint::one() {
                return Err(Error::InvalidPrime);
            }
            m = m.wrapping_mul(prime);
        }
        if m != *self.pubkey_components.n {
            return Err(Error::InvalidModulus);
        }

        // Check that de ≡ 1 mod p-1, for each prime.
        // This implies that e is coprime to each p-1 as e has a multiplicative
        // inverse. Therefore e is coprime to lcm(p-1,q-1,r-1,...) =
        // exponent(ℤ/nℤ). It also implies that a^de ≡ a mod p as a^(p-1) ≡ 1
        // mod p. Thus a^de ≡ a mod n for all a coprime to n, as required.
        let d = self.d.widen(2 * self.d.bits_precision());
        let de = d.wrapping_mul(&BoxedUint::from(self.pubkey_components.e));

        for prime in &self.primes {
            let prime = prime.widen(d.bits_precision());
            let x = NonZero::new(prime.wrapping_sub(&BoxedUint::one())).unwrap();
            let congruence = de.rem_vartime(&x);
            if !bool::from(congruence.is_one()) {
                return Err(Error::InvalidExponent);
            }
        }

        Ok(())
    }

    /// Decrypt the given message.
    pub fn decrypt<P: PaddingScheme>(&self, padding: P, ciphertext: &[u8]) -> Result<Vec<u8>> {
        padding.decrypt(Option::<&mut DummyRng>::None, self, ciphertext)
    }

    /// Decrypt the given message.
    ///
    /// Uses `rng` to blind the decryption process.
    pub fn decrypt_blinded<R: CryptoRngCore, P: PaddingScheme>(
        &self,
        rng: &mut R,
        padding: P,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>> {
        padding.decrypt(Some(rng), self, ciphertext)
    }

    /// Sign the given digest.
    pub fn sign<S: SignatureScheme>(&self, padding: S, digest_in: &[u8]) -> Result<Vec<u8>> {
        padding.sign(Option::<&mut DummyRng>::None, self, digest_in)
    }

    /// Sign the given digest using the provided `rng`, which is used in the
    /// following ways depending on the [`SignatureScheme`]:
    ///
    /// - [`Pkcs1v15Sign`][`crate::Pkcs1v15Sign`] padding: uses the RNG
    ///   to mask the private key operation with random blinding, which helps
    ///   mitigate sidechannel attacks.
    /// - [`Pss`][`crate::Pss`] always requires randomness. Use
    ///   [`Pss::new`][`crate::Pss::new`] for a standard RSASSA-PSS signature, or
    ///   [`Pss::new_blinded`][`crate::Pss::new_blinded`] for RSA-BSSA blind
    ///   signatures.
    pub fn sign_with_rng<R: CryptoRngCore, S: SignatureScheme>(
        &self,
        rng: &mut R,
        padding: S,
        digest_in: &[u8],
    ) -> Result<Vec<u8>> {
        padding.sign(Some(rng), self, digest_in)
    }
}

impl PrivateKeyPartsNew for RsaPrivateKey {
    fn d(&self) -> &BoxedUint {
        &self.d
    }

    fn primes(&self) -> &[BoxedUint] {
        &self.primes
    }

    fn dp(&self) -> Option<&BoxedUint> {
        self.precomputed.as_ref().map(|p| &p.dp)
    }

    fn dq(&self) -> Option<&BoxedUint> {
        self.precomputed.as_ref().map(|p| &p.dq)
    }

    fn qinv(&self) -> Option<&BoxedResidue> {
        self.precomputed.as_ref().map(|p| &p.qinv)
    }

    fn crt_values(&self) -> Option<&[CrtValueNew]> {
        None
    }

    fn p_params(&self) -> Option<&BoxedResidueParams> {
        self.precomputed.as_ref().map(|p| &p.p_params)
    }

    fn q_params(&self) -> Option<&BoxedResidueParams> {
        self.precomputed.as_ref().map(|p| &p.q_params)
    }
}

/// Check that the public key is well formed and has an exponent within acceptable bounds.
#[inline]
pub fn check_public(public_key: &impl PublicKeyParts) -> Result<()> {
    check_public_with_max_size(&public_key.n(), &public_key.e(), RsaPublicKey::MAX_SIZE)
}

/// Check that the public key is well formed and has an exponent within acceptable bounds.
#[inline]
fn check_public_with_max_size(n: &BigUint, e: &BigUint, max_size: usize) -> Result<()> {
    if n.bits() > max_size {
        return Err(Error::ModulusTooLarge);
    }

    let eu64 = e.to_u64().ok_or(Error::PublicExponentTooLarge)?;

    if e >= n || n.is_even() {
        return Err(Error::InvalidModulus);
    }

    if e.is_even() {
        return Err(Error::InvalidExponent);
    }

    if eu64 < RsaPublicKey::MIN_PUB_EXPONENT {
        return Err(Error::PublicExponentTooSmall);
    }

    if eu64 > RsaPublicKey::MAX_PUB_EXPONENT {
        return Err(Error::PublicExponentTooLarge);
    }

    Ok(())
}

pub(crate) fn to_biguint(uint: &BoxedUint) -> BigUint {
    BigUint::from_bytes_be(&uint.to_be_bytes())
}

pub(crate) fn to_uint_exact(big_uint: BigUint, nbits: u32) -> BoxedUint {
    let res = inner_to_uint(big_uint);

    match res.bits_precision().cmp(&nbits) {
        Ordering::Equal => res,
        Ordering::Greater => panic!("too large: {} > {}", res.bits_precision(), nbits),
        Ordering::Less => res.widen(nbits),
    }
}

fn inner_to_uint(big_uint: BigUint) -> BoxedUint {
    let bytes = big_uint.to_bytes_be();
    let rem = bytes.len() % Limb::BYTES;
    let pad_count = if rem == 0 { 0 } else { Limb::BYTES - rem };

    let mut padded_bytes = vec![0u8; pad_count];
    padded_bytes.extend_from_slice(&bytes);

    BoxedUint::from_be_slice(&padded_bytes, padded_bytes.len() as u32 * 8).unwrap()
}

pub(crate) fn to_uint(big_uint: BigUint) -> BoxedUint {
    let nbits = needed_bits(&big_uint);
    let res = inner_to_uint(big_uint);
    if (res.bits_precision() as u32) < nbits {
        return res.widen(nbits);
    }
    res
}

pub(crate) fn reduce(n: &BoxedUint, p: BoxedResidueParams) -> BoxedResidue {
    let bits_precision = p.modulus().bits_precision();
    let modulus = NonZero::new(p.modulus().clone()).unwrap();

    let n = match n.bits_precision().cmp(&bits_precision) {
        Ordering::Less => n.widen(bits_precision),
        Ordering::Equal => n.clone(),
        Ordering::Greater => n.shorten(bits_precision),
    };

    let n_reduced = n.rem_vartime(&modulus).widen(p.bits_precision());
    BoxedResidue::new(&n_reduced, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::rsa::{rsa_decrypt_and_check, rsa_encrypt};
    use crate::traits::{PrivateKeyParts, PublicKeyParts};

    use hex_literal::hex;
    use num_traits::{FromPrimitive, ToPrimitive};
    use pkcs8::DecodePrivateKey;
    use rand_chacha::{rand_core::SeedableRng, ChaCha8Rng};

    #[test]
    fn test_from_into() {
        let raw_n = BoxedUint::from(101u64);
        let private_key = RsaPrivateKey {
            pubkey_components: RsaPublicKey {
                n: NonZero::new(raw_n.clone()).unwrap(),
                e: 200u64,
                n_params: BoxedResidueParams::new(raw_n).unwrap(),
            },
            d: BoxedUint::from(123u64),
            primes: vec![],
            precomputed: None,
        };
        let public_key: RsaPublicKey = private_key.into();

        assert_eq!(PublicKeyParts::n(&public_key).to_u64(), Some(101));
        assert_eq!(PublicKeyParts::e(&public_key).to_u64(), Some(200));
    }

    fn test_key_basics(private_key: &RsaPrivateKey) {
        private_key.validate().expect("invalid private key");

        assert!(
            PrivateKeyParts::d(private_key) < PublicKeyParts::n(private_key),
            "private exponent too large"
        );

        let pub_key: RsaPublicKey = private_key.clone().into();
        let m = BoxedUint::from(42u64);
        let c = rsa_encrypt(&pub_key, &m).expect("encryption successfull");

        let m2 = rsa_decrypt_and_check::<ChaCha8Rng>(private_key, None, &c)
            .expect("unable to decrypt without blinding");
        assert_eq!(m, m2);
        let mut rng = ChaCha8Rng::from_seed([42; 32]);
        let m3 = rsa_decrypt_and_check(private_key, Some(&mut rng), &c)
            .expect("unable to decrypt with blinding");
        assert_eq!(m, m3);
    }

    macro_rules! key_generation {
        ($name:ident, $multi:expr, $size:expr) => {
            #[test]
            fn $name() {
                let mut rng = ChaCha8Rng::from_seed([42; 32]);
                let exp = BigUint::from_u64(RsaPrivateKey::EXP).expect("invalid static exponent");

                for _ in 0..10 {
                    let components =
                        generate_multi_prime_key_with_exp(&mut rng, $multi, $size, &exp).unwrap();
                    let private_key = RsaPrivateKey::from_components(
                        components.n,
                        components.e,
                        components.d,
                        components.primes,
                    )
                    .unwrap();
                    assert_eq!(PublicKeyParts::n(&private_key).bits(), $size);

                    test_key_basics(&private_key);
                }
            }
        };
    }

    key_generation!(key_generation_128, 2, 128);
    key_generation!(key_generation_1024, 2, 1024);

    key_generation!(key_generation_multi_3_256, 3, 256);

    key_generation!(key_generation_multi_4_64, 4, 64);

    key_generation!(key_generation_multi_5_64, 5, 64);
    key_generation!(key_generation_multi_8_576, 8, 576);
    key_generation!(key_generation_multi_16_1024, 16, 1024);

    #[test]
    fn test_negative_decryption_value() {
        let private_key = RsaPrivateKey::from_components(
            BigUint::from_bytes_le(&[
                99, 192, 208, 179, 0, 220, 7, 29, 49, 151, 75, 107, 75, 73, 200, 180,
            ]),
            BigUint::from_bytes_le(&[1, 0, 1]),
            BigUint::from_bytes_le(&[
                81, 163, 254, 144, 171, 159, 144, 42, 244, 133, 51, 249, 28, 12, 63, 65,
            ]),
            vec![
                BigUint::from_bytes_le(&[105, 101, 60, 173, 19, 153, 3, 192]),
                BigUint::from_bytes_le(&[235, 65, 160, 134, 32, 136, 6, 241]),
            ],
        )
        .unwrap();

        for _ in 0..1000 {
            test_key_basics(&private_key);
        }
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde() {
        use rand_chacha::{rand_core::SeedableRng, ChaCha8Rng};
        use serde_test::{assert_tokens, Token};

        let mut rng = ChaCha8Rng::from_seed([42; 32]);
        let priv_key = RsaPrivateKey::new(&mut rng, 64).expect("failed to generate key");

        let priv_tokens = [
            Token::Struct {
                name: "RsaPrivateKey",
                len: 3,
            },
            Token::Str("pubkey_components"),
            Token::Struct {
                name: "RsaPublicKey",
                len: 2,
            },
            Token::Str("n"),
            Token::Seq { len: Some(2) },
            Token::U32(3814409919),
            Token::U32(3429654832),
            Token::SeqEnd,
            Token::Str("e"),
            Token::Seq { len: Some(1) },
            Token::U32(65537),
            Token::SeqEnd,
            Token::StructEnd,
            Token::Str("d"),
            Token::Seq { len: Some(2) },
            Token::U32(1482162201),
            Token::U32(1675500232),
            Token::SeqEnd,
            Token::Str("primes"),
            Token::Seq { len: Some(2) },
            Token::Seq { len: Some(1) },
            Token::U32(4133289821),
            Token::SeqEnd,
            Token::Seq { len: Some(1) },
            Token::U32(3563808971),
            Token::SeqEnd,
            Token::SeqEnd,
            Token::StructEnd,
        ];
        assert_tokens(&priv_key, &priv_tokens);

        let priv_tokens = [
            Token::Struct {
                name: "RsaPublicKey",
                len: 2,
            },
            Token::Str("n"),
            Token::Seq { len: Some(2) },
            Token::U32(3814409919),
            Token::U32(3429654832),
            Token::SeqEnd,
            Token::Str("e"),
            Token::Seq { len: Some(1) },
            Token::U32(65537),
            Token::SeqEnd,
            Token::StructEnd,
        ];
        assert_tokens(&RsaPublicKey::from(priv_key), &priv_tokens);
    }

    #[test]
    fn invalid_coeff_private_key_regression() {
        use base64ct::{Base64, Encoding};

        let n = Base64::decode_vec("wC8GyQvTCZOK+iiBR5fGQCmzRCTWX9TQ3aRG5gGFk0wB6EFoLMAyEEqeG3gS8xhAm2rSWYx9kKufvNat3iWlbSRVqkcbpVAYlj2vTrpqDpJl+6u+zxFYoUEBevlJJkAhl8EuCccOA30fVpcfRvXPTtvRd3yFT9E9EwZljtgSI02w7gZwg7VIxaGeajh5Euz6ZVQZ+qNRKgXrRC7gPRqVyI6Dt0Jc+Su5KBGNn0QcPDzOahWha1ieaeMkFisZ9mdpsJoZ4tw5eicLaUomKzALHXQVt+/rcZSrCd6/7uUo11B/CYBM4UfSpwXaL88J9AE6A5++no9hmJzaF2LLp+Qwx4yY3j9TDutxSAjsraxxJOGZ3XyA9nG++Ybt3cxZ5fP7ROjxCfROBmVv5dYn0O9OBIqYeCH6QraNpZMadlLNIhyMv8Y+P3r5l/PaK4VJaEi5pPosnEPawp0W0yZDzmjk2z1LthaRx0aZVrAjlH0Rb/6goLUQ9qu1xsDtQVVpN4A89ZUmtTWORnnJr0+595eHHxssd2gpzqf4bPjNITdAEuOCCtpvyi4ls23zwuzryUYjcUOEnsXNQ+DrZpLKxdtsD/qNV/j1hfeyBoPllC3cV+6bcGOFcVGbjYqb+Kw1b0+jL69RSKQqgmS+qYqr8c48nDRxyq3QXhR8qtzUwBFSLVk=").unwrap();
        let e = Base64::decode_vec("AQAB").unwrap();
        let d = Base64::decode_vec("qQazSQ+FRN7nVK1bRsROMRB8AmsDwLVEHivlz1V3Td2Dr+oW3YUMgxedhztML1IdQJPq/ad6qErJ6yRFNySVIjDaxzBTOEoB1eHa1btOnBJWb8rVvvjaorixvJ6Tn3i4EuhsvVy9DoR1k4rGj3qSIiFjUVvLRDAbLyhpGgEfsr0Z577yJmTC5E8JLRMOKX8Tmxsk3jPVpsgd65Hu1s8S/ZmabwuHCf9SkdMeY/1bd/9i7BqqJeeDLE4B5x1xcC3z3scqDUTzqGO+vZPhjgprPDRlBamVwgenhr7KwCn8iaLamFinRVwOAag8BeBqOJj7lURiOsKQa9FIX1kdFUS1QMQxgtPycLjkbvCJjriqT7zWKsmJ7l8YLs6Wmm9/+QJRwNCEVdMTXKfCP1cJjudaiskEQThfUldtgu8gUDNYbQ/Filb2eKfiX4h1TiMxZqUZHVZyb9nShbQoXJ3vj/MGVF0QM8TxhXM8r2Lv9gDYU5t9nQlUMLhs0jVjai48jHABbFNyH3sEcOmJOIwJrCXw1dzG7AotwyaEVUHOmL04TffmwCFfnyrLjbFgnyOeoyIIBYjcY7QFRm/9nupXMTH5hZ2qrHfCJIp0KK4tNBdQqmnHapFl5l6Le1s4qBS5bEIzjitobLvAFm9abPlDGfxmY6mlrMK4+nytwF9Ct7wc1AE=").unwrap();
        let primes = vec![
            Base64::decode_vec("9kQWEAzsbzOcdPa+s5wFfw4XDd7bB1q9foZ31b1+TNjGNxbSBCFlDF1q98vwpV6nM8bWDh/wtbNoETSQDgpEnYOQ26LWEw6YY1+q1Q2GGEFceYUf+Myk8/vTc8TN6Zw0bKZBWy10Qo8h7xk4JpzuI7NcxvjJYTkS9aErFxi3vVH0aiZC0tmfaCqr8a2rJxyVwqreRpOjwAWrotMsf2wGsF4ofx5ScoFy5GB5fJkkdOrW1LyTvZAUCX3cstPr19+TNC5zZOk7WzZatnCkN5H5WzalWtZuu0oVL205KPOa3R8V2yv5e6fm0v5fTmqSuvjmaMJLXCN4QJkmIzojO99ckQ==").unwrap(),
            Base64::decode_vec("x8exdMjVA2CiI+Thx7loHtVcevoeE2sZ7btRVAvmBqo+lkHwxb7FHRnWvuj6eJSlD2f0T50EewIhhiW3R9BmktCk7hXjbSCnC1u9Oxc1IAUm/7azRqyfCMx43XhLxpD+xkBCpWkKDLxGczsRwTuaP3lKS3bSdBrNlGmdblubvVBIq4YZ2vXVlnYtza0cS+dgCK7BGTqUsrCUd/ZbIvwcwZkZtpkhj1KQfto9X/0OMurBzAqbkeq1cyRHXHkOfN/qbUIIRqr9Ii7Eswf9Vk8xp2O1Nt8nzcYS9PFD12M5eyaeFEkEYfpNMNGuTzp/31oqVjbpoCxS6vuWAZyADxhISQ==").unwrap(),
            Base64::decode_vec("is7d0LY4HoXszlC2NO7gejkq7XqL4p1W6hZJPYTNx+r37t1CC2n3Vvzg6kNdpRixDhIpXVTLjN9O7UO/XuqSumYKJIKoP52eb4Tg+a3hw5Iz2Zsb5lUTNSLgkQSBPAf71LHxbL82JL4g1nBUog8ae60BwnVArThKY4EwlJguGNw09BAU4lwf6csDl/nX2vfVwiAloYpeZkHL+L8m+bueGZM5KE2jEz+7ztZCI+T+E5i69rZEYDjx0lfLKlEhQlCW3HbCPELqXgNJJkRfi6MP9kXa9lSfnZmoT081RMvqonB/FUa4HOcKyCrw9XZEtnbNCIdbitfDVEX+pSSD7596wQ==").unwrap(),
            Base64::decode_vec("GPs0injugfycacaeIP5jMa/WX55VEnKLDHom4k6WlfDF4L4gIGoJdekcPEUfxOI5faKvHyFwRP1wObkPoRBDM0qZxRfBl4zEtpvjHrd5MibSyJkM8+J0BIKk/nSjbRIGeb3hV5O56PvGB3S0dKhCUnuVObiC+ne7izplsD4OTG70l1Yud33UFntyoMxrxGYLUSqhBMmZfHquJg4NOWOzKNY/K+EcHDLj1Kjvkcgv9Vf7ocsVxvpFdD9uGPceQ6kwRDdEl6mb+6FDgWuXVyqR9+904oanEIkbJ7vfkthagLbEf57dyG6nJlqh5FBZWxGIR72YGypPuAh7qnnqXXjY2Q==").unwrap(),
            Base64::decode_vec("CUWC+hRWOT421kwRllgVjy6FYv6jQUcgDNHeAiYZnf5HjS9iK2ki7v8G5dL/0f+Yf+NhE/4q8w4m8go51hACrVpP1p8GJDjiT09+RsOzITsHwl+ceEKoe56ZW6iDHBLlrNw5/MtcYhKpjNU9KJ2udm5J/c9iislcjgckrZG2IB8ADgXHMEByZ5DgaMl4AKZ1Gx8/q6KftTvmOT5rNTMLi76VN5KWQcDWK/DqXiOiZHM7Nr4dX4me3XeRgABJyNR8Fqxj3N1+HrYLe/zs7LOaK0++F9Ul3tLelhrhsvLxei3oCZkF9A/foD3on3luYA+1cRcxWpSY3h2J4/22+yo4+Q==").unwrap(),
        ];

        RsaPrivateKey::from_components(
            BigUint::from_bytes_be(&n),
            BigUint::from_bytes_be(&e),
            BigUint::from_bytes_be(&d),
            primes.iter().map(|p| BigUint::from_bytes_be(p)).collect(),
        )
        .unwrap();
    }

    #[test]
    fn reject_oversized_private_key() {
        // -----BEGIN PUBLIC KEY-----
        // MIIEIjANBgkqhkiG9w0BAQEFAAOCBA8AMIIECgKCBAEAkMBiB8qsNVXAsJR6Xoto
        // H1r2rtZl/xzUK2tIfy99aPE489u+5tLxCQhQf+a89158vSDpr2/xwgK8w9u0Xpu2
        // m7XRKjVMS0Y6UIINFoeTc87rVXT92Scr47kNVcGmSFXez4BSDpS+LKpWwXN+0AQu
        // +cmcfdtsx2862iEbqQvq4PwKGQJOdOR0yldH8O4yeJK/buvIOXRHjb++vtQND/xi
        // bFGAcd9WJqvaOG7tclhbZ277mbO6ER+y9Lj7AyO8ywybWqNeHaVPHMysPhT7HUWI
        // 17m59i1OpuVwwEnvzDQQEUf9d5hUmkLYb5qQzuf6Ddnx/04QJCKAgkhyr9CXgnV6
        // vEZ3PKtpicCHRxk7eqTEmgBlgwqH5vflRFV1iywQMXJnuRhzWOQaXl/vb8v4HIvF
        // 4TatEZKqfzpbyScLIiYbPEAhHXKdZMd2zY8hkSbicifePApAZmuNpAxxJDZzphh7
        // r4lD6t8MPT/RUAdtrZfihqaBhduFI6YeVIy6emg05M6YWvlUyer7nYGaPRS1JqD4
        // 0v7xOtme5I8Qw6APiFPXhTqBK3occr7TgGb3V3lpC8Eq+esNHrji98R1fITkFXJW
        // KdFcTWjBghPxiobUzMCFUrPIDJcWXeBzrARAryU+hXjEiFfzluXrps0B7RJQ/rLD
        // LXeTn4vovUeHQVHa7YfoyWMy9pfqeVC+56LBK7SEIAvL0I3lrq5vIv+ZIuOAdbVg
        // JiRy8DneCOk2LP3RnA8M0HSevYW93DiC+4h/l4ntjjiOfi6yRVOZ8WbVyXZ/83j4
        // 6+pGWgvi0uMyb+btgOXjBQv7bGqdyHMc5Lqk5bF7ExETx51vKQMYCV4351caS6aX
        // q16lYZATHgbTADEAZHdroDMJB+HMQaze9O6qU5ZO8wxxAjw89xry0dnoOQD/yA4H
        // 7CRCo9vVDpV2hqIvHY9RI2T7cek28kmQpKvNvvK+ovmM138dHKViWULHk0fBRt7m
        // 4wQ+tiL2PmJ/Tr8g1gVhM6S9D1XdE9z0KeDnODCWn1Q8sx2G2ah4ynnYQURDWcwO
        // McAoP6bdJ7cCt+4F2tEsMPf4S/EwlnjvuNoQjvztxCPahYe9EnyggtQXyHJveIn7
        // gDJsP6b93VB6x4QbLy5ch4DUhqDWginuKVeo7CTgDkq03j/IEaS1BHwreSDQceny
        // +bYWONwV+4TMpGytKOHvU5288kmHbyZHdXuaXk8LLqbnqr30fa6Cbp4llCi9sH5a
        // Kmi5jxQfVTe+elkMs7oVsLsVgkZS6NqPcOuEckAFijNqG223+IJoqvifCzO5Bdcs
        // JTOLE+YaUYc8LUJwIaPykgcXmtMvQjeT8MCQ3aAlzkHfDpSvvICrXtqbGiaKolU6
        // mQIDAQAB
        // -----END PUBLIC KEY-----

        let n = BigUint::from_bytes_be(&hex!(
            "
            90c06207caac3555c0b0947a5e8b681f5af6aed665ff1cd42b6b487f2f7d68f1
            38f3dbbee6d2f10908507fe6bcf75e7cbd20e9af6ff1c202bcc3dbb45e9bb69b
            b5d12a354c4b463a50820d16879373ceeb5574fdd9272be3b90d55c1a64855de
            cf80520e94be2caa56c1737ed0042ef9c99c7ddb6cc76f3ada211ba90beae0fc
            0a19024e74e474ca5747f0ee327892bf6eebc83974478dbfbebed40d0ffc626c
            518071df5626abda386eed72585b676efb99b3ba111fb2f4b8fb0323bccb0c9b
            5aa35e1da54f1cccac3e14fb1d4588d7b9b9f62d4ea6e570c049efcc34101147
            fd7798549a42d86f9a90cee7fa0dd9f1ff4e10242280824872afd09782757abc
            46773cab6989c08747193b7aa4c49a0065830a87e6f7e54455758b2c10317267
            b9187358e41a5e5fef6fcbf81c8bc5e136ad1192aa7f3a5bc9270b22261b3c40
            211d729d64c776cd8f219126e27227de3c0a40666b8da40c71243673a6187baf
            8943eadf0c3d3fd150076dad97e286a68185db8523a61e548cba7a6834e4ce98
            5af954c9eafb9d819a3d14b526a0f8d2fef13ad99ee48f10c3a00f8853d7853a
            812b7a1c72bed38066f75779690bc12af9eb0d1eb8e2f7c4757c84e415725629
            d15c4d68c18213f18a86d4ccc08552b3c80c97165de073ac0440af253e8578c4
            8857f396e5eba6cd01ed1250feb2c32d77939f8be8bd47874151daed87e8c963
            32f697ea7950bee7a2c12bb484200bcbd08de5aeae6f22ff9922e38075b56026
            2472f039de08e9362cfdd19c0f0cd0749ebd85bddc3882fb887f9789ed8e388e
            7e2eb2455399f166d5c9767ff378f8ebea465a0be2d2e3326fe6ed80e5e3050b
            fb6c6a9dc8731ce4baa4e5b17b131113c79d6f290318095e37e7571a4ba697ab
            5ea56190131e06d300310064776ba0330907e1cc41acdef4eeaa53964ef30c71
            023c3cf71af2d1d9e83900ffc80e07ec2442a3dbd50e957686a22f1d8f512364
            fb71e936f24990a4abcdbef2bea2f98cd77f1d1ca5625942c79347c146dee6e3
            043eb622f63e627f4ebf20d6056133a4bd0f55dd13dcf429e0e73830969f543c
            b31d86d9a878ca79d841444359cc0e31c0283fa6dd27b702b7ee05dad12c30f7
            f84bf1309678efb8da108efcedc423da8587bd127ca082d417c8726f7889fb80
            326c3fa6fddd507ac7841b2f2e5c8780d486a0d68229ee2957a8ec24e00e4ab4
            de3fc811a4b5047c2b7920d071e9f2f9b61638dc15fb84cca46cad28e1ef539d
            bcf249876f2647757b9a5e4f0b2ea6e7aabdf47dae826e9e259428bdb07e5a2a
            68b98f141f5537be7a590cb3ba15b0bb15824652e8da8f70eb847240058a336a
            1b6db7f88268aaf89f0b33b905d72c25338b13e61a51873c2d427021a3f29207
            179ad32f423793f0c090dda025ce41df0e94afbc80ab5eda9b1a268aa2553a99"
        ));

        let e = BigUint::from_u64(65537).unwrap();

        assert_eq!(
            RsaPublicKey::new(n, e).err().unwrap(),
            Error::ModulusTooLarge
        );
    }

    #[test]
    fn build_key_from_primes() {
        const RSA_2048_PRIV_DER: &[u8] = include_bytes!("../tests/examples/pkcs8/rsa2048-priv.der");
        let ref_key = RsaPrivateKey::from_pkcs8_der(RSA_2048_PRIV_DER).unwrap();
        assert_eq!(ref_key.validate(), Ok(()));

        let primes = PrivateKeyParts::primes(&ref_key).to_vec();

        let exp = PublicKeyParts::e(&ref_key);
        let key =
            RsaPrivateKey::from_primes(primes, exp).expect("failed to import key from primes");
        assert_eq!(key.validate(), Ok(()));

        assert_eq!(PublicKeyParts::n(&key), PublicKeyParts::n(&ref_key));

        assert_eq!(PrivateKeyParts::dp(&key), PrivateKeyParts::dp(&ref_key));
        assert_eq!(PrivateKeyParts::dq(&key), PrivateKeyParts::dq(&ref_key));

        assert_eq!(PrivateKeyParts::d(&key), PrivateKeyParts::d(&ref_key));
    }

    #[test]
    fn build_key_from_p_q() {
        const RSA_2048_SP800_PRIV_DER: &[u8] =
            include_bytes!("../tests/examples/pkcs8/rsa2048-sp800-56b-priv.der");
        let ref_key = RsaPrivateKey::from_pkcs8_der(RSA_2048_SP800_PRIV_DER).unwrap();
        assert_eq!(ref_key.validate(), Ok(()));

        let primes = PrivateKeyParts::primes(&ref_key).to_vec();
        let exp = PublicKeyParts::e(&ref_key);

        let key = RsaPrivateKey::from_p_q(primes[0].clone(), primes[1].clone(), exp)
            .expect("failed to import key from primes");
        assert_eq!(key.validate(), Ok(()));

        assert_eq!(PublicKeyParts::n(&key), PublicKeyParts::n(&ref_key));

        assert_eq!(PrivateKeyParts::dp(&key), PrivateKeyParts::dp(&ref_key));
        assert_eq!(PrivateKeyParts::dq(&key), PrivateKeyParts::dq(&ref_key));

        assert_eq!(PrivateKeyParts::d(&key), PrivateKeyParts::d(&ref_key));
    }
}
