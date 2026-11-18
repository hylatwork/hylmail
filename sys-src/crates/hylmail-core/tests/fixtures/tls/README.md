# TLS test fixtures

`localhost.crt.pem` and `localhost.key.pem` are a **throwaway self-signed
certificate** used only by the test suite, so that the TLS code paths can be
exercised for real: a handshake that must succeed when the certificate is
configured as an extra trust anchor, and a handshake that must *fail* when it is
not.

* Subject / Issuer: `C=DE, O=HylMail Test Fixtures, CN=localhost`
* Subject alternative names: `DNS:localhost`, `IP:127.0.0.1`
* Validity: 20 years from generation
* Key: RSA 2048, PKCS#8, unencrypted

It was generated with:

```bash
openssl req -x509 -newkey rsa:2048 \
  -keyout localhost.key.pem -out localhost.crt.pem \
  -days 7300 -nodes -sha256 \
  -subj '/C=DE/O=HylMail Test Fixtures/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -addext 'keyUsage=critical,digitalSignature,keyEncipherment' \
  -addext 'extendedKeyUsage=serverAuth'
```

`basicConstraints=CA:FALSE` is load-bearing: the same certificate is both the
server's certificate and the extra trust anchor the test configures, and
`rustls-webpki` rejects a chain whose **end entity** is a CA with
`CaUsedAsEndEntity`.  `openssl req -x509` emits `CA:TRUE` unless told
otherwise, so a fixture generated without that extension fails the three
`net::tests` handshakes with exactly that message.  Regenerate the files with
the full command above rather than by hand.

**Do not trust this certificate.** It is committed on purpose — an unencrypted
private key that is public knowledge — and it only ever appears in tests that
connect to `127.0.0.1`. The shipping application never reads this directory, and
no test uses it outside of `TlsPolicy::with_extra_roots_pem` for a loopback
connection.
