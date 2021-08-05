use gam::FONT_TOTAL_LEN;
use ticktimer_server::Ticktimer;
use utralib::generated::*;
use xous::SOC_STAGING_GW_LEN;
use crate::api::*;
use core::num::NonZeroUsize;
use num_traits::*;

use gam::modal::{Modal, Slider};
use locales::t;

use crate::bcrypt::*;
use crate::api::PasswordType;

use core::convert::TryInto;
use ed25519_dalek::{Keypair, Signature, Signer};
use engine_sha512::*;
use digest::Digest;
use graphics_server::BulkRead;
use core::mem::size_of;

use aes_xous::{Aes256, NewBlockCipher, BlockDecrypt, BlockEncrypt};
use cipher::generic_array::GenericArray;

use root_keys::key2bits::*;

// TODO: add hardware acceleration for BCRYPT so we can hit the OWASP target without excessive UX delay
const BCRYPT_COST: u32 = 7;   // 10 is the minimum recommended by OWASP; takes 5696 ms to verify @ 10 rounds; 804 ms to verify 7 rounds

/// Size of the total area allocated for signatures. It is equal to the size of one FLASH sector, which is the smallest
/// increment that can be erased.
const SIGBLOCK_SIZE: u32 = 0x1000;

#[repr(C)]
struct SignatureInFlash {
    pub version: u32,
    pub signed_len: u32,
    pub signature: [u8; 64],
}

struct KeyRomLocs {}
#[allow(dead_code)]
impl KeyRomLocs {
    const FPGA_KEY:            u8 = 0x00;
    const SELFSIGN_PRIVKEY:    u8 = 0x08;
    const SELFSIGN_PUBKEY:     u8 = 0x10;
    const DEVELOPER_PUBKEY:    u8 = 0x18;
    const THIRDPARTY_PUBKEY:   u8 = 0x20;
    const USER_KEY:   u8 = 0x28;
    const PEPPER:     u8 = 0xf8;
    const FPGA_MIN_REV:   u8 = 0xfc;
    const LOADER_MIN_REV: u8 = 0xfd;
    const CONFIG:     u8 = 0xff;
}

pub struct KeyField {
    mask: u32,
    offset: u32,
}
impl KeyField {
    pub const fn new(width: u32, offset: u32) -> Self {
        let mask = (1 << width) - 1;
        KeyField {
            mask,
            offset,
        }
    }
    pub fn ms(&self, value: u32) -> u32 {
        let ms_le = (value & self.mask) << self.offset;
        ms_le.to_be()
    }
}
#[allow(dead_code)]
pub(crate) mod keyrom_config {
    use crate::KeyField;
    pub const VERSION_MINOR:       KeyField = KeyField::new(8, 0 );
    pub const VERSION_MAJOR:       KeyField = KeyField::new(8, 8 );
    pub const DEVBOOT_DISABLE:     KeyField = KeyField::new(1, 16);
    pub const ANTIROLLBACK_ENA:    KeyField = KeyField::new(1, 17);
    pub const ANTIROLLFORW_ENA:    KeyField = KeyField::new(1, 18);
    pub const FORWARD_REV_LIMIT:   KeyField = KeyField::new(4, 19);
    pub const FORWARD_MINOR_LIMIT: KeyField = KeyField::new(4, 23);
    pub const INITIALIZED:         KeyField = KeyField::new(1, 27);
}

enum RootkeyResult {
    AlignmentError,
    KeyError,
    IntegrityError,
    FlashError,
}

/// This structure is mapped into the password cache page and can be zero-ized at any time
/// we avoid using fancy Rust structures because everything has to "make sense" after a forced zero-ization
/// The "password" here is generated as follows:
///   `user plaintext (up to first 72 bytes) -> bcrypt (24 bytes) -> sha512trunc256 -> [u8; 32]`
/// The final sha512trunc256 expansion is because we will use this to XOR against secret keys stored in
/// the KEYROM that may be up to 256 bits in length. For shorter keys, the hashed password is simply truncated.
    #[repr(C)]
struct PasswordCache {
    hashed_boot_pw: [u8; 32],
    hashed_boot_pw_valid: u32, // non-zero for valid
    hashed_update_pw: [u8; 32],
    hashed_update_pw_valid: u32,
    fpga_key: [u8; 32],
    fpga_key_valid: u32,
}

/// helper routine that will reverse the order of bits. uses a divide-and-conquer approach.
fn bitflip(input: &[u8], output: &mut [u8]) {
    assert!((input.len() % 4 == 0) && (output.len() % 4 == 0) && (input.len() == output.len()));
    for (src, dst) in
    input.chunks(4).into_iter()
    .zip(output.chunks_mut(4).into_iter()) {
        let mut word = u32::from_le_bytes(src.try_into().unwrap()); // read in as LE
        word = ((word >> 1) & 0x5555_5555) | ((word & (0x5555_5555)) << 1);
        word = ((word >> 2) & 0x3333_3333) | ((word & (0x3333_3333)) << 2);
        word = ((word >> 4) & 0x0F0F_0F0F) | ((word & (0x0F0F_0F0F)) << 4);
        // copy out as BE, this performs the final byte-level swap needed by the divde-and-conquer algorithm
        for (&s, d) in word.to_be_bytes().iter().zip(dst.iter_mut()) {
            *d = s;
        }
    }
}

const AES_BLOCKSIZE: usize = 16;
/// This structure encapsulates the tools necessary to create an Oracle that can go from
/// the encrypted bitstream to plaintext and back again, based on the position in the bitstream.
/// It is a partial re-implementation of the Cbc crate from block-ciphers, and the reason we're
/// not just using the stock Cbc crate is that it doesn't seem to support restarting the Cbc
/// stream from an arbitrary position.
struct BitstreamOracle<'a> {
    bitstream: &'a [u8],
    cipher: Aes256,
    iv: [u8; AES_BLOCKSIZE],
    /// subslice of the bitstream that contains just the encrypted area of the bitstream
    ciphertext: &'a [u8],
}
impl<'a> BitstreamOracle<'a> {
    pub fn new(key: &'a[u8], bitstream: &'a[u8]) -> BitstreamOracle<'a> {
        let mut position: usize = 0;

