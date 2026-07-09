//! MDX descriptor decryption — a TrueCrypt-derived scheme.
//!
//! Ported from cdemu `libmirage` `image-mdx/crypto.c` (itself based on the
//! `mdsx` reverse-engineering project). AES-256 is used purely as an ECB block
//! primitive; the CBC-with-de-whitening and key-derivation logic is implemented
//! here. Only the **descriptor**-decryption path is needed (track data in the
//! supported images is unencrypted), so the AES-XTS/LRW data path is omitted.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use aes::Aes256;

use super::{ENC_HEADER_SIZE, IV_SIZE, SALT_SIZE};
use crate::error::{OpticaldiscsError, Result};

/// Standard CRC-32 (poly `0xEDB88320`, reflected) — verifies the key data.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((crc & 1) * 0xEDB8_8320);
        }
    }
    !crc
}

/// Reflected CRC-32 with the CD-EDC polynomial `0xD8018001`, init 0, no invert.
fn crc32_edc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc = (crc >> 8) ^ EDC_LUT[((crc & 0xFF) as u8 ^ b) as usize];
    }
    crc
}

/// Lazily-free slice-by-1 table for [`crc32_edc`].
static EDC_LUT: [u32; 256] = build_edc_lut();

const fn build_edc_lut() -> [u32; 256] {
    let mut lut = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = (crc >> 1) ^ ((crc & 1) * 0xD801_8001);
            j += 1;
        }
        lut[i] = crc;
        i += 1;
    }
    lut
}

