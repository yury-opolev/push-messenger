// Until the authenticator uses it (Task 2 of the agent-key plan), only tests call the verifier.
#[cfg_attr(not(test), allow(dead_code))]
mod jws;

#[cfg(test)]
pub(crate) mod testing {
    //! A P-256 signing key for tests and the assertions it makes.

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::{Value, json};

    pub(crate) struct TestKey {
        pair: EcdsaKeyPair,
    }

    impl TestKey {
        pub(crate) fn generate() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { pair }
        }

        /// The public JWK `{kty, crv, x, y}`.
        pub(crate) fn jwk(&self) -> Value {
            let point = self.pair.public_key().as_ref();
            json!({"kty": "EC", "crv": "P-256",
                "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&point[33..65])})
        }

        pub(crate) fn thumbprint(&self) -> String {
            let point = self.pair.public_key().as_ref();
            super::jws::thumbprint(&point[1..33], &point[33..65])
        }

        /// A valid assertion for `audience`, issued now, valid for two minutes.
        // The integration tests of Task 2 of the agent-key plan use it.
        #[allow(dead_code)]
        pub(crate) fn assertion(&self, audience: &str) -> String {
            let now = crate::service::unix_now();
            self.sign(
                &self.header(),
                &json!({"aud": audience, "iat": now, "exp": now + 120,
                    "jti": uuid::Uuid::new_v4().simple().to_string()}),
            )
        }

        /// The standard header: ES256, push-agent+jwt and this key's JWK.
        pub(crate) fn header(&self) -> Value {
            json!({"alg": "ES256", "typ": "push-agent+jwt", "jwk": self.jwk()})
        }

        /// `header` and `claims` as JSON, signed with this key.
        pub(crate) fn sign(&self, header: &Value, claims: &Value) -> String {
            self.sign_raw(&header.to_string(), &claims.to_string())
        }

        /// Raw header and claims texts (for duplicate members), signed with this key.
        pub(crate) fn sign_raw(&self, header: &str, claims: &str) -> String {
            let signed = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header),
                URL_SAFE_NO_PAD.encode(claims)
            );
            let signature = self
                .pair
                .sign(&SystemRandom::new(), signed.as_bytes())
                .unwrap();
            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
        }
    }
}