        // Search through the bitstream for the key words that define the IV and ciphertetx length.
        // This is done so that if extra headers are added or modified, the code doesn't break (versus coding in static offsets).
        let mut iv_pos = 0;
        while position < bitstream.len() {
            let cwd = u32::from_be_bytes(bitstream[position..position+4].try_into().unwrap());
            if cwd == 0x3001_6004 {
                iv_pos = position + 4
            }
            if cwd == 0x3003_4001 {
                break;
            }
            position += 1;
        }

        let position = position + 4;
        let ciphertext_len = 4 * u32::from_be_bytes(bitstream[position..position+4].try_into().unwrap());
        let ciphertext_start = position + 4;
        log::info!("ciphertext len: {} bytes, start: 0x{:08x}", ciphertext_len, ciphertext_start);
        let ciphertext = &bitstream[ciphertext_start..ciphertext_start + ciphertext_len as usize];

        let mut iv_bytes: [u8; AES_BLOCKSIZE] = [0; AES_BLOCKSIZE];
        bitflip(&bitstream[iv_pos..iv_pos + AES_BLOCKSIZE], &mut iv_bytes);
        log::info!("recovered iv (pre-flip): {:x?}", &bitstream[iv_pos..iv_pos + AES_BLOCKSIZE]);
        log::info!("recovered iv           : {:x?}", &iv_bytes);

        let cipher = Aes256::new(key.try_into().unwrap());