/// Derive the descriptor password from the encryption header salt (the `mdsx`
/// "unshuffle" routine).
fn derive_password(salt: &[u8]) -> [u8; SALT_SIZE] {
    let mut words = [0u32; SALT_SIZE / 4];
    for (i, w) in words.iter_mut().enumerate() {
        *w = u32::from_le_bytes(salt[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let mut modifier = crc32_edc(salt) ^ 0x5673_72ff;
    for w in words.iter_mut() {
        modifier = modifier.wrapping_mul(0x35e8_5a6d).wrapping_add(0x1548_dce9);
        let mut v = *w ^ modifier ^ 0xec56_4717;
        // Replace any zero octet with 0x5f.
        if v & 0x0000_00ff == 0 {
            v |= 0x0000_005f;
        }
        if v & 0x0000_ff00 == 0 {
            v |= 0x0000_5f00;
        }
        if v & 0x00ff_0000 == 0 {
            v |= 0x005f_0000;
        }
        if v & 0xff00_0000 == 0 {
            v |= 0x5f00_0000;
        }
        *w = v;
    }
    let mut out = [0u8; SALT_SIZE];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// AES-256-ECB decrypt one 16-byte block in place.
fn aes_decrypt_block(cipher: &Aes256, block: &mut [u8]) {
    let mut b = GenericArray::clone_from_slice(block);
    cipher.decrypt_block(&mut b);
    block.copy_from_slice(&b);
}

/// TrueCrypt legacy CBC with de-whitening (upper 8 bytes of the IV), in place.
/// `data.len()` must be a multiple of 16.
fn cbc_dewhiten(cipher: &Aes256, data: &mut [u8], iv: &[u8; 16]) {
    let iv1 = u64::from_le_bytes(iv[8..16].try_into().unwrap());
    let mut chain = [u64::from_le_bytes(iv[0..8].try_into().unwrap()), iv1];
    for blk in data.chunks_exact_mut(16) {
        // De-whitening with the upper half of the IV, before deciphering.
        let mut d0 = u64::from_le_bytes(blk[0..8].try_into().unwrap()) ^ iv1;
        let mut d1 = u64::from_le_bytes(blk[8..16].try_into().unwrap()) ^ iv1;
        let ciphertext = [d0, d1];
        blk[0..8].copy_from_slice(&d0.to_le_bytes());
        blk[8..16].copy_from_slice(&d1.to_le_bytes());

        aes_decrypt_block(cipher, blk);

        d0 = u64::from_le_bytes(blk[0..8].try_into().unwrap()) ^ chain[0];
        d1 = u64::from_le_bytes(blk[8..16].try_into().unwrap()) ^ chain[1];
        blk[0..8].copy_from_slice(&d0.to_le_bytes());
        blk[8..16].copy_from_slice(&d1.to_le_bytes());
        chain = ciphertext;
    }
}

/// Build an AES-256 cipher from a master-key buffer whose first [`IV_SIZE`]
/// bytes are the IV seed and whose following 32 bytes are the AES key.
fn cipher_from(key_source: &[u8]) -> Aes256 {
    Aes256::new(GenericArray::from_slice(&key_source[IV_SIZE..IV_SIZE + 32]))
}

/// Decipher the 512-byte encryption header, returning the deciphered buffer
/// (its leading [`SALT_SIZE`] salt bytes are unchanged).
pub fn decipher_encryption_header(header: &[u8]) -> Result<Vec<u8>> {
    if header.len() != ENC_HEADER_SIZE {
        return Err(OpticaldiscsError::Parse(
            "MDX encryption header wrong size".into(),
        ));
    }
    let mut buf = header.to_vec();
    let salt: [u8; SALT_SIZE] = buf[0..SALT_SIZE].try_into().unwrap();
    let password = derive_password(&salt);

    // PBKDF2-HMAC-RIPEMD160, 2000 iterations → 120 + IV_SIZE bytes of key material.
    let mut master = [0u8; 120 + IV_SIZE];
    pbkdf2::pbkdf2_hmac::<ripemd::Ripemd160>(&password, &salt, 2000, &mut master);

    let cipher = cipher_from(&master);
    let iv: [u8; 16] = master[0..16].try_into().unwrap();
    // Encrypted region runs from key_data_checksum (at SALT_SIZE) to the end.
    cbc_dewhiten(&cipher, &mut buf[SALT_SIZE..], &iv);
    Ok(buf)
}

/// Decipher then zlib-inflate the descriptor.
///
/// `key_data` is the deciphered header's key-data field; `compressed_size` is
/// the exact compressed length (the input may be zero-padded to a 16-byte
/// multiple, which zlib ignores).
pub fn decipher_descriptor(
    desc_raw: &[u8],
    key_data: &[u8],
    compressed_size: usize,
) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut buf = desc_raw.to_vec();
    let aligned = buf.len() & !15;
    let cipher = cipher_from(key_data);
    let iv: [u8; 16] = key_data[0..16].try_into().unwrap();
    // De-whitening is applied per 512-byte TrueCrypt block.
    let mut off = 0;
    while off < aligned {
        let bs = (aligned - off).min(512);
        cbc_dewhiten(&cipher, &mut buf[off..off + bs], &iv);
        off += bs;
    }

    let end = compressed_size.min(buf.len());
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&buf[..end])
        .read_to_end(&mut out)
        .map_err(|e| OpticaldiscsError::Parse(format!("MDX descriptor inflate failed: {e}")))?;
    Ok(out)
}

// ── Forward crypto (test-only) — build synthetic encrypted MDX fixtures ───────

/// AES-256-ECB encrypt one 16-byte block in place.
#[cfg(test)]
fn aes_encrypt_block(cipher: &Aes256, block: &mut [u8]) {
    use aes::cipher::BlockEncrypt;
    let mut b = GenericArray::clone_from_slice(block);
    cipher.encrypt_block(&mut b);
    block.copy_from_slice(&b);
}

/// Inverse of [`cbc_dewhiten`]: encrypt plaintext into stored ciphertext.
#[cfg(test)]
fn cbc_dewhiten_encrypt(cipher: &Aes256, data: &mut [u8], iv: &[u8; 16]) {
    let iv1 = u64::from_le_bytes(iv[8..16].try_into().unwrap());
    let mut chain = [u64::from_le_bytes(iv[0..8].try_into().unwrap()), iv1];
    for blk in data.chunks_exact_mut(16) {
        let u0 = u64::from_le_bytes(blk[0..8].try_into().unwrap()) ^ chain[0];
        let u1 = u64::from_le_bytes(blk[8..16].try_into().unwrap()) ^ chain[1];
        blk[0..8].copy_from_slice(&u0.to_le_bytes());
        blk[8..16].copy_from_slice(&u1.to_le_bytes());
        aes_encrypt_block(cipher, blk);
        let t0 = u64::from_le_bytes(blk[0..8].try_into().unwrap());
        let t1 = u64::from_le_bytes(blk[8..16].try_into().unwrap());
        blk[0..8].copy_from_slice(&(t0 ^ iv1).to_le_bytes());
        blk[8..16].copy_from_slice(&(t1 ^ iv1).to_le_bytes());
        chain = [t0, t1];
    }
}

/// Build a 512-byte encryption header the reader will accept: plaintext fields
/// filled in, then the encrypted region enciphered from the salt-derived key.
#[cfg(test)]
pub(super) fn build_test_encryption_header(
    salt: &[u8; SALT_SIZE],
    key_data: &[u8; super::KEYDATA_SIZE],
    compressed_size: u32,
    decompressed_size: u32,
) -> Vec<u8> {
    const KEYDATA_SIZE: usize = super::KEYDATA_SIZE;
    let mut hdr = vec![0u8; ENC_HEADER_SIZE];
    hdr[0..SALT_SIZE].copy_from_slice(salt);
    let crc = crc32(key_data);
    hdr[SALT_SIZE..SALT_SIZE + 4].copy_from_slice(&crc.to_le_bytes());
    hdr[SALT_SIZE + 4..SALT_SIZE + 8].copy_from_slice(&super::MAGIC.to_le_bytes());
    hdr[SALT_SIZE + 8..SALT_SIZE + 10].copy_from_slice(&1u16.to_le_bytes()); // unknown1
    hdr[SALT_SIZE + 10..SALT_SIZE + 12].copy_from_slice(&0x100u16.to_le_bytes()); // key_size
    hdr[SALT_SIZE + 16..SALT_SIZE + 16 + KEYDATA_SIZE].copy_from_slice(key_data);
    let tail = SALT_SIZE + 16 + KEYDATA_SIZE;
    hdr[tail..tail + 4].copy_from_slice(&compressed_size.to_le_bytes());
    hdr[tail + 4..tail + 8].copy_from_slice(&decompressed_size.to_le_bytes());

    let password = derive_password(salt);
    let mut master = [0u8; 120 + IV_SIZE];
    pbkdf2::pbkdf2_hmac::<ripemd::Ripemd160>(&password, salt, 2000, &mut master);
    let cipher = cipher_from(&master);
    let iv: [u8; 16] = master[0..16].try_into().unwrap();
    cbc_dewhiten_encrypt(&cipher, &mut hdr[SALT_SIZE..], &iv);
    hdr
}

/// Encrypt a compressed descriptor payload (padded to a 16-byte multiple) with
/// the header's key data, matching [`decipher_descriptor`].
#[cfg(test)]
pub(super) fn encrypt_test_descriptor(
    compressed: &[u8],
    key_data: &[u8; super::KEYDATA_SIZE],
) -> Vec<u8> {
    let mut buf = compressed.to_vec();
    while buf.len() % 16 != 0 {
        buf.push(0);
    }
    let cipher = cipher_from(key_data);
    let iv: [u8; 16] = key_data[0..16].try_into().unwrap();
    let mut off = 0;
    while off < buf.len() {
        let bs = (buf.len() - off).min(512);
        cbc_dewhiten_encrypt(&cipher, &mut buf[off..off + bs], &iv);
        off += bs;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbc_dewhiten_roundtrips() {
        let cipher = Aes256::new(GenericArray::from_slice(&[7u8; 32]));
        let iv = [3u8; 16];
        let mut data: Vec<u8> = (0..64).collect();
        let orig = data.clone();
        cbc_dewhiten_encrypt(&cipher, &mut data, &iv);
        assert_ne!(data, orig);
        cbc_dewhiten(&cipher, &mut data, &iv);
        assert_eq!(data, orig);
    }

    #[test]
    fn edc_lut_matches_runtime() {
        // Spot-check the const-built table against a runtime computation.
        let mut lut = [0u32; 256];
        for (i, e) in lut.iter_mut().enumerate() {
            let mut crc = i as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ ((crc & 1) * 0xD801_8001);
            }
            *e = crc;
        }
        assert_eq!(lut, EDC_LUT);
    }

    #[test]
    fn crc32_known_vector() {
        // Standard CRC-32 of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
