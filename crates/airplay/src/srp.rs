use std::sync::OnceLock;

use num_bigint::BigUint;
use sha2::{Digest, Sha512};

pub(crate) const PUBLIC_LEN: usize = 384;
pub(crate) const PROOF_LEN: usize = 64;
pub(crate) const PRIVATE_LEN: usize = 32;
/// Claude: a private value whose public value or premaster secret would not
/// fill all 384 bytes is drawn again, about once in 128 draws.
const DRAWS: usize = 64;

const MODULUS: &[u8] = b"\
    FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
    020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
    4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
    EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05\
    98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB\
    9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
    E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718\
    3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33\
    A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
    ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864\
    D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2\
    08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";
const GENERATOR: u8 = 5;

/// Claude: RFC 5054's 3072-bit group, with the values SRP-6a derives from it.
struct Group {
    n: BigUint,
    g: BigUint,
    k: BigUint,
    hash_n_xor_hash_g: [u8; PROOF_LEN],
}

fn group() -> &'static Group {
    static GROUP: OnceLock<Group> = OnceLock::new();
    GROUP.get_or_init(|| {
        let hex: Vec<u8> = MODULUS.iter().copied().filter(|b| *b != b' ').collect();
        let n = BigUint::parse_bytes(&hex, 16).expect("the modulus is hexadecimal");
        let g = BigUint::from(GENERATOR);
        let k = to_int(sha512(&[&padded(&n.to_bytes_be()), &padded(&[GENERATOR])]));
        let hash_n = sha512(&[&n.to_bytes_be()]);
        let hash_g = sha512(&[&[GENERATOR]]);
        let hash_n_xor_hash_g = std::array::from_fn(|i| hash_n[i] ^ hash_g[i]);
        Group {
            n,
            g,
            k,
            hash_n_xor_hash_g,
        }
    })
}

/// Claude: what a client sends and expects once it has the server's salt and
/// public value.
pub(crate) struct ClientSession {
    pub(crate) public: [u8; PUBLIC_LEN],
    pub(crate) proof: [u8; PROOF_LEN],
    pub(crate) server_proof: [u8; PROOF_LEN],
    pub(crate) key: [u8; PROOF_LEN],
}

/// Claude: the client side of SRP-6a with SHA-512 over the 3072-bit group, as
/// HomeKit pairs: x = H(s | H(I:P)), u = H(PAD(A) | PAD(B)), K = H(S),
/// M1 = H(H(N) xor H(g) | H(I) | s | A | B | K) and M2 = H(A | M1 | K).
/// Implementations differ on stripping leading zero bytes from s, A, B and S;
/// the salt and B are hashed exactly as the server sent them, and `private` is
/// drawn again until A and S fill all 384 bytes, so no stripping can apply.
pub(crate) fn client(
    user: &str,
    password: &str,
    salt: &[u8],
    server_public: &[u8],
    mut private: impl FnMut() -> Result<[u8; PRIVATE_LEN], String>,
) -> Result<ClientSession, String> {
    let group = group();
    let b = BigUint::from_bytes_be(server_public);
    if server_public.len() > PUBLIC_LEN || b.bits() == 0 || b >= group.n {
        return Err("its SRP public value is not in the group".into());
    }
    let x = to_int(sha512(&[
        salt,
        &sha512(&[user.as_bytes(), b":", password.as_bytes()]),
    ]));
    let k_times_verifier = (&group.k * group.g.modpow(&x, &group.n)) % &group.n;
    let base = (&b + &group.n - k_times_verifier) % &group.n;
    let server_padded = padded(server_public);
    for _ in 0..DRAWS {
        let a = BigUint::from_bytes_be(&private()?);
        let Some(public) = full_width(&group.g.modpow(&a, &group.n)) else {
            continue;
        };
        let u = to_int(sha512(&[&public, &server_padded]));
        if u.bits() == 0 {
            continue;
        }
        let Some(premaster) = full_width(&base.modpow(&(a + u * &x), &group.n)) else {
            continue;
        };
        let key = sha512(&[&premaster]);
        let proof = sha512(&[
            &group.hash_n_xor_hash_g,
            &sha512(&[user.as_bytes()]),
            salt,
            &public,
            server_public,
            &key,
        ]);
        let server_proof = sha512(&[&public, &proof, &key]);
        return Ok(ClientSession {
            public,
            proof,
            server_proof,
            key,
        });
    }
    Err(format!("no SRP private value worked in {DRAWS} draws"))
}