        BitstreamOracle {
            bitstream,
            cipher,
            iv: iv_bytes,
            ciphertext,
        }
    }
    pub fn clear(&mut self) {
        self.cipher.clear();
    }
    /// decrypts a portion of the bitstream starting at "from", of length output
    pub fn decrypt(&self, from: usize, output: &mut [u8]) {
        assert!(from & (AES_BLOCKSIZE - 1) == 0); // all requests must be an even multiple of an AES block size

        let mut index = from;
        let mut temp_block = [0; AES_BLOCKSIZE];
        let mut chain: [u8; AES_BLOCKSIZE] = [0; AES_BLOCKSIZE];
        for block in output.chunks_mut(AES_BLOCKSIZE).into_iter() {
            if index ==  0 {
                chain = self.iv;
            } else {
                bitflip(&self.ciphertext[index - AES_BLOCKSIZE..index], &mut chain);
            };
            // copy the ciphertext into the temp_block, with a bitflip
            bitflip(&self.ciphertext[index..index + AES_BLOCKSIZE], &mut temp_block);

            // replaces the ciphertext with "plaintext"
            let mut d = GenericArray::clone_from_slice(&mut temp_block);
            self.cipher.decrypt_block(&mut d);
            for (&src, dst) in d.iter().zip(temp_block.iter_mut()) {
                *dst = src;
            }

            // now XOR against the IV into the final output block. We use a "temp" block so we
            // guarantee an even block size for AES, but in fact, the output block does not
            // have to be an even multiple of our block size!
            for (dst, (&src, &iv)) in block.iter_mut().zip(temp_block.iter().zip(chain.iter())) {
                *dst = src ^ iv;
            }
            index += AES_BLOCKSIZE;
        }
    }
    pub fn ciphertext_len(&self) -> usize {
        self.ciphertext.len()
    }
}
impl<'a> Drop for BitstreamOracle<'a> {
    fn drop(&mut self) {
        self.cipher.clear();
        for b in self.iv.iter_mut() {
            *b = 0;
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) struct RootKeys {
    keyrom: utralib::CSR<u32>,
    gateware: xous::MemoryRange,
    staging: xous::MemoryRange,
    loader_code: xous::MemoryRange,
    kernel: xous::MemoryRange,
    /// regions of RAM that holds all plaintext passwords, keys, and temp data. stuck in two well-defined page so we can
    /// zero-ize it upon demand, without guessing about stack frames and/or Rust optimizers removing writes
    sensitive_data: xous::MemoryRange, // this gets purged at least on every suspend, but ideally purged sooner than that
    pass_cache: xous::MemoryRange,  // this can be purged based on a policy, as set below
    boot_password_policy: PasswordRetentionPolicy,
    update_password_policy: PasswordRetentionPolicy,
    cur_password_type: Option<PasswordType>, // for tracking which password we're dealing with at the UX layer
    susres: susres::Susres, // for disabling suspend/resume
    trng: trng::Trng,
    gam: gam::Gam, // for raising UX elements directly
    gfx: graphics_server::Gfx, // for reading out font planes for signing verification
    spinor: spinor::Spinor,
    ticktimer: ticktimer_server::Ticktimer,
}

impl RootKeys {
    pub fn new(xns: &xous_names::XousNames) -> RootKeys {
        let keyrom = xous::syscall::map_memory(
            xous::MemoryAddress::new(utra::keyrom::HW_KEYROM_BASE),
            None,
            4096,
            xous::MemoryFlags::R | xous::MemoryFlags::W,
        )
        .expect("couldn't map keyrom CSR range");
        // read-only memory maps. even if we don't refer to them, we map them into our process
        // so that no other processes can claim them
        let gateware = xous::syscall::map_memory(
            Some(NonZeroUsize::new((xous::SOC_MAIN_GW_LOC + xous::FLASH_PHYS_BASE) as usize).unwrap()),
            None,
            xous::SOC_MAIN_GW_LEN as usize,
            xous::MemoryFlags::R,
        ).expect("couldn't map in the SoC gateware region");
        let staging = xous::syscall::map_memory(
            Some(NonZeroUsize::new((xous::SOC_STAGING_GW_LOC + xous::FLASH_PHYS_BASE) as usize).unwrap()),
            None,
            xous::SOC_STAGING_GW_LEN as usize,
            xous::MemoryFlags::R,
        ).expect("couldn't map in the SoC staging region");
        let loader_code = xous::syscall::map_memory(
            Some(NonZeroUsize::new((xous::LOADER_LOC + xous::FLASH_PHYS_BASE) as usize).unwrap()),
            None,
            xous::LOADER_CODE_LEN as usize,
            xous::MemoryFlags::R,
        ).expect("couldn't map in the loader code region");
        let kernel = xous::syscall::map_memory(
            Some(NonZeroUsize::new((xous::KERNEL_LOC + xous::FLASH_PHYS_BASE) as usize).unwrap()),
            None,
            xous::KERNEL_LEN as usize,
            xous::MemoryFlags::R,
        ).expect("couldn't map in the kernel region");

        let sensitive_data = xous::syscall::map_memory(
            None,
            None,
            0x1000,
            xous::MemoryFlags::R | xous::MemoryFlags::W,
        ).expect("couldn't map sensitive data page");
        let pass_cache = xous::syscall::map_memory(
            None,
            None,
            0x1000,
            xous::MemoryFlags::R | xous::MemoryFlags::W,
        ).expect("couldn't map sensitive data page");

        let spinor = spinor::Spinor::new(&xns).expect("couldn't connect to spinor server");
        spinor.register_soc_token().expect("couldn't register rootkeys as the one authorized writer to the gateware update area!");

        let keys = RootKeys {
            keyrom: CSR::new(keyrom.as_mut_ptr() as *mut u32),
            gateware,
            staging,
            loader_code,
            kernel,
            sensitive_data,
            pass_cache,
            update_password_policy: PasswordRetentionPolicy::AlwaysPurge,
            boot_password_policy: PasswordRetentionPolicy::AlwaysKeep,
            cur_password_type: None,
            susres: susres::Susres::new_without_hook(&xns).expect("couldn't connect to susres without hook"),
            trng: trng::Trng::new(&xns).expect("couldn't connect to TRNG server"),
            gam: gam::Gam::new(&xns).expect("couldn't connect to GAM"),
            gfx: graphics_server::Gfx::new(&xns).expect("couldn't connect to gfx"),
            spinor,
            ticktimer: ticktimer_server::Ticktimer::new().expect("couldn't connect to ticktimer"),
        };

        keys
    }

    fn purge_password(&mut self, pw_type: PasswordType) {
        unsafe {
            let pcache_ptr: *mut PasswordCache = self.pass_cache.as_mut_ptr() as *mut PasswordCache;
            match pw_type {
                PasswordType::Boot => {
                    for p in (*pcache_ptr).hashed_boot_pw.iter_mut() {
                        *p = 0;
                    }
                    (*pcache_ptr).hashed_boot_pw_valid = 0;
                }
                PasswordType::Update => {
                    for p in (*pcache_ptr).hashed_update_pw.iter_mut() {
                        *p = 0;
                    }
                    (*pcache_ptr).hashed_update_pw_valid = 0;

                    for p in (*pcache_ptr).fpga_key.iter_mut() {
                        *p = 0;
                    }
                    (*pcache_ptr).fpga_key_valid = 0;
                }
            }
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    fn purge_sensitive_data(&mut self) {
        let data = self.sensitive_data.as_slice_mut::<u32>();
        for d in data.iter_mut() {
            *d = 0;
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    pub fn suspend(&mut self) {
        match self.boot_password_policy {
            PasswordRetentionPolicy::AlwaysKeep => {
                ()
            },
            _ => {
                self.purge_password(PasswordType::Boot);
            }
        }
        match self.update_password_policy {
            PasswordRetentionPolicy::AlwaysKeep => {
                ()
            },
            _ => {
                self.purge_password(PasswordType::Update);
            }
        }
        self.purge_sensitive_data();
    }
    pub fn resume(&mut self) {
    }

    pub fn update_policy(&mut self, policy: Option<PasswordRetentionPolicy>) {
        let pw_type = if let Some(cur_type) = self.cur_password_type {
            cur_type
        } else {
            log::error!("got an unexpected policy update from the UX");
            return;
        };
        if let Some(p) = policy {
            match pw_type {
                PasswordType::Boot => self.boot_password_policy = p,
                PasswordType::Update => self.update_password_policy = p,
            };
        } else {
            match pw_type {
                PasswordType::Boot => PasswordRetentionPolicy::AlwaysPurge,
                PasswordType::Update => PasswordRetentionPolicy::AlwaysPurge,
            };
        }
        // once the policy has been set, revert the current type to None
        self.cur_password_type = None;
    }

    /// Plaintext password is passed as a &str. Any copies internally are destroyed. Caller is responsible for destroying the &str original.
    /// Performs a bcrypt hash of the password, with the currently set salt; does not store the plaintext after exit.
    pub fn hash_and_save_password(&mut self, pw: &str) {
        let pw_type = if let Some(cur_type) = self.cur_password_type {
            cur_type
        } else {
            log::error!("got an unexpected password from the UX");
            return;
        };
        let mut hashed_password: [u8; 24] = [0; 24];
        let mut salt = self.get_salt();
        // we change the salt ever-so-slightly for every password. This doesn't make any one password more secure;
        // but it disallows guessing all the passwords with a single off-the-shelf hashcat run.
        salt[0] ^= pw_type as u8;

        let timer = ticktimer_server::Ticktimer::new().expect("couldn't connect to ticktimer");
        // the bcrypt function takes the plaintext password and makes one copy to prime the blowfish bcrypt
        // cipher. It is responsible for erasing this state.
        let start_time = timer.elapsed_ms();
        bcrypt(BCRYPT_COST, &salt, pw, &mut hashed_password); // note: this internally makes a copy of the password, and destroys it
        let elapsed = timer.elapsed_ms() - start_time;
        log::info!("bcrypt cost: {} time: {}ms", BCRYPT_COST, elapsed); // benchmark to figure out how to set cost parameter

        // expand the 24-byte (192-bit) bcrypt result into 256 bits, so we can use it directly as XOR key material
        // against 256-bit AES and curve25519 keys
        // for such a small hash, software is the most performant choice
        let mut hasher = engine_sha512::Sha512Trunc256::new(Some(engine_sha512::FallbackStrategy::SoftwareOnly));
        hasher.update(hashed_password);
        let digest = hasher.finalize();

        let pcache_ptr: *mut PasswordCache = self.pass_cache.as_mut_ptr() as *mut PasswordCache;
        unsafe {
            match pw_type {
                PasswordType::Boot => {
                    for (&src, dst) in digest.iter().zip((*pcache_ptr).hashed_boot_pw.iter_mut()) {
                        *dst = src;
                    }
                    (*pcache_ptr).hashed_boot_pw_valid = 1;
                }
                PasswordType::Update => {
                    for (&src, dst) in digest.iter().zip((*pcache_ptr).hashed_update_pw.iter_mut()) {
                        *dst = src;
                    }
                    (*pcache_ptr).hashed_update_pw_valid = 1;
                }
            }
        }
    }

    /// Reads a 256-bit key at a given index offset
    fn read_key_256(&mut self, index: u8) -> [u8; 32] {
        let mut key: [u8; 32] = [0; 32];
        for (addr, word) in key.chunks_mut(4).into_iter().enumerate() {
            self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, index as u32 + addr as u32);
            let keyword = self.keyrom.rf(utra::keyrom::DATA_DATA);
            for (&byte, dst) in keyword.to_be_bytes().iter().zip(word.iter_mut()) {
                *dst = byte;
            }
        }
        key
    }
    /// Reads a 128-bit key at a given index offset
    fn read_key_128(&mut self, index: u8) -> [u8; 16] {
        let mut key: [u8; 16] = [0; 16];
        for (addr, word) in key.chunks_mut(4).into_iter().enumerate() {
            self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, index as u32 + addr as u32);
            let keyword = self.keyrom.rf(utra::keyrom::DATA_DATA);
            for (&byte, dst) in keyword.to_be_bytes().iter().zip(word.iter_mut()) {
                *dst = byte;
            }
        }
        key
    }

    /// Returns the `salt` needed for the `bcrypt` routine.
    /// This routine handles the special-case of being unitialized: in that case, we need to get
    /// salt from a staging area, and not our KEYROM. However, `setup_key_init` must be called
    /// first to ensure that the staging area has a valid salt.
    fn get_salt(&mut self) -> [u8; 16] {
        if !self.is_initialized() {
            // we're not initialized, use the salt that should already be in the staging area
            let sensitive_slice = self.sensitive_data.as_slice::<u32>();
            let mut key: [u8; 16] = [0; 16];
            for (word, &keyword) in key.chunks_mut(4).into_iter()
                                                    .zip(sensitive_slice[KeyRomLocs::PEPPER as usize..KeyRomLocs::PEPPER as usize + 128/(size_of::<u32>()*8)].iter()) {
                for (&byte, dst) in keyword.to_be_bytes().iter().zip(word.iter_mut()) {
                    *dst = byte;
                }
            }
            key
        } else {
            self.read_key_128(KeyRomLocs::PEPPER)
        }
    }

    /// Called by the UX layer to track which password we're currently requesting
    pub fn set_ux_password_type(&mut self, cur_type: Option<PasswordType>) {
        self.cur_password_type = cur_type;
    }
    /// Called by the UX layer to check which password request is in progress
    pub fn get_ux_password_type(&self) -> Option<PasswordType> {self.cur_password_type}

    pub fn is_initialized(&mut self) -> bool {
        self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, KeyRomLocs::CONFIG as u32);
        let config = self.keyrom.rf(utra::keyrom::DATA_DATA);
        if config & keyrom_config::INITIALIZED.ms(1) != 0 {
            true
        } else {
            false
        }
    }

    /// Called by the UX layer to set up a key init run. It disables suspend/resume for the duration
    /// of the run, and also sets up some missing fields of KEYROM necessary to encrypt passwords.
    pub fn setup_key_init(&mut self) {
        // block suspend/resume ops during security-sensitive operations
        self.susres.set_suspendable(false).expect("couldn't block suspend/resume");
        // in this block, keyrom data is copied into RAM.
        // make a copy of the KEYROM to hold the new mods, in the sensitive data area
        let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
        for addr in 0..256 {
            self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, addr);
            sensitive_slice[addr as usize] = self.keyrom.rf(utra::keyrom::DATA_DATA);
        }

        // provision the pepper
        for keyword in sensitive_slice[KeyRomLocs::PEPPER as usize..KeyRomLocs::PEPPER as usize + 128/(size_of::<u32>()*8)].iter_mut() {
            *keyword = self.trng.get_u32().expect("couldn't get random number");
        }
    }

    /// Core of the key initialization routine. Requires a `progress_modal` dialog box that has been set
    /// up with the appropriate notification messages by the UX layer, and a `Slider` type action which
    /// is used to report the progress of the initialization routine. We assume the `Slider` box is set
    /// up to report progress on a range of 0-100%.
    ///
    /// This routine dispatches the following activities:
    /// - generate signing private key (encrypted with update password)
    /// - generate rootkey (encrypted with boot password)
    /// - generate signing public key
    /// - set the init bit
    /// - sign the loader
    /// - sign the kernel
    /// - compute the patch set for the FPGA bitstream
    /// - do the patch (whatever that means - gotta deal with the AES key, HMAC etc.)
    /// - verify the FPGA image hmac
    /// - sign the FPGA image
    /// - get ready for a reboot
    ///   - returns true if we should reboot (everything succeeded)
    ///   - returns false if we had an error condition (don't reboot)
    pub fn do_key_init(&mut self, progress_modal: &mut Modal, progress_action: &mut Slider) -> bool {
        // kick the progress bar to indicate we've entered the routine
        update_progress(1, progress_modal, progress_action);

        let keypair: Keypair = Keypair::generate(&mut self.trng);
        // pub key is easy, no need to encrypt
        let public_key: [u8; ed25519_dalek::PUBLIC_KEY_LENGTH] = keypair.public.to_bytes();
        { // scope sensitive_slice narrowly, as it borrows *self mutably, and can mess up later calls that borrow an immutable self
            // sensitive_slice is our staging area for the new keyrom contents
            let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
            for (src, dst) in public_key.chunks(4).into_iter()
            .zip(sensitive_slice[KeyRomLocs::SELFSIGN_PUBKEY as usize..KeyRomLocs::SELFSIGN_PUBKEY as usize + 256/(size_of::<u32>()*8)].iter_mut()) {
                *dst = u32::from_be_bytes(src.try_into().unwrap())
            }
        }

        // extract the update password key from the cache, and apply it to the private key
        let pcache: &mut PasswordCache = unsafe{&mut *(self.pass_cache.as_mut_ptr() as *mut PasswordCache)};
        #[cfg(feature = "hazardous-debug")]
        {
            log::info!("cached boot passwords {:x?}", pcache.hashed_boot_pw);
            log::info!("cached update password: {:x?}", pcache.hashed_update_pw);
        }
        // private key must XOR with password before storing
        let mut private_key_enc: [u8; ed25519_dalek::SECRET_KEY_LENGTH] = [0; ed25519_dalek::SECRET_KEY_LENGTH];
        // we do this from to try and avoid making as few copies of the hashed password as possible
        for (dst, (plain, key)) in
        private_key_enc.iter_mut()
        .zip(keypair.secret.to_bytes().iter()
        .zip(pcache.hashed_update_pw.iter())) {
            *dst = plain ^ key;
        }

        // store the private key to the keyrom staging area
        {
            let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
            for (src, dst) in private_key_enc.chunks(4).into_iter()
            .zip(sensitive_slice[KeyRomLocs::SELFSIGN_PRIVKEY as usize..KeyRomLocs::SELFSIGN_PRIVKEY as usize + 256/(size_of::<u32>()*8)].iter_mut()) {
                *dst = u32::from_be_bytes(src.try_into().unwrap())
            }
        }

        update_progress(5, progress_modal, progress_action);

        // generate and store a root key (aka boot key), this is what is unlocked by the "boot password"
        // ironically, it's a "lower security" key because it just acts as a gatekeeper to further
        // keys that would have a stronger password applied to them, based upon the importance of the secret
        // think of this more as a user PIN login confirmation, than as a significant cryptographic event
        let mut boot_key_enc: [u8; 32] = [0; 32];
        for (dst, key) in
        boot_key_enc.chunks_mut(4).into_iter()
        .zip(pcache.hashed_boot_pw.chunks(4).into_iter()) {
            let key_word = self.trng.get_u32().unwrap().to_be_bytes();
            // just unroll this loop, it's fast and easy enough
            (*dst)[0] = key[0] ^ key_word[0];
            (*dst)[1] = key[1] ^ key_word[1];
            (*dst)[2] = key[2] ^ key_word[2];
            (*dst)[3] = key[3] ^ key_word[3];
            // also note that interestingly, we don't have to XOR it with the hashed boot password --
            // this key isn't used by this routine, just initialized, so really, it only matters to
            // XOR it with the password when you use it the first time to encrypt something.
        }

        // store the boot key to the keyrom staging area
        {
            let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
            for (src, dst) in private_key_enc.chunks(4).into_iter()
            .zip(sensitive_slice[KeyRomLocs::USER_KEY as usize..KeyRomLocs::USER_KEY as usize + 256/(size_of::<u32>()*8)].iter_mut()) {
                *dst = u32::from_be_bytes(src.try_into().unwrap())
            }
        }

        // sign the loader
        progress_modal.modify(None, Some(t!("rootkeys.init.signing_loader", xous::LANG)), false, None, false, None);
        update_progress(10, progress_modal, progress_action);
        let (loader_sig, loader_len) = self.sign_loader(&keypair);

        // sign the kernel
        progress_modal.modify(None, Some(t!("rootkeys.init.signing_kernel", xous::LANG)), false, None, false, None);
        update_progress(20, progress_modal, progress_action);
        let (kernel_sig, kernel_len) = self.sign_kernel(&keypair);

        update_progress(25, progress_modal, progress_action);
        // encrypt the FPGA key using the update password. in an un-init system, it is provided to us in plaintext format
        // e.g. in the case that we're doing a BBRAM boot (eFuse flow would give us a 0's key and we'd later on set it)
        #[cfg(feature = "hazardous-debug")]
        self.debug_print_key(KeyRomLocs::FPGA_KEY as usize, 256, "FPGA key before encryption: ");
        {
            let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
            // before we encrypt it, stash a copy in our password cache, as we'll need it later on to encrypt the bitstream
            for (word, key) in sensitive_slice[KeyRomLocs::FPGA_KEY as usize..KeyRomLocs::FPGA_KEY as usize + 256/(size_of::<u32>()*8)].iter()
            .zip(pcache.fpga_key.chunks_mut(4).into_iter()) {
                for (&s, d) in word.to_be_bytes().iter().zip(key.iter_mut()) {
                    *d = s;
                }
            }
            pcache.fpga_key_valid = 1;

            // now encrypt it in the staging area
            for (word, key_word) in sensitive_slice[KeyRomLocs::FPGA_KEY as usize..KeyRomLocs::FPGA_KEY as usize + 256/(size_of::<u32>()*8)].iter_mut()
            .zip(pcache.hashed_update_pw.chunks(4).into_iter()) {
                *word = *word ^ u32::from_be_bytes(key_word.try_into().unwrap());
            }
        }
        update_progress(30, progress_modal, progress_action);

        // set the "init" bit in the staging area
        {
            let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
            self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, KeyRomLocs::CONFIG as u32);
            let config = self.keyrom.rf(utra::keyrom::DATA_DATA);
            sensitive_slice[KeyRomLocs::CONFIG as usize] |= keyrom_config::INITIALIZED.ms(1);
        }

        #[cfg(feature = "hazardous-debug")]
        {
            log::info!("Self private key: {:x?}", keypair.secret.to_bytes());
            log::info!("Self public key: {:x?}", keypair.public.to_bytes());
            self.debug_staging();
        }

        // Because we're initializing keys for the *first* time, make a backup copy of the bitstream to
        // the staging area. Note that if we're doing an update, the update target would already be
        // in the staging area, so this step should be skipped.
        progress_modal.modify(None, Some(t!("rootkeys.init.backup_gateware", xous::LANG)), false, None, false, None);
        update_progress(40, progress_modal, progress_action);
        if self.make_gateware_backup(40, 60, progress_modal, progress_action).is_err() {
            log::error!("error occured in make_gateware_backup.");
            return false;
        };

        // compute the keyrom patch set for the bitstream
        // at this point the KEYROM as replicated in sensitive_slice should have all its assets in place
        progress_modal.modify(None, Some(t!("rootkeys.init.patching_keys", xous::LANG)), false, None, false, None);
        update_progress(60, progress_modal, progress_action);

        let gateware = self.gateware.as_slice::<u8>();
        assert!(pcache.fpga_key_valid == 1);
        // note to self: BitstreamOracle implements Drop which will clear the key schedule and iv
        let oracle = BitstreamOracle::new(&pcache.fpga_key, gateware);

        if self.patch_keys(&oracle, 60, 70, progress_modal, progress_action).is_err() {
            log::error!("error occured in patch_keys.");
            return false;
        }
        patch::should_patch(42);

        // verify that the patch worked
        progress_modal.modify(None, Some(t!("rootkeys.init.verifying_gateware", xous::LANG)), false, None, false, None);
        update_progress(90, progress_modal, progress_action);

        // --> put the verify call here

        // sign the image, commit the signature
        // --> add something to sign the gateware region here
        // --> format the signatures and write them to FLASH

        // finalize the progress bar on exit -- always leave at 100%
        progress_modal.modify(None, Some(t!("rootkeys.init.finished", xous::LANG)), false, None, false, None);
        update_progress(100, progress_modal, progress_action);

        true
    }

    pub fn test(&mut self, progress_modal: &mut Modal, progress_action: &mut Slider) -> bool {
        let pcache: &mut PasswordCache = unsafe{&mut *(self.pass_cache.as_mut_ptr() as *mut PasswordCache)};

        // setup the local cache
        let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
        for addr in 0..256 {
            self.keyrom.wfo(utra::keyrom::ADDRESS_ADDRESS, addr);
            sensitive_slice[addr as usize] = self.keyrom.rf(utra::keyrom::DATA_DATA);
        }

        for (word, key) in sensitive_slice[KeyRomLocs::FPGA_KEY as usize..KeyRomLocs::FPGA_KEY as usize + 256/(size_of::<u32>()*8)].iter()
        .zip(pcache.fpga_key.chunks_mut(4).into_iter()) {
            for (&s, d) in word.to_be_bytes().iter().zip(key.iter_mut()) {
                *d = s;
            }
        }
        pcache.fpga_key_valid = 1;

        // make the oracle
        let gateware = self.gateware.as_slice::<u8>();
        assert!(pcache.fpga_key_valid == 1);
        let oracle = BitstreamOracle::new(&pcache.fpga_key, gateware);

        // TEST
        if self.patch_keys(&oracle, 60, 80, progress_modal, progress_action).is_err() {
            log::error!("error occured in patch_keys.");
            return false;
        }

        if self.verify_gateware(&oracle, 80, 100, progress_modal, progress_action).is_err() {
            log::error!("error occurred in gateware verification");
            return false;
        }
        // finalize the progress bar on exit -- always leave at 100%
        progress_modal.modify(None, Some(t!("rootkeys.init.finished", xous::LANG)), false, None, false, None);
        update_progress(100, progress_modal, progress_action);
        true
    }

    fn patch_keys(&self, oracle: &BitstreamOracle, prog_start: u32, prog_end: u32, progress_modal: &mut Modal, progress_action: &mut Slider) -> Result<(), RootkeyResult> {
        let mut last_prog_update = 0;

        // extract the HMAC header
        let mut hmac_header: [u8; 64] = [0; 64];
        oracle.decrypt(0, &mut hmac_header);

        // check that the header has a well-known sequence in it -- sanity check the AES key
        let mut pass = true;
        for &b in hmac_header[32..].iter() {
            if b != 0x6C {
                pass = false;
            }
        }
        if !pass {
            log::error!("hmac_header did not decrypt correctly: {:x?}", hmac_header);
            return Err(RootkeyResult::KeyError);
        }

        // patch the keyrom frames
        for frame in patch::PATCH_FRAMES.iter() {

        }

        // now seek to the various frames and patch their data...
        Ok(())
    }

    fn verify_gateware(&self, oracle: &BitstreamOracle, prog_start: u32, prog_end: u32, progress_modal: &mut Modal, progress_action: &mut Slider) -> Result<(), RootkeyResult> {
        let mut last_prog_update = 0;

        let mut hmac_area = [0; 64];
        oracle.decrypt(0, &mut hmac_area);
        let mut hmac_code: [u8; 32] = [0; 32];
        for (dst, (&hm1, &mask)) in
        hmac_code.iter_mut()
        .zip(hmac_area[0..32].iter().zip(hmac_area[32..64].iter())) {
            *dst = hm1 ^ mask;
        }
        log::trace!("hmac code: {:x?}", hmac_code);

        log::debug!("verifying gateware");
        let mut hasher = engine_sha512::Sha256::new();
        // magic number alert:
        // 160 = reserved space for the 2nd hash
        // 320 = some padding that is built into the file format, for whatever reason xilinx picked
        let tot_len = oracle.ciphertext_len() - 320 - 160;
        // slow but steady. we can optimize later.
        // for now, very tiny optimization - use blocksize * 2 because that happens to be divisible by the bitstream parameters
        let mut decrypt = [0; AES_BLOCKSIZE*2];
        let mut flipped = [0; AES_BLOCKSIZE*2];
        for index in (0..tot_len).step_by(AES_BLOCKSIZE*2) {
            oracle.decrypt(index, &mut decrypt);
            bitflip(&decrypt, &mut flipped);
            hasher.update(&flipped);

            let progress = ((prog_end - prog_start) * index as u32) / tot_len as u32;
            if progress != last_prog_update {
                log::debug!("progress: {}", progress);
                last_prog_update = progress;
                update_progress(progress + prog_start, progress_modal, progress_action);
            }
        }
        let h1_digest: [u8; 32] = hasher.finalize().try_into().unwrap();
        log::trace!("computed hash: {:x?}", h1_digest);

        let mut hasher2 = engine_sha512::Sha256::new();
        let footer_mask: [u8; 32] = [0x3A; 32];
        let mut masked_footer: [u8; 32] = [0; 32];
        for (dst, (&hm2, &mask)) in
        masked_footer.iter_mut()
        .zip(hmac_code.iter().zip(footer_mask.iter())) {
            *dst = hm2 ^ mask;
        }
        log::debug!("masked_footer: {:x?}", masked_footer);
        log::debug!("footer_mask: {:x?}", footer_mask);
        let mut masked_footer_flipped: [u8; 32] = [0; 32];
        bitflip(&masked_footer, &mut masked_footer_flipped);
        let mut footer_mask_flipped: [u8; 32] = [0; 32];
        bitflip(&footer_mask, &mut footer_mask_flipped);
        hasher2.update(masked_footer_flipped);
        hasher2.update(footer_mask_flipped);
        hasher2.update(h1_digest);
        let h2_digest: [u8; 32] = hasher2.finalize().try_into().unwrap();

        log::debug!("h2 hash: {:x?}", h2_digest);
        let mut ref_digest_flipped: [u8; 32] = [0; 32];
        oracle.decrypt(oracle.ciphertext_len() - 32, &mut ref_digest_flipped);
        let mut ref_digest: [u8; 32] = [0; 32];
        log::debug!("ref digest (flipped): {:x?}", ref_digest_flipped);
        bitflip(&ref_digest_flipped, &mut ref_digest);
        log::debug!("ref digest          : {:x?}", ref_digest);

        let mut matching = true;
        for (&l, &r) in ref_digest.iter().zip(h2_digest.iter()) {
            if l != r {
                matching = false;
            }
        }

        if matching {
            Ok(())
        } else {
            Err(RootkeyResult::IntegrityError)
        }
    }


    fn make_gateware_backup(&mut self, prog_start: u32, prog_end: u32, progress_modal: &mut Modal, progress_action: &mut Slider) -> Result<(), RootkeyResult> {
        let gateware_dest = self.staging.as_slice::<u8>();
        let gateware_src = self.gateware.as_slice::<u8>();

        log::trace!("src: {:x?}", &gateware_src[0..32]);
        log::trace!("dst: {:x?}", &gateware_dest[0..32]);

        let mut last_prog_update = 0;
        const PATCH_CHUNK: usize = 65536; // this controls the granularity of the erase operation
        let mut dest_patch_addr = xous::SOC_STAGING_GW_LOC;
        for (dst, src) in
        gateware_dest.chunks(PATCH_CHUNK).into_iter()
        .zip(gateware_src.chunks(PATCH_CHUNK)) {
            log::debug!("writing {} backup bytes to address 0x{:08x}", src.len(), dest_patch_addr);
            self.spinor.patch(dst, dest_patch_addr, src, 0)
                .map_err(|_| RootkeyResult::FlashError)?;
            dest_patch_addr += PATCH_CHUNK as u32;

            let progress = ((prog_end - prog_start) * (dest_patch_addr - xous::SOC_STAGING_GW_LOC)) / xous::SOC_STAGING_GW_LEN;
            if progress != last_prog_update {
                last_prog_update = progress;
                update_progress(progress + prog_start, progress_modal, progress_action);
            }
        }

        log::trace!("src: {:x?}", &gateware_src[0..32]);
        log::trace!("dst: {:x?}", &gateware_dest[0..32]);
        Ok(())
    }

    #[cfg(feature = "hazardous-debug")]
    fn debug_staging(&self) {
        self.debug_print_key(KeyRomLocs::FPGA_KEY as usize, 256, "FPGA key: ");
        self.debug_print_key(KeyRomLocs::SELFSIGN_PRIVKEY as usize, 256, "Self private key: ");
        self.debug_print_key(KeyRomLocs::SELFSIGN_PUBKEY as usize, 256, "Self public key: ");
        self.debug_print_key(KeyRomLocs::DEVELOPER_PUBKEY as usize, 256, "Dev public key: ");
        self.debug_print_key(KeyRomLocs::THIRDPARTY_PUBKEY as usize, 256, "3rd party public key: ");
        self.debug_print_key(KeyRomLocs::USER_KEY as usize, 256, "Boot key: ");
        self.debug_print_key(KeyRomLocs::PEPPER as usize, 128, "Pepper: ");
        self.debug_print_key(KeyRomLocs::CONFIG as usize, 32, "Config (as BE): ");
    }

    #[cfg(feature = "hazardous-debug")]
    fn debug_print_key(&self, offset: usize, num_bits: usize, name: &str) {
        use core::fmt::Write;
        let mut debugstr = xous_ipc::String::<4096>::new();
        let sensitive_slice = self.sensitive_data.as_slice_mut::<u32>();
        write!(debugstr, "{}", name).unwrap();
        for word in sensitive_slice[offset .. offset as usize + num_bits/(size_of::<u32>()*8)].iter() {
            for byte in word.to_be_bytes().iter() {
                write!(debugstr, "{:02x}", byte).unwrap();
            }
        }
        log::info!("{}", debugstr);
    }

    pub fn sign_loader(&mut self, signing_key: &Keypair) -> (Signature, u32) {
        let loader_len =
            xous::LOADER_CODE_LEN
            - SIGBLOCK_SIZE
            + graphics_server::fontmap::FONT_TOTAL_LEN as u32
            + 8; // two u32 words are appended to the end, which repeat the "version" and "length" fields encoded in the signature block

        // this is a huge hash, so, get a hardware hasher, even if it means waiting for it
        let mut hasher = engine_sha512::Sha512::new(Some(engine_sha512::FallbackStrategy::WaitForHardware));
        let loader_region = self.loader_code.as_slice::<u8>();
        // the loader data starts one page in; the first page is reserved for the signature itself
        hasher.update(&loader_region[SIGBLOCK_SIZE as usize..]);

        // now get the font plane data
        self.gfx.bulk_read_restart(); // reset the bulk read pointers on the gfx side
        let mut bulkread = BulkRead::default();
        let mut buf = xous_ipc::Buffer::into_buf(bulkread).expect("couldn't transform bulkread into aligned buffer");
        // this form of loop was chosen to avoid the multiple re-initializations and copies that would be entailed
        // in our usual idiom for pasing buffers around. instead, we create a single buffer, and re-use it for
        // every iteration of the loop.
        loop {
            buf.lend_mut(self.gfx.conn(), self.gfx.bulk_read_fontmap_op()).expect("couldn't do bulkread from gfx");
            let br = buf.as_flat::<BulkRead, _>().unwrap();
            hasher.update(&br.buf[..br.len as usize]);
            if br.len != bulkread.buf.len() as u32 {
                log::trace!("non-full block len: {}", br.len);
            }
            if br.len < bulkread.buf.len() as u32 {
                // read until we get a buffer that's not fully filled
                break;
            }
        }
        if false { // this path is for debugging the loader hash. It spoils the loader signature in the process.
            let digest = hasher.finalize();
            log::info!("len: {}", loader_len);
            log::info!("{:x?}", digest);
            // fake hasher for now
            let mut hasher = engine_sha512::Sha512::new(Some(engine_sha512::FallbackStrategy::WaitForHardware));
            hasher.update(&loader_region[SIGBLOCK_SIZE as usize..]);
            (signing_key.sign_prehashed(hasher, None).expect("couldn't sign the loader"), loader_len)
        } else {
            (signing_key.sign_prehashed(hasher, None).expect("couldn't sign the loader"), loader_len)
        }
    }

    pub fn sign_kernel(&mut self, signing_key: &Keypair) -> (Signature, u32) {
        let mut hasher = engine_sha512::Sha512::new(Some(engine_sha512::FallbackStrategy::WaitForHardware));
        let kernel_region = self.kernel.as_slice::<u8>();
        // for the kernel length, we can't know/trust the given length in the signature field, so we sign the entire
        // length of the region. This will increase the time it takes to verify; however, at the current trend, we'll probably
        // use most of the available space for the kernel, so by the time we're done maybe only 10-20% of the space is empty.
        let kernel_len = kernel_region.len() - SIGBLOCK_SIZE as usize;
        hasher.update(&kernel_region[SIGBLOCK_SIZE as usize ..]);

        (signing_key.sign_prehashed(hasher, None).expect("couldn't sign the kernel"), kernel_len as u32)
    }

    /// Called by the UX layer at the epilogue of the initialization run. Allows suspend/resume to resume,
    /// and zero-izes any sensitive data that was created in the process.
    pub fn finish_key_init(&mut self) {
        // purge the password cache, if the policy calls for it
        match self.boot_password_policy {
            PasswordRetentionPolicy::AlwaysPurge => {
                self.purge_password(PasswordType::Boot);
            },
            _ => ()
        }
        match self.update_password_policy {
            PasswordRetentionPolicy::AlwaysPurge => {
                self.purge_password(PasswordType::Update);
            },
            _ => ()
        }

        // now purge the keyrom copy and other temporaries
        self.purge_sensitive_data();

        // re-allow suspend/resume ops
        self.susres.set_suspendable(true).expect("couldn't re-allow suspend/resume");
    }
}

fn update_progress(new_state: u32, progress_modal: &mut Modal, progress_action: &mut Slider) {
    log::info!("progress: {}", new_state);
    progress_action.set_state(new_state);
    progress_modal.modify(
        Some(gam::modal::ActionType::Slider(*progress_action)),
        None, false, None, false, None);
    progress_modal.redraw(); // stage the modal box pixels to the back buffer
    progress_modal.gam.redraw().expect("couldn't cause back buffer to be sent to the screen");
    xous::yield_slice(); // this gives time for the GAM to do the sending
}