fn sha512(parts: &[&[u8]]) -> [u8; PROOF_LEN] {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn to_int(hash: [u8; PROOF_LEN]) -> BigUint {
    BigUint::from_bytes_be(&hash)
}

fn padded(bytes: &[u8]) -> [u8; PUBLIC_LEN] {
    let mut out = [0; PUBLIC_LEN];
    out[PUBLIC_LEN - bytes.len()..].copy_from_slice(bytes);
    out
}

fn full_width(value: &BigUint) -> Option<[u8; PUBLIC_LEN]> {
    let bytes = value.to_bytes_be();
    (bytes.len() == PUBLIC_LEN).then(|| padded(&bytes))
}

/// Claude: the server side, for the fake receiver: it holds the verifier for
/// `user` and `password` and checks a client's proof.
#[cfg(test)]
pub(crate) struct Server {
    user: String,
    salt: Vec<u8>,
    verifier: BigUint,
    private: BigUint,
    pub(crate) public: [u8; PUBLIC_LEN],
}

#[cfg(test)]
impl Server {
    pub(crate) fn new(user: &str, password: &str, salt: &[u8], private: &[u8]) -> Server {
        let group = group();
        let x = to_int(sha512(&[
            salt,
            &sha512(&[user.as_bytes(), b":", password.as_bytes()]),
        ]));
        let verifier = group.g.modpow(&x, &group.n);
        let private = BigUint::from_bytes_be(private);
        let public = (&group.k * &verifier + group.g.modpow(&private, &group.n)) % &group.n;
        Server {
            user: user.to_owned(),
            salt: salt.to_vec(),
            verifier,
            private,
            public: padded(&public.to_bytes_be()),
        }
    }

    /// Claude: the server's proof and the session key, when the client's
    /// proof holds.
    pub(crate) fn verify(
        &self,
        client_public: &[u8],
        proof: &[u8],
    ) -> Option<([u8; PROOF_LEN], [u8; PROOF_LEN])> {
        let group = group();
        let a = BigUint::from_bytes_be(client_public);
        if client_public.len() > PUBLIC_LEN || a.bits() == 0 || a >= group.n {
            return None;
        }
        let u = to_int(sha512(&[&padded(client_public), &self.public]));
        let premaster = (a * self.verifier.modpow(&u, &group.n)).modpow(&self.private, &group.n);
        let key = sha512(&[&padded(&premaster.to_bytes_be())]);
        let expected = sha512(&[
            &group.hash_n_xor_hash_g,
            &sha512(&[self.user.as_bytes()]),
            &self.salt,
            client_public,
            &self.public,
            &key,
        ]);
        (expected[..] == *proof).then(|| (sha512(&[client_public, proof, &key]), key))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Claude: the SRP test vectors of the HomeKit Accessory Protocol
    /// specification, as homebridge/fast-srp's src/test/hap_test_data.ts and
    /// test_hap_vectors.ts carry them (M1 is fast-srp's). M2 is H(A | M1 | K),
    /// computed with `openssl dgst -sha512`.
    pub(crate) mod hap {
        pub(crate) const USER: &str = "alice";
        pub(crate) const PASSWORD: &str = "password123";
        pub(crate) const SALT: &str = "BEB25379 D1A8581E B5A72767 3A2441EE";
        pub(crate) const PRIVATE_A: &str =
            "60975527 035CF2AD 1989806F 0407210B C81EDC04 E2762A56 AFD529DD DA2D4393";
        pub(crate) const PRIVATE_B: &str =
            "E487CB59 D31AC550 471E81F0 0F6928E0 1DDA08E9 74A004F4 9E61F5D1 05284D20";
        pub(crate) const PUBLIC_A: &str = "
            FAB6F5D2 615D1E32 3512E799 1CC37443 F487DA60 4CA8C923 0FCB04E5 41DCE628
            0B27CA46 80B0374F 179DC3BD C7553FE6 2459798C 701AD864 A91390A2 8C93B644
            ADBF9C00 745B942B 79F9012A 21B9B787 82319D83 A1F83628 66FBD6F4 6BFC0DDB
            2E1AB6E4 B45A9906 B82E37F0 5D6F97F6 A3EB6E18 2079759C 4F684783 7B62321A
            C1B4FA68 641FCB4B B98DD697 A0C73641 385F4BAB 25B79358 4CC39FC8 D48D4BD8
            67A9A3C1 0F8EA121 70268E34 FE3BBE6F F89998D6 0DA2F3E4 283CBEC1 393D52AF
            724A5723 0C604E9F BCE583D7 613E6BFF D67596AD 121A8707 EEC46944 95703368
            6A155F64 4D5C5863 B48F61BD BF19A53E AB6DAD0A 186B8C15 2E5F5D8C AD4B0EF8
            AA4EA500 8834C3CD 342E5E0F 167AD045 92CD8BD2 79639398 EF9E114D FAAAB919
            E14E8509 89224DDD 98576D79 385D2210 902E9F9B 1F2D86CF A47EE244 635465F7
            1058421A 0184BE51 DD10CC9D 079E6F16 04E7AA9B 7CF7883C 7D4CE12B 06EBE160
            81E23F27 A231D184 32D7D1BB 55C28AE2 1FFCF005 F57528D1 5A88881B B3BBB7FE";
        pub(crate) const PUBLIC_B: &str = "
            40F57088 A482D4C7 733384FE 0D301FDD CA9080AD 7D4F6FDF 09A01006 C3CB6D56
            2E41639A E8FA21DE 3B5DBA75 85B27558 9BDB2798 63C56280 7B2B9908 3CD1429C
            DBE89E25 BFBD7E3C AD3173B2 E3C5A0B1 74DA6D53 91E6A06E 465F037A 40062548
            39A56BF7 6DA84B1C 94E0AE20 8576156F E5C140A4 BA4FFC9E 38C3B07B 88845FC6
            F7DDDA93 381FE0CA 6084C4CD 2D336E54 51C464CC B6EC65E7 D16E548A 273E8262
            84AF2559 B6264274 215960FF F47BDD63 D3AFF064 D6137AF7 69661C9D 4FEE4738
            2603C88E AA098058 1D077584 61B777E4 356DDA58 35198B51 FEEA308D 70F75450
            B71675C0 8C7D8302 FD7539DD 1FF2A11C B4258AA7 0D234436 AA42B6A0 615F3F91
            5D55CC3B 966B2716 B36E4D1A 06CE5E5D 2EA3BEE5 A1270E87 51DA45B6 0B997B0F
            FDB0F996 2FEE4F03 BEE780BA 0A845B1D 92714217 83AE6601 A61EA2E3 42E4F2E8
            BC935A40 9EAD19F2 21BD1B74 E2964DD1 9FC845F6 0EFC0933 8B60B6B2 56D8CAC8
            89CCA306 CC370A0B 18C8B886 E95DA0AF 5235FEF4 393020D2 B7F30569 04759042";
        pub(crate) const KEY: &str = "
            5CBC219D B052138E E1148C71 CD449896 3D682549 CE91CA24 F098468F 06015BEB
            6AF245C2 093F98C3 651BCA83 AB8CAB2B 580BBF02 184FEFDF 26142F73 DF95AC50";
        pub(crate) const CLIENT_PROOF: &str = "5f7c14ab57ed0e94fd1d78c6b4dd09ed7e340b7e05d419a9fd760f6b35e523d1310777a1ae1d2826f596f3a85116cc457c7c964d4f44ded5559da818c88b617f";
        pub(crate) const SERVER_PROOF: &str = "2fa0e81f5cb73b88fa0964270f321dd641f2227a5d805c40f1bfe96aaf6a19ffce8e23287965a39eab9d5a02215f89e128177ed2c4f103e655a045531bcbf7ad";
    }

    fn private(text: &str) -> [u8; PRIVATE_LEN] {
        hex(text).try_into().unwrap()
    }

    #[test]
    fn the_client_reproduces_the_homekit_test_vectors() {
        let session = client(
            hap::USER,
            hap::PASSWORD,
            &hex(hap::SALT),
            &hex(hap::PUBLIC_B),
            || Ok(private(hap::PRIVATE_A)),
        )
        .unwrap();
        assert_eq!(session.public.to_vec(), hex(hap::PUBLIC_A));
        assert_eq!(session.key.to_vec(), hex(hap::KEY));
        assert_eq!(session.proof.to_vec(), hex(hap::CLIENT_PROOF));
        assert_eq!(session.server_proof.to_vec(), hex(hap::SERVER_PROOF));
    }

    #[test]
    fn the_test_server_reproduces_the_homekit_test_vectors() {
        let server = Server::new(
            hap::USER,
            hap::PASSWORD,
            &hex(hap::SALT),
            &hex(hap::PRIVATE_B),
        );
        assert_eq!(server.public.to_vec(), hex(hap::PUBLIC_B));
        let (server_proof, key) = server
            .verify(&hex(hap::PUBLIC_A), &hex(hap::CLIENT_PROOF))
            .unwrap();
        assert_eq!(key.to_vec(), hex(hap::KEY));
        assert_eq!(server_proof.to_vec(), hex(hap::SERVER_PROOF));
        let mut wrong = hex(hap::CLIENT_PROOF);
        wrong[0] ^= 1;
        assert!(server.verify(&hex(hap::PUBLIC_A), &wrong).is_none());
    }

    /// Claude: private values found with Python's `pow` for the HomeKit
    /// vectors: one whose public value starts with a zero byte, and one whose
    /// premaster secret does.
    const SHORT_PUBLIC: &str = "b0da156ddf01bbf851cfc30206d3902cee061e11af7bde103160d00c3fa8bffe";
    const SHORT_PREMASTER: &str =
        "45b226f33e63548d81c4b29edc8dc372a88401654c07694b1a6e237d7f62db3f";

    #[test]
    fn private_values_leaving_a_leading_zero_byte_are_drawn_again() {
        for short in [SHORT_PUBLIC, SHORT_PREMASTER] {
            let mut draws = vec![private(hap::PRIVATE_A), private(short)];
            let session = client(
                hap::USER,
                hap::PASSWORD,
                &hex(hap::SALT),
                &hex(hap::PUBLIC_B),
                || Ok(draws.pop().unwrap()),
            )
            .unwrap();
            assert!(draws.is_empty(), "{short} was used");
            assert_eq!(session.public.to_vec(), hex(hap::PUBLIC_A));
        }
    }

    /// Claude: a server public value with a leading zero byte, found with
    /// Python's `pow` for the HomeKit verifier, and the proofs that follow
    /// when it arrives as 383 bytes or padded to 384 with the HomeKit `a`.
    const SHORT_B: &str = "
            02ccd9bfcdf3809e83a083effec5f9b0d1e565349d5d5629f76a764d80059d1b
            02c9349699b7ddfbeda3547604f62e9a5415c787faf358f57d6d0acc91f80fc0
            374b65945c769a735f6bbaae45349af17bc4f8337c0c94addad791dda716d71a
            ade4d5145c0cf17289b21b5653976db52bd9cb4178110ba5fecd1d7e6b2397cb
            2241ed4f3250e07b8f9ffc5e52a4754a89024f974038b21eae303501dd041cef
            014973226c51446538ea76bbf52de9ef11cf52ca36ec02d1693cd34d980bdcd8
            a9f5d321c2f8c9638b558c420ba246685e735f1160edc2103df721b40abd0d47
            33fef2591826cbbe6c854490f000068231885e92751ba2b832203c7b8e83647b
            0bfaddc9b917622be1ee53a2443be8adf3ac11b37af44e24d001bf56a13d82f0
            d845108d7227b2d67211166e679e9957972d2a43f7f83159dd41710111a21cd4
            aca740b6e601b2ec446e942f0a053f98cdbcfa7f61aeb9f981485f35742e3d9f
            20a6b8c9e23a818e77642b6d33bb8cf03b9fed0148c14ff97ce04d30c9754a";
    const SHORT_B_KEY: &str = "f7630ab280f8d3d343606d272919464b4e8097226b9956a96f3bbb3cdbf1e349220d75ac0ca085d0964f153e4eb6e8c3bc5eef61f62a06ef1459d86bb8e429e5";
    const SHORT_B_MINIMAL_PROOF: &str = "158e741a6aec56cdcf4f81b889c6c39ef76a99704c1fb79fec47b82c0750db2dd721a6bd1a99386c8f008d490ace2ae698bee2b44ab0ed10b833c2fc76eb31a8";
    const SHORT_B_PADDED_PROOF: &str = "8b0fcf8bebcee30c621c3811f34b1ccedc152a2d9d9f0f3a8a732ede2c2f89227608ff2eb9cf167759f547a9bc44b787ceaeea03ddecb7d1eb375484b256ae2c";

    #[test]
    fn a_short_server_public_value_is_padded_for_u_and_hashed_as_sent_for_the_proof() {
        let short = hex(SHORT_B);
        assert_eq!(short.len(), 383);
        for (sent, proof) in [
            (short.clone(), SHORT_B_MINIMAL_PROOF),
            ([&[0][..], &short].concat(), SHORT_B_PADDED_PROOF),
        ] {
            let session = client(hap::USER, hap::PASSWORD, &hex(hap::SALT), &sent, || {
                Ok(private(hap::PRIVATE_A))
            })
            .unwrap();
            assert_eq!(session.key.to_vec(), hex(SHORT_B_KEY));
            assert_eq!(session.proof.to_vec(), hex(proof));
        }
    }

    #[test]
    fn server_public_values_outside_the_group_are_refused() {
        let modulus: Vec<u8> = group().n.to_bytes_be();
        for public in [
            vec![0u8; 384],
            modulus,
            vec![0xFF; 385],
            [&[0][..], &hex(hap::PUBLIC_B)].concat(),
        ] {
            let refused = client(hap::USER, hap::PASSWORD, &hex(hap::SALT), &public, || {
                Ok(private(hap::PRIVATE_A))
            });
            assert!(refused.is_err());
        }
    }
}
